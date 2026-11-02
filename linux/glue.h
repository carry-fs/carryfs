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

/* Implemented in Rust. */
int carryfs_fill_super(struct super_block *sb, u32 uid, u32 gid, int silent);
void carryfs_free_volume(void *volume);
struct inode *carryfs_iget(struct super_block *sb, u64 ino, umode_t type);

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
bool carryfs_inode_is_new(struct inode *inode);
void carryfs_i_size_write(struct inode *inode, loff_t size);
void carryfs_inode_set_ops(struct inode *inode, const struct inode_operations *iop, const struct file_operations *fop, const struct address_space_operations *aops);
void carryfs_inode_set_link(struct inode *inode, char *link);
char *carryfs_inode_link(struct inode *inode);
const struct qstr *carryfs_dentry_name(const struct dentry *dentry);
const unsigned char *carryfs_qstr_name(const struct qstr *name);
u32 carryfs_qstr_len(const struct qstr *name);
void carryfs_qstr_set_hash(struct qstr *name, u32 hash);
bool carryfs_dir_emit(struct dir_context *ctx, const char *name, int len, u64 ino, unsigned int type);
void carryfs_prime_dentry(struct dentry *parent, const char *name, int len, u64 ino,
			  umode_t type);
bool carryfs_dir_primed(struct inode *dir);
void carryfs_set_dir_primed(struct inode *dir);
struct dentry *carryfs_file_dentry(struct file *file);
void carryfs_inode_inc_iversion(struct inode *dir);
void carryfs_set_dentry_version(struct dentry *dentry, struct inode *dir);
int carryfs_revalidate_negative(struct inode *dir, struct dentry *dentry, unsigned int flags);
unsigned int carryfs_inode_nlink(struct inode *inode);
bool carryfs_dir_emit_dots(struct file *file, struct dir_context *ctx);
struct bio *carryfs_bio_alloc_read(struct super_block *sb, u64 sector, unsigned int pages);
struct inode *carryfs_folio_inode(struct folio *folio);
struct inode *carryfs_readahead_inode(struct readahead_control *rac);
u64 carryfs_readahead_index(struct readahead_control *rac);
u64 carryfs_readahead_count(struct readahead_control *rac);
struct folio *carryfs_readahead_folio(struct readahead_control *rac);
loff_t carryfs_folio_pos(struct folio *folio);
void *carryfs_kmap_local_folio(struct folio *folio, size_t offset);
void carryfs_kunmap_local(void *addr);
void *carryfs_page_address(struct page *page);
u32 carryfs_crc32c(u32 crc, const void *data, size_t len);

#endif /* CARRYFS_GLUE_H */
