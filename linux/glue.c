// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0
/*
 * C helpers for the CarryFS Rust driver: the fs_context mount API, buffer
 * heads, and static inline VFS functions that the kernel's Rust bindings do
 * not cover. Each stands in for a helper that in-tree Rust VFS abstractions
 * would provide; no filesystem logic lives here.
 */

#include <linux/blkdev.h>
#include <linux/buffer_head.h>
#include <linux/crc32.h>
#include <linux/cred.h>
#include <linux/fs.h>
#include <linux/fs_context.h>
#include <linux/fs_parser.h>
#include <linux/highmem.h>
#include <linux/iversion.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/namei.h>
#include <linux/pagemap.h>
#include <linux/sched/mm.h>
#include <linux/slab.h>
#include <linux/statfs.h>
#include <linux/version.h>
#include <linux/workqueue.h>

#include "glue.h"

struct carryfs_mount_options {
	kuid_t uid;
	kgid_t gid;
};

enum { Opt_uid, Opt_gid };

static const struct fs_parameter_spec carryfs_param_spec[] = {
	fsparam_uid("uid", Opt_uid),
	fsparam_gid("gid", Opt_gid),
	{}
};

static int carryfs_parse_param(struct fs_context *fc, struct fs_parameter *param)
{
	struct carryfs_mount_options *opts = fc->fs_private;
	struct fs_parse_result result;
	int opt;

	opt = fs_parse(fc, carryfs_param_spec, param, &result);
	if (opt < 0)
		return opt;
	if (opt == Opt_uid)
		opts->uid = result.uid;
	else
		opts->gid = result.gid;
	return 0;
}

static int carryfs_fill_super_fc(struct super_block *sb, struct fs_context *fc)
{
	struct carryfs_mount_options *opts = fc->fs_private;

	return carryfs_fill_super(sb, __kuid_val(opts->uid), __kgid_val(opts->gid),
				  fc->sb_flags & SB_SILENT);
}

static int carryfs_get_tree(struct fs_context *fc)
{
	return get_tree_bdev(fc, carryfs_fill_super_fc);
}

static int carryfs_reconfigure(struct fs_context *fc)
{
	/* Switching between read-only and read-write needs a remount. */
	if ((fc->sb_flags ^ fc->root->d_sb->s_flags) & SB_RDONLY)
		return -EBUSY;
	return 0;
}

static void carryfs_free_fc(struct fs_context *fc)
{
	kfree(fc->fs_private);
}

static const struct fs_context_operations carryfs_context_ops = {
	.parse_param	= carryfs_parse_param,
	.get_tree	= carryfs_get_tree,
	.reconfigure	= carryfs_reconfigure,
	.free		= carryfs_free_fc,
};

static int carryfs_init_fs_context(struct fs_context *fc)
{
	struct carryfs_mount_options *opts;

	opts = kzalloc(sizeof(*opts), GFP_KERNEL);
	if (!opts)
		return -ENOMEM;
	/* Like vfat and exfat: the files belong to whoever mounts the volume. */
	opts->uid = current_fsuid();
	opts->gid = current_fsgid();
	fc->fs_private = opts;
	fc->ops = &carryfs_context_ops;
	return 0;
}

static void carryfs_kill_sb(struct super_block *sb)
{
	void *volume;

	carryfs_stop_timer(sb);
	volume = sb->s_fs_info;
	kill_block_super(sb);
	carryfs_free_volume(volume);
}

/* Commit open transactions every few seconds, as ext4's default commit=5. */
#define CARRYFS_COMMIT_INTERVAL (5 * HZ)

struct carryfs_timer {
	struct delayed_work work;
	struct super_block *sb;
};

static void carryfs_timer_fn(struct work_struct *work)
{
	struct carryfs_timer *timer =
		container_of(to_delayed_work(work), struct carryfs_timer, work);

	carryfs_periodic_commit(timer->sb);
	schedule_delayed_work(&timer->work, CARRYFS_COMMIT_INTERVAL);
}

void *carryfs_timer_start(struct super_block *sb)
{
	struct carryfs_timer *timer = kzalloc(sizeof(*timer), GFP_KERNEL);

	if (!timer)
		return NULL;
	timer->sb = sb;
	INIT_DELAYED_WORK(&timer->work, carryfs_timer_fn);
	schedule_delayed_work(&timer->work, CARRYFS_COMMIT_INTERVAL);
	return timer;
}

void carryfs_timer_stop(void *timer)
{
	struct carryfs_timer *t = timer;

	if (!t)
		return;
	disable_delayed_work_sync(&t->work);
	kfree(t);
}

/* Inodes closed after writing whose writeback starts in one batch. */
#define CARRYFS_FLUSH_BATCH 64

/*
 * A mount's background work: reading the catalog ahead after mounting, as
 * the FUSE adapter does and as ext4 prefetches block bitmaps, so that
 * first listings find it cached; and starting writeback of files closed
 * after writing.
 */
struct carryfs_background {
	struct super_block *sb;
	struct work_struct prefetch;
	struct work_struct flush;
	spinlock_t lock;
	bool stopped;
	unsigned int count;
	struct inode *inodes[CARRYFS_FLUSH_BATCH];
};

static void carryfs_prefetch_fn(struct work_struct *work)
{
	struct carryfs_background *bg =
		container_of(work, struct carryfs_background, prefetch);

	carryfs_prefetch_catalog(bg->sb);
}

/* Start writeback of the queued inodes under one plug, so their writes merge. */
static void carryfs_flush_fn(struct work_struct *work)
{
	struct carryfs_background *bg =
		container_of(work, struct carryfs_background, flush);
	struct inode *inodes[CARRYFS_FLUSH_BATCH];
	struct blk_plug plug;
	unsigned int count, i;

	for (;;) {
		spin_lock(&bg->lock);
		count = bg->count;
		memcpy(inodes, bg->inodes, count * sizeof(*inodes));
		bg->count = 0;
		spin_unlock(&bg->lock);
		if (!count)
			return;
		blk_start_plug(&plug);
		/* Errors stay on the mapping for a later fsync. */
		for (i = 0; i < count; i++)
			filemap_flush(inodes[i]->i_mapping);
		blk_finish_plug(&plug);
		for (i = 0; i < count; i++)
			iput(inodes[i]);
	}
}

void *carryfs_background_new(struct super_block *sb)
{
	struct carryfs_background *bg = kzalloc(sizeof(*bg), GFP_KERNEL);

	if (!bg)
		return NULL;
	bg->sb = sb;
	INIT_WORK(&bg->prefetch, carryfs_prefetch_fn);
	INIT_WORK(&bg->flush, carryfs_flush_fn);
	spin_lock_init(&bg->lock);
	return bg;
}

void carryfs_prefetch_queue(void *background)
{
	struct carryfs_background *bg = background;

	queue_work(system_unbound_wq, &bg->prefetch);
}

/* Before unmounting evicts inodes: the queue holds references. */
void carryfs_background_stop(void *background)
{
	struct carryfs_background *bg = background;

	if (!bg)
		return;
	spin_lock(&bg->lock);
	bg->stopped = true;
	spin_unlock(&bg->lock);
	cancel_work_sync(&bg->prefetch);
	flush_work(&bg->flush);
	/* Closes cannot queue more once stopped; drain what raced the stop. */
	carryfs_flush_fn(&bg->flush);
	kfree(bg);
}

/*
 * At the last close of a file written through it, have a worker start its
 * writeback, so its data is on its way before an fsync of it or its
 * directory, as the FUSE adapter writes data before fsync and NFS writes
 * back on close. The close itself does not wait.
 */
int carryfs_file_release(struct inode *inode, struct file *file)
{
	struct carryfs_background *bg = carryfs_volume_background(inode->i_sb);
	bool queued = false;

	if (!(file->f_mode & FMODE_WRITE) || !bg ||
	    !mapping_tagged(inode->i_mapping, PAGECACHE_TAG_DIRTY) || !igrab(inode))
		return 0;
	spin_lock(&bg->lock);
	if (!bg->stopped && bg->count < CARRYFS_FLUSH_BATCH) {
		bg->inodes[bg->count++] = inode;
		queued = true;
	}
	spin_unlock(&bg->lock);
	if (queued) {
		queue_work(system_unbound_wq, &bg->flush);
	} else {
		filemap_flush(inode->i_mapping);
		iput(inode);
	}
	return 0;
}

static struct file_system_type carryfs_fs_type = {
	.owner		= THIS_MODULE,
	.name		= "carryfs",
	.init_fs_context = carryfs_init_fs_context,
	.parameters	= carryfs_param_spec,
	.kill_sb	= carryfs_kill_sb,
	.fs_flags	= FS_REQUIRES_DEV,
};
MODULE_ALIAS_FS("carryfs");

/*
 * Verifies readahead units once their reads complete, and records data
 * units once their writes do, outside interrupt context, as erofs and
 * btrfs verify data and iomap completes ioends in work.
 */
static struct workqueue_struct *carryfs_read_wq;

int carryfs_register(void)
{
	int error;

	carryfs_read_wq = alloc_workqueue("carryfs-io", WQ_MEM_RECLAIM | WQ_UNBOUND, 0);
	if (!carryfs_read_wq)
		return -ENOMEM;
	error = register_filesystem(&carryfs_fs_type);
	if (error)
		destroy_workqueue(carryfs_read_wq);
	return error;
}

void carryfs_unregister(void)
{
	unregister_filesystem(&carryfs_fs_type);
	destroy_workqueue(carryfs_read_wq);
}

/* A chain of data reads or writes in flight; its batch belongs to Rust. */
struct carryfs_read {
	struct work_struct work;
	void *batch;
	int status;
};

static void carryfs_read_work(struct work_struct *work)
{
	struct carryfs_read *read = container_of(work, struct carryfs_read, work);

	carryfs_read_done(read->batch, read->status);
	kfree(read);
}

static void carryfs_read_end_io(struct bio *bio)
{
	struct carryfs_read *read = bio->bi_private;

	read->status = blk_status_to_errno(bio->bi_status);
	bio_put(bio);
	queue_work(carryfs_read_wq, &read->work);
}

static void carryfs_write_work(struct work_struct *work)
{
	struct carryfs_read *write = container_of(work, struct carryfs_read, work);

	carryfs_write_done(write->batch, write->status);
	kfree(write);
}

/*
 * Submit @bio, the last of a chain of data writes, and call
 * carryfs_write_done(@batch) from the I/O workqueue once all complete, as
 * iomap completes ioends. Fails only before submitting.
 */
int carryfs_submit_write(struct bio *bio, void *batch)
{
	struct carryfs_read *write = kmalloc(sizeof(*write), GFP_NOFS);

	if (!write)
		return -ENOMEM;
	INIT_WORK(&write->work, carryfs_write_work);
	write->batch = batch;
	bio->bi_private = write;
	bio->bi_end_io = carryfs_read_end_io;
	submit_bio(bio);
	return 0;
}

/*
 * Submit @bio, the last of a chain, and call carryfs_read_done(@batch) from
 * the read workqueue once the whole chain completes. Fails only before
 * submitting.
 */
int carryfs_submit_read(struct bio *bio, void *batch)
{
	struct carryfs_read *read = kmalloc(sizeof(*read), GFP_NOFS);

	if (!read)
		return -ENOMEM;
	INIT_WORK(&read->work, carryfs_read_work);
	read->batch = batch;
	bio->bi_private = read;
	bio->bi_end_io = carryfs_read_end_io;
	submit_bio(bio);
	return 0;
}

/* Block device geometry. */

bool carryfs_bdev_nonrot(struct super_block *sb)
{
#if LINUX_VERSION_CODE < KERNEL_VERSION(7, 2, 0)
	return bdev_nonrot(sb->s_bdev);
#else
	return !bdev_rot(sb->s_bdev);
#endif
}

u64 carryfs_bdev_bytes(struct super_block *sb)
{
	return bdev_nr_bytes(sb->s_bdev);
}

unsigned int carryfs_bdev_logical_block_size(struct super_block *sb)
{
	return bdev_logical_block_size(sb->s_bdev);
}

unsigned int carryfs_bdev_physical_block_size(struct super_block *sb)
{
	return bdev_physical_block_size(sb->s_bdev);
}

void carryfs_set_uuid(struct super_block *sb, const u8 *uuid)
{
	super_set_uuid(sb, uuid, 16);
}

void carryfs_statfs_fill(struct kstatfs *buf, u64 type, u64 bsize, u64 blocks, u64 bfree,
			 u64 bavail, u64 namelen, u32 fsid_low, u32 fsid_high)
{
	buf->f_type = type;
	buf->f_bsize = bsize;
	buf->f_frsize = bsize;
	buf->f_blocks = blocks;
	buf->f_bfree = bfree;
	buf->f_bavail = bavail;
	buf->f_namelen = namelen;
	buf->f_fsid.val[0] = fsid_low;
	buf->f_fsid.val[1] = fsid_high;
}

/* Metadata pages through the block device's buffer cache. */

struct buffer_head *carryfs_bread(struct super_block *sb, u64 block)
{
	return sb_bread(sb, block);
}

void carryfs_brelse(struct buffer_head *bh)
{
	brelse(bh);
}

/* @block if the buffer cache holds it uptodate, else NULL. */
struct buffer_head *carryfs_bread_cached(struct super_block *sb, u64 block)
{
	struct buffer_head *bh = sb_find_get_block(sb, block);

	if (bh && !buffer_uptodate(bh)) {
		brelse(bh);
		return NULL;
	}
	return bh;
}

/* Start reading those of @count blocks that are not cached, in one batch. */
void carryfs_readahead_blocks(struct super_block *sb, const u64 *blocks, unsigned int count)
{
	struct blk_plug plug;
	unsigned int i;

	blk_start_plug(&plug);
	for (i = 0; i < count; i++)
		sb_breadahead(sb, blocks[i]);
	blk_finish_plug(&plug);
}

const u8 *carryfs_bh_data(struct buffer_head *bh)
{
	return bh->b_data;
}

/* Set once a page passed its checksum and record validation. */
#define BH_Carryfs_Verified BH_PrivateStart

bool carryfs_bh_verified(struct buffer_head *bh)
{
	return test_bit(BH_Carryfs_Verified, &bh->b_state);
}

void carryfs_bh_set_verified(struct buffer_head *bh)
{
	set_bit(BH_Carryfs_Verified, &bh->b_state);
}

/*
 * Write @count pages, @pages[i] to block @blocks[i], through the buffer
 * cache, and wait for them. The written buffers stay cached and verified:
 * the caller validated each page. Returns 0 or -EIO.
 */
int carryfs_write_blocks(struct super_block *sb, const u64 *blocks, const u8 *const *pages,
			 unsigned int count)
{
	struct blk_plug plug;
	unsigned int i;
	int error = 0;

	blk_start_plug(&plug);
	for (i = 0; i < count; i++) {
		struct buffer_head *bh = sb_getblk(sb, blocks[i]);

		lock_buffer(bh);
		memcpy(bh->b_data, pages[i], sb->s_blocksize);
		set_buffer_uptodate(bh);
		set_bit(BH_Carryfs_Verified, &bh->b_state);
		mark_buffer_dirty(bh);
		unlock_buffer(bh);
		write_dirty_buffer(bh, REQ_SYNC);
		brelse(bh);
	}
	blk_finish_plug(&plug);
	for (i = 0; i < count; i++) {
		struct buffer_head *bh = sb_find_get_block(sb, blocks[i]);

		if (!bh)
			continue;
		wait_on_buffer(bh);
		if (buffer_write_io_error(bh) || !buffer_uptodate(bh))
			error = -EIO;
		brelse(bh);
	}
	return error;
}

/*
 * Write back and wait for every inode of @sb with dirty pages, walking the
 * inode list as drop_pagecache_sb() does; sync_inodes_sb() would need
 * s_umount. Returns the first error.
 */
int carryfs_write_dirty_inodes(struct super_block *sb)
{
	struct inode *inode, **started = NULL, **grown;
	unsigned int count = 0, capacity = 0, i;
	struct blk_plug plug;
	int error = 0, status;

	/*
	 * Start every dirty inode's writeback under one plug, so the block
	 * layer merges their writes, then wait for them all: writeback
	 * completes in the I/O workqueue.
	 */
	blk_start_plug(&plug);
	spin_lock(&sb->s_inode_list_lock);
	list_for_each_entry(inode, &sb->s_inodes, i_sb_list) {
		spin_lock(&inode->i_lock);
		if ((inode_state_read(inode) & (I_FREEING | I_WILL_FREE | I_NEW)) ||
		    !mapping_tagged(inode->i_mapping, PAGECACHE_TAG_DIRTY)) {
			spin_unlock(&inode->i_lock);
			continue;
		}
		__iget(inode);
		spin_unlock(&inode->i_lock);
		spin_unlock(&sb->s_inode_list_lock);

		if (count == capacity) {
			capacity = max(2 * capacity, 64U);
			grown = krealloc_array(started, capacity, sizeof(*started), GFP_NOFS);
			if (!grown) {
				blk_finish_plug(&plug);
				error = -ENOMEM;
				iput(inode);
				goto wait;
			}
			started = grown;
		}
		started[count++] = inode;
		status = filemap_fdatawrite(inode->i_mapping);
		if (!error)
			error = status;

		cond_resched();
		spin_lock(&sb->s_inode_list_lock);
	}
	spin_unlock(&sb->s_inode_list_lock);
	blk_finish_plug(&plug);
wait:
	for (i = 0; i < count; i++) {
		status = filemap_fdatawait(started[i]->i_mapping);
		if (!error)
			error = status;
		iput(started[i]);
	}
	kfree(started);
	return error;
}

/*
 * A regular file's i_private holds the unit it last reserved, plus one,
 * so that writes into a unit already reserved skip the volume lock. Every
 * release of the file's reservations clears it before testing whether a
 * unit is busy; a write tests it after locking its folio, which makes the
 * unit busy. With a full barrier on each side, either the release sees
 * the locked folio and keeps the unit, or the write sees the cleared hint
 * and takes the lock.
 */
bool carryfs_reserved_hint(struct inode *inode, u64 unit)
{
	smp_mb();
	return (unsigned long)READ_ONCE(inode->i_private) == unit + 1;
}

void carryfs_set_reserved_hint(struct inode *inode, u64 unit)
{
	WRITE_ONCE(inode->i_private, (void *)(unsigned long)(unit + 1));
}

void carryfs_clear_reserved_hint(struct address_space *mapping)
{
	WRITE_ONCE(mapping->host->i_private, NULL);
	smp_mb();
}

/* A durable cache flush: earlier writes reach media before later ones. */
int carryfs_flush(struct super_block *sb)
{
	return blkdev_issue_flush(sb->s_bdev);
}

unsigned int carryfs_nofs_save(void)
{
	return memalloc_nofs_save();
}

void carryfs_nofs_restore(unsigned int flags)
{
	memalloc_nofs_restore(flags);
}

/* Inodes, dentries, and directory iteration. */

bool carryfs_inode_is_new(struct inode *inode)
{
	return inode_state_read_once(inode) & I_NEW;
}

void carryfs_i_size_write(struct inode *inode, loff_t size)
{
	i_size_write(inode, size);
}

/*
 * Instantiate a dentry for an entry readdir just returned, as NFS and CIFS
 * do for readdirplus, so stats after a listing find it in the dcache.
 * Entries already cached are left alone; failures only skip the priming.
 * @known, if not NULL, is the entry's inode record as readdir read it.
 */
void carryfs_prime_dentry(struct dentry *parent, const char *name, int len, u64 ino,
			  umode_t type, const void *known)
{
	struct qstr qname = QSTR_INIT(name, len);
#if LINUX_VERSION_CODE < KERNEL_VERSION(7, 2, 0)
	DECLARE_WAIT_QUEUE_HEAD_ONSTACK(wq);
#endif
	struct dentry *dentry, *alias;
	struct inode *inode;

	/* Hashes with the filesystem's d_hash, then looks up. */
	dentry = try_lookup_noperm(&qname, parent);
	if (dentry) {
		if (!IS_ERR(dentry))
			dput(dentry);
		return;
	}
#if LINUX_VERSION_CODE < KERNEL_VERSION(7, 2, 0)
	dentry = d_alloc_parallel(parent, &qname, &wq);
#else
	/* 7.2 dropped the caller's wait queue. */
	dentry = d_alloc_parallel(parent, &qname);
#endif
	if (IS_ERR(dentry))
		return;
	if (!d_in_lookup(dentry))
		goto out;
	inode = carryfs_iget(parent->d_sb, ino, type, known);
	if (IS_ERR(inode)) {
		d_lookup_done(dentry);
		goto out;
	}
	alias = d_splice_alias(inode, dentry);
	d_lookup_done(dentry);
	if (alias && !IS_ERR(alias))
		dput(alias);
out:
	dput(dentry);
}

u64 carryfs_inode_query_iversion(struct inode *inode)
{
	return inode_query_iversion(inode);
}

bool carryfs_inode_eq_iversion(struct inode *inode, u64 version)
{
	return inode_eq_iversion(inode, version);
}

void carryfs_inode_inc_iversion(struct inode *dir)
{
	inode_inc_iversion(dir);
}

/*
 * Positive dentries stay valid. A negative one stays valid while its
 * directory is unchanged, so a later case-insensitive create is seen, and
 * never for creation, so the new name keeps the caller's spelling. Nothing
 * here sleeps, so RCU path walks keep their fast path.
 */
int carryfs_revalidate_negative(struct inode *dir, struct dentry *dentry, unsigned int flags)
{
	if (d_really_is_positive(dentry))
		return 1;
	if (flags & (LOOKUP_CREATE | LOOKUP_RENAME_TARGET))
		return 0;
	return inode_eq_iversion(dir, dentry->d_time);
}

  bool carryfs_dir_emit_dots(struct file *file, struct dir_context *ctx)
{
	return dir_emit_dots(file, ctx);
}

/* Block I/O. */

struct bio *carryfs_bio_alloc_read(struct super_block *sb, u64 sector, unsigned int pages)
{
	struct bio *bio = bio_alloc(sb->s_bdev, pages, REQ_OP_READ, GFP_NOIO);

	bio->bi_iter.bi_sector = sector;
	return bio;
}

struct bio *carryfs_bio_alloc_write(struct super_block *sb, u64 sector, unsigned int pages)
{
	struct bio *bio = bio_alloc(sb->s_bdev, pages, REQ_OP_WRITE, GFP_NOIO);

	bio->bi_iter.bi_sector = sector;
	return bio;
}

/* New data blocks may still sit in the block device's cache as metadata. */
void carryfs_clean_aliases(struct super_block *sb, u64 block, u64 count)
{
	clean_bdev_aliases(sb->s_bdev, block, count);
}

/* A block plug for a batch of writes; NULL when out of memory. */
void *carryfs_plug_start(void)
{
	struct blk_plug *plug = kmalloc(sizeof(*plug), GFP_NOFS);

	if (plug)
		blk_start_plug(plug);
	return plug;
}

void carryfs_plug_finish(void *plug)
{
	if (plug) {
		blk_finish_plug(plug);
		kfree(plug);
	}
}

void carryfs_file_allow_direct(struct file *file)
{
	file->f_mode |= FMODE_CAN_ODIRECT;
}

/* Page cache. */

/* The locked folio a buffered write fills, or an ERR_PTR. */
struct folio *carryfs_write_folio(struct address_space *mapping, loff_t pos)
{
	return __filemap_get_folio(mapping, pos >> PAGE_SHIFT, FGP_WRITEBEGIN,
				   mapping_gfp_mask(mapping));
}

bool carryfs_folio_uptodate(struct folio *folio)
{
	return folio_test_uptodate(folio);
}

void carryfs_folio_mark_uptodate(struct folio *folio)
{
	folio_mark_uptodate(folio);
}

/* simple_write_end() from fs/libfs.c, which is not exported. */
int carryfs_write_end(const struct kiocb *iocb, struct address_space *mapping, loff_t pos,
		      unsigned int len, unsigned int copied, struct folio *folio, void *fsdata)
{
	struct inode *inode = folio->mapping->host;
	loff_t last_pos = pos + copied;

	if (!folio_test_uptodate(folio)) {
		if (copied < len) {
			size_t from = offset_in_folio(folio, pos);

			folio_zero_range(folio, from + copied, len - copied);
		}
		folio_mark_uptodate(folio);
	}
	if (last_pos > inode->i_size)
		i_size_write(inode, last_pos);
	folio_mark_dirty(folio);
	folio_unlock(folio);
	folio_put(folio);
	return copied;
}

/*
 * An uptodate folio of @mapping at @index, locked and referenced, or NULL
 * when the page cache has none.
 */
struct folio *carryfs_cached_folio(struct address_space *mapping, pgoff_t index)
{
	struct folio *folio = filemap_lock_folio(mapping, index);

	if (IS_ERR(folio))
		return NULL;
	if (!folio_test_uptodate(folio)) {
		folio_unlock(folio);
		folio_put(folio);
		return NULL;
	}
	return folio;
}

/*
 * Whether a folio of @mapping in [@first, @last] is dirty, or locked: a
 * write holds its folio locked from reserving space until dirtying it. With
 * @writeback, a folio under writeback counts too.
 */
bool carryfs_range_busy(struct address_space *mapping, pgoff_t first, pgoff_t last,
			bool writeback)
{
	XA_STATE(xas, &mapping->i_pages, first);
	struct folio *folio;
	bool busy = false;

	rcu_read_lock();
	xas_for_each(&xas, folio, last) {
		if (xas_retry(&xas, folio) || xa_is_value(folio))
			continue;
		if (folio_test_dirty(folio) || folio_test_locked(folio) ||
		    (writeback && folio_test_writeback(folio))) {
			busy = true;
			break;
		}
	}
	rcu_read_unlock();
	return busy;
}

/*
 * filemap_page_mkwrite() from mm/filemap.c, reserving space for the folio's
 * unit while it is locked, before a shared mapping dirties it.
 */
static vm_fault_t carryfs_page_mkwrite(struct vm_fault *vmf)
{
	struct address_space *mapping = vmf->vma->vm_file->f_mapping;
	struct folio *folio = page_folio(vmf->page);
	vm_fault_t ret = VM_FAULT_LOCKED;
	int error;

	sb_start_pagefault(mapping->host->i_sb);
	file_update_time(vmf->vma->vm_file);
	folio_lock(folio);
	if (folio->mapping != mapping) {
		folio_unlock(folio);
		ret = VM_FAULT_NOPAGE;
		goto out;
	}
	error = carryfs_reserve_folio(mapping->host, folio->index);
	if (error) {
		folio_unlock(folio);
		ret = vmf_error(error);
		goto out;
	}
	folio_mark_dirty(folio);
	folio_wait_stable(folio);
out:
	sb_end_pagefault(mapping->host->i_sb);
	return ret;
}

static const struct vm_operations_struct carryfs_file_vm_ops = {
	.fault = filemap_fault,
	.map_pages = filemap_map_pages,
	.page_mkwrite = carryfs_page_mkwrite,
};

int carryfs_file_mmap_prepare(struct vm_area_desc *desc)
{
	int error = generic_file_mmap_prepare(desc);

	if (!error)
		desc->vm_ops = &carryfs_file_vm_ops;
	return error;
}

void carryfs_folio_unlock_put(struct folio *folio)
{
	folio_unlock(folio);
	folio_put(folio);
}

void carryfs_folio_start_writeback(struct folio *folio)
{
	folio_start_writeback(folio);
	folio_unlock(folio);
}

void carryfs_folio_redirty(struct writeback_control *wbc, struct folio *folio)
{
	folio_redirty_for_writepage(wbc, folio);
}

bool carryfs_wbc_background(struct writeback_control *wbc)
{
	return wbc->for_background;
}

void carryfs_mapping_error(struct address_space *mapping, int error)
{
	mapping_set_error(mapping, error);
}

loff_t carryfs_i_size_read(struct inode *inode)
{
	return i_size_read(inode);
}

struct inode *carryfs_mapping_host(struct address_space *mapping)
{
	return mapping->host;
}

struct folio *carryfs_writeback_iter(struct address_space *mapping,
				     struct writeback_control *wbc, struct folio *folio,
				     int *error)
{
	return writeback_iter(mapping, wbc, folio, error);
}

struct inode *carryfs_folio_inode(struct folio *folio)
{
	return folio->mapping->host;
}

struct inode *carryfs_readahead_inode(struct readahead_control *rac)
{
	return rac->mapping->host;
}

u64 carryfs_readahead_index(struct readahead_control *rac)
{
	return readahead_index(rac);
}

u64 carryfs_readahead_count(struct readahead_control *rac)
{
	return readahead_count(rac);
}

struct folio *carryfs_readahead_folio(struct readahead_control *rac)
{
	return readahead_folio(rac);
}

loff_t carryfs_folio_pos(struct folio *folio)
{
	return folio_pos(folio);
}

void *carryfs_kmap_local_folio(struct folio *folio, size_t offset)
{
	return kmap_local_folio(folio, offset);
}

void carryfs_kunmap_local(void *addr)
{
	kunmap_local(addr);
}

void *carryfs_page_address(struct page *page)
{
	return page_address(page);
}

/* Checksums: the kernel picks the fastest CRC32C for this CPU. */

u32 carryfs_crc32c(u32 crc, const void *data, size_t len)
{
	return crc32c(crc, data, len);
}
