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
#include <linux/module.h>
#include <linux/namei.h>
#include <linux/pagemap.h>
#include <linux/slab.h>
#include <linux/statfs.h>
#include <linux/version.h>

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
	if (PAGE_SIZE != 4096)
		return -EOPNOTSUPP;
	if (!(fc->sb_flags & SB_RDONLY))
		return -EROFS;
	return get_tree_bdev(fc, carryfs_fill_super_fc);
}

static int carryfs_reconfigure(struct fs_context *fc)
{
	if (!(fc->sb_flags & SB_RDONLY))
		return -EROFS;
	/* This revision mounts read-only only. */
	fc->sb_flags |= SB_RDONLY;
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
	void *volume = sb->s_fs_info;

	kill_block_super(sb);
	carryfs_free_volume(volume);
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

int carryfs_register(void)
{
	return register_filesystem(&carryfs_fs_type);
}

void carryfs_unregister(void)
{
	unregister_filesystem(&carryfs_fs_type);
}

/* Block device geometry. */

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

/* Inodes, dentries, and directory iteration. */

bool carryfs_inode_is_new(struct inode *inode)
{
	return inode_state_read_once(inode) & I_NEW;
}

void carryfs_i_size_write(struct inode *inode, loff_t size)
{
	i_size_write(inode, size);
}

void carryfs_inode_set_ops(struct inode *inode, const struct inode_operations *iop,
			   const struct file_operations *fop,
			   const struct address_space_operations *aops)
{
	inode->i_op = iop;
	if (fop)
		inode->i_fop = fop;
	if (aops)
		inode->i_mapping->a_ops = aops;
}

void carryfs_inode_set_link(struct inode *inode, char *link)
{
	inode->i_link = link;
}

char *carryfs_inode_link(struct inode *inode)
{
	return inode->i_link;
}

const struct qstr *carryfs_dentry_name(const struct dentry *dentry)
{
	return &dentry->d_name;
}

const unsigned char *carryfs_qstr_name(const struct qstr *name)
{
	return name->name;
}

u32 carryfs_qstr_len(const struct qstr *name)
{
	return name->len;
}

void carryfs_qstr_set_hash(struct qstr *name, u32 hash)
{
	name->hash = hash;
}

bool carryfs_dir_emit(struct dir_context *ctx, const char *name, int len, u64 ino,
		      unsigned int type)
{
	return dir_emit(ctx, name, len, ino, type);
}

/*
 * Instantiate a dentry for an entry readdir just returned, as NFS and CIFS
 * do for readdirplus, so stats after a listing find it in the dcache.
 * Entries already cached are left alone; failures only skip the priming.
 */
void carryfs_prime_dentry(struct dentry *parent, const char *name, int len, u64 ino,
			  umode_t type)
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
	dentry = d_alloc_parallel(parent, &qname);
#endif
	if (IS_ERR(dentry))
		return;
	if (!d_in_lookup(dentry))
		goto out;
	inode = carryfs_iget(parent->d_sb, ino, type);
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

/*
 * A directory inode's i_private marks a listing that primed every entry;
 * readers race only to set it.
 */
bool carryfs_dir_primed(struct inode *dir)
{
	return READ_ONCE(dir->i_private);
}

void carryfs_set_dir_primed(struct inode *dir)
{
	WRITE_ONCE(dir->i_private, (void *)1);
}

struct dentry *carryfs_file_dentry(struct file *file)
{
	return file->f_path.dentry;
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

/* Page cache. */

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
