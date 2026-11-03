// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! The C helpers in `glue.c`.

use core::ffi::c_void;
use kernel::bindings::super_block;
use kernel::bindings::{dentry, dir_context, file, folio, inode, page, qstr, readahead_control};
use kernel::ffi::c_int;

/// A `struct buffer_head`, which the Rust bindings leave out.
#[repr(C)]
pub(crate) struct BufferHead {
    _opaque: [u8; 0],
}

// The kernel's own binding declarations allow this too: bindgen leaves some
// fields of these structs out, but only pointers to them cross this boundary.
#[allow(improper_ctypes)]
unsafe extern "C" {
    pub(crate) fn carryfs_register() -> c_int;
    pub(crate) fn carryfs_unregister();

    pub(crate) fn carryfs_bdev_bytes(sb: *mut super_block) -> u64;
    pub(crate) fn carryfs_bdev_logical_block_size(sb: *mut super_block) -> u32;
    pub(crate) fn carryfs_bdev_physical_block_size(sb: *mut super_block) -> u32;
    pub(crate) fn carryfs_set_uuid(sb: *mut super_block, uuid: *const u8);
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn carryfs_statfs_fill(
        buf: *mut kernel::bindings::kstatfs,
        kind: u64,
        block_size: u64,
        blocks: u64,
        free: u64,
        available: u64,
        name_len: u64,
        fsid_low: u32,
        fsid_high: u32,
    );

    pub(crate) fn carryfs_bread(sb: *mut super_block, block: u64) -> *mut BufferHead;
    pub(crate) fn carryfs_brelse(bh: *mut BufferHead);
    pub(crate) fn carryfs_bread_cached(sb: *mut super_block, block: u64) -> *mut BufferHead;
    pub(crate) fn carryfs_readahead_blocks(sb: *mut super_block, blocks: *const u64, count: u32);
    pub(crate) fn carryfs_bh_data(bh: *mut BufferHead) -> *const u8;
    pub(crate) fn carryfs_bh_verified(bh: *mut BufferHead) -> bool;
    pub(crate) fn carryfs_bh_set_verified(bh: *mut BufferHead);

    pub(crate) fn carryfs_write_blocks(
        sb: *mut super_block,
        blocks: *const u64,
        pages: *const *const u8,
        count: u32,
    ) -> c_int;
    pub(crate) fn carryfs_flush(sb: *mut super_block) -> c_int;
    pub(crate) fn carryfs_write_dirty_inodes(sb: *mut super_block) -> c_int;
    pub(crate) fn carryfs_timer_start(sb: *mut super_block) -> *mut c_void;
    pub(crate) fn carryfs_timer_stop(timer: *mut c_void);
    pub(crate) fn carryfs_nofs_save() -> u32;
    pub(crate) fn carryfs_nofs_restore(flags: u32);
    pub(crate) fn carryfs_inode_is_new(inode: *mut inode) -> bool;
    pub(crate) fn carryfs_i_size_write(inode: *mut inode, size: i64);
    pub(crate) fn carryfs_prime_dentry(
        parent: *mut dentry,
        name: *const u8,
        len: c_int,
        ino: u64,
        kind: u16,
    );
    pub(crate) fn carryfs_inode_query_iversion(inode: *mut inode) -> u64;
    pub(crate) fn carryfs_inode_eq_iversion(inode: *mut inode, version: u64) -> bool;
    pub(crate) fn carryfs_inode_inc_iversion(dir: *mut inode);
    pub(crate) fn carryfs_revalidate_negative(
        dir: *mut inode,
        dentry: *mut dentry,
        flags: u32,
    ) -> c_int;
    pub(crate) fn carryfs_dir_emit_dots(file: *mut file, ctx: *mut dir_context) -> bool;

    pub(crate) fn carryfs_bio_alloc_read(
        sb: *mut super_block,
        sector: u64,
        pages: u32,
    ) -> *mut kernel::bindings::bio;
    pub(crate) fn carryfs_folio_inode(folio: *mut folio) -> *mut inode;
    pub(crate) fn carryfs_bio_alloc_write(
        sb: *mut super_block,
        sector: u64,
        pages: u32,
    ) -> *mut kernel::bindings::bio;
    pub(crate) fn carryfs_file_allow_direct(file: *mut kernel::bindings::file);
    pub(crate) fn carryfs_plug_start() -> *mut c_void;
    pub(crate) fn carryfs_plug_finish(plug: *mut c_void);
    pub(crate) fn carryfs_clean_aliases(sb: *mut super_block, block: u64, count: u64);
    pub(crate) fn carryfs_write_folio(
        mapping: *mut kernel::bindings::address_space,
        pos: i64,
    ) -> *mut folio;
    pub(crate) fn carryfs_folio_uptodate(folio: *mut folio) -> bool;
    pub(crate) fn carryfs_folio_mark_uptodate(folio: *mut folio);
    pub(crate) fn carryfs_write_end(
        iocb: *const kernel::bindings::kiocb,
        mapping: *mut kernel::bindings::address_space,
        pos: i64,
        len: u32,
        copied: u32,
        folio: *mut folio,
        fsdata: *mut c_void,
    ) -> c_int;
    pub(crate) fn carryfs_cached_folio(
        mapping: *mut kernel::bindings::address_space,
        index: u64,
    ) -> *mut folio;
    pub(crate) fn carryfs_folio_unlock_put(folio: *mut folio);
    pub(crate) fn carryfs_range_busy(
        mapping: *mut kernel::bindings::address_space,
        first: u64,
        last: u64,
    ) -> bool;
    pub(crate) fn carryfs_file_mmap_prepare(desc: *mut kernel::bindings::vm_area_desc) -> c_int;
    pub(crate) fn carryfs_folio_start_writeback(folio: *mut folio);
    pub(crate) fn carryfs_folio_redirty(
        wbc: *mut kernel::bindings::writeback_control,
        folio: *mut folio,
    );
    pub(crate) fn carryfs_mapping_error(
        mapping: *mut kernel::bindings::address_space,
        error: c_int,
    );
    pub(crate) fn carryfs_i_size_read(inode: *mut inode) -> i64;
    pub(crate) fn carryfs_mapping_host(mapping: *mut kernel::bindings::address_space)
        -> *mut inode;
    pub(crate) fn carryfs_writeback_iter(
        mapping: *mut kernel::bindings::address_space,
        wbc: *mut kernel::bindings::writeback_control,
        folio: *mut folio,
        error: *mut c_int,
    ) -> *mut folio;
    pub(crate) fn carryfs_readahead_inode(rac: *mut readahead_control) -> *mut inode;
    pub(crate) fn carryfs_readahead_index(rac: *mut readahead_control) -> u64;
    pub(crate) fn carryfs_readahead_count(rac: *mut readahead_control) -> u64;
    pub(crate) fn carryfs_readahead_folio(rac: *mut readahead_control) -> *mut folio;
    pub(crate) fn carryfs_folio_pos(folio: *mut folio) -> i64;
    pub(crate) fn carryfs_kmap_local_folio(folio: *mut folio, offset: usize) -> *mut c_void;
    pub(crate) fn carryfs_kunmap_local(addr: *mut c_void);
    pub(crate) fn carryfs_page_address(page: *mut page) -> *mut c_void;
}

/// A name the VFS passed in, as bytes.
///
/// # Safety
///
/// `name` must point to a valid `qstr` whose bytes outlive the returned slice.
pub(crate) unsafe fn qstr_bytes<'a>(name: *const qstr) -> &'a [u8] {
    // SAFETY: The caller guarantees `name` is valid; a qstr's `name` points
    // to `len` readable bytes.
    unsafe {
        core::slice::from_raw_parts(
            (*name).name,
            (*name).__bindgen_anon_1.__bindgen_anon_1.len as usize,
        )
    }
}
