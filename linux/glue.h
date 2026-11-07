/* SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0 */
/*
 * The interface between the CarryFS Rust driver and its C helpers in glue.c.
 * glue.rs declares the same functions for Rust.
 */
#ifndef CARRYFS_GLUE_H
#define CARRYFS_GLUE_H

#include <linux/fs.h>

struct buffer_head;
struct dir_context;
struct kstatfs;
struct readahead_control;
struct writeback_control;

/* Implemented in Rust. */
int carryfs_fill_super(struct super_block *sb, u32 uid, u32 gid, int silent);
void carryfs_free_volume(void *volume);
void carryfs_stop_timer(struct super_block *sb);
void carryfs_periodic_commit(struct super_block *sb);
struct inode *carryfs_iget(struct super_block *sb, u64 ino, umode_t type);
int carryfs_reserve_folio(struct inode *inode, u64 index);

/* Implemented in glue.c. */
int carryfs_register(void);
void carryfs_unregister(void);
u64 carryfs_bdev_bytes(struct super_block *sb);
unsigned int carryfs_bdev_logical_block_size(struct super_block *sb);
unsigned int carryfs_bdev_physical_block_size(struct super_block *sb);
void carryfs_set_uuid(struct super_block *sb, const u8 *uuid);
void carryfs_statfs_fill(struct kstatfs *buf, u64 type, u64 bsize, u64 blocks, u64 bfree, u64 bavail, u64 namelen, u32 fsid_low, u32 fsid_high);
struct buffer_head *carryfs_bread(struct super_block *sb, u64 block);
void carryfs_brelse(struct buffer_head *bh);
struct buffer_head *carryfs_bread_cached(struct super_block *sb, u64 block);
void carryfs_readahead_blocks(struct super_block *sb, const u64 *blocks, unsigned int count);
const u8 *carryfs_bh_data(struct buffer_head *bh);
bool carryfs_bh_verified(struct buffer_head *bh);
void carryfs_bh_set_verified(struct buffer_head *bh);
int carryfs_write_blocks(struct super_block *sb, const u64 *blocks, const u8 *const *pages,
			 unsigned int count);
int carryfs_flush(struct super_block *sb);
int carryfs_write_dirty_inodes(struct super_block *sb);
void *carryfs_timer_start(struct super_block *sb);
void carryfs_timer_stop(void *timer);
unsigned int carryfs_nofs_save(void);
void carryfs_nofs_restore(unsigned int flags);
bool carryfs_inode_is_new(struct inode *inode);
void carryfs_i_size_write(struct inode *inode, loff_t size);
void carryfs_prime_dentry(struct dentry *parent, const char *name, int len, u64 ino,
			  umode_t type);
u64 carryfs_inode_query_iversion(struct inode *inode);
bool carryfs_inode_eq_iversion(struct inode *inode, u64 version);
void carryfs_inode_inc_iversion(struct inode *dir);
int carryfs_revalidate_negative(struct inode *dir, struct dentry *dentry, unsigned int flags);
bool carryfs_dir_emit_dots(struct file *file, struct dir_context *ctx);
struct bio *carryfs_bio_alloc_read(struct super_block *sb, u64 sector, unsigned int pages);
struct inode *carryfs_folio_inode(struct folio *folio);
struct inode *carryfs_readahead_inode(struct readahead_control *rac);
u64 carryfs_readahead_index(struct readahead_control *rac);
u64 carryfs_readahead_count(struct readahead_control *rac);
struct bio *carryfs_bio_alloc_write(struct super_block *sb, u64 sector, unsigned int pages);
void carryfs_file_allow_direct(struct file *file);
void *carryfs_plug_start(void);
void carryfs_plug_finish(void *plug);
void carryfs_clean_aliases(struct super_block *sb, u64 block, u64 count);
struct folio *carryfs_write_folio(struct address_space *mapping, loff_t pos);
bool carryfs_folio_uptodate(struct folio *folio);
void carryfs_folio_mark_uptodate(struct folio *folio);
int carryfs_write_end(const struct kiocb *iocb, struct address_space *mapping, loff_t pos,
		      unsigned int len, unsigned int copied, struct folio *folio, void *fsdata);
struct folio *carryfs_cached_folio(struct address_space *mapping, pgoff_t index);
void carryfs_folio_unlock_put(struct folio *folio);
bool carryfs_range_busy(struct address_space *mapping, pgoff_t first, pgoff_t last);
struct vm_area_desc;
int carryfs_file_mmap_prepare(struct vm_area_desc *desc);
void carryfs_folio_start_writeback(struct folio *folio);
void carryfs_folio_redirty(struct writeback_control *wbc, struct folio *folio);
void carryfs_mapping_error(struct address_space *mapping, int error);
loff_t carryfs_i_size_read(struct inode *inode);
struct inode *carryfs_mapping_host(struct address_space *mapping);
struct folio *carryfs_writeback_iter(struct address_space *mapping,
				     struct writeback_control *wbc, struct folio *folio,
				     int *error);
struct folio *carryfs_readahead_folio(struct readahead_control *rac);
loff_t carryfs_folio_pos(struct folio *folio);
void *carryfs_kmap_local_folio(struct folio *folio, size_t offset);
void carryfs_kunmap_local(void *addr);
void *carryfs_page_address(struct page *page);
u32 carryfs_crc32c(u32 crc, const void *data, size_t len);

#endif /* CARRYFS_GLUE_H */
