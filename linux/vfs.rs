// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Typed views of the VFS objects the driver uses.
//!
//! Linux 7.0's `kernel` crate has no filesystem abstractions, so these
//! wrappers stand in for the `kernel::fs` an upstream series would add:
//! each is `#[repr(transparent)]` over `Opaque<T>`, as `kernel::fs::File`
//! is, and states once the invariants that make its methods safe. The
//! VFS callbacks turn raw pointers into these references at the boundary;
//! filesystem logic works with references.

use core::ffi::c_void;

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::types::Opaque;

use crate::glue;
use crate::volume::{self, Volume};

/// A super block of this filesystem.
///
/// # Invariants
///
/// It is mounted and its `s_fs_info` holds this driver's `Volume`.
#[repr(transparent)]
pub(crate) struct SuperBlock(Opaque<bindings::super_block>);

impl SuperBlock {
    /// # Safety
    ///
    /// `ptr` must be a mounted super block of this filesystem, valid for
    /// `'a`, whose `s_fs_info` is set.
    pub(crate) unsafe fn from_raw<'a>(ptr: *mut bindings::super_block) -> &'a Self {
        // SAFETY: The caller guarantees validity; the cast is sound
        // because the type is `repr(transparent)`.
        unsafe { &*ptr.cast() }
    }

    pub(crate) fn as_ptr(&self) -> *mut bindings::super_block {
        self.0.get()
    }

    pub(crate) fn volume(&self) -> &Volume {
        volume::of(self.as_ptr())
    }
}

/// An inode of this filesystem.
///
/// # Invariants
///
/// It is referenced for as long as the borrow lasts, and belongs to a
/// mounted super block of this filesystem.
#[repr(transparent)]
pub(crate) struct Inode(Opaque<bindings::inode>);

impl Inode {
    /// # Safety
    ///
    /// `ptr` must be a referenced inode of this filesystem, valid for `'a`.
    pub(crate) unsafe fn from_raw<'a>(ptr: *mut bindings::inode) -> &'a Self {
        // SAFETY: As for `SuperBlock::from_raw`.
        unsafe { &*ptr.cast() }
    }

    pub(crate) fn as_ptr(&self) -> *mut bindings::inode {
        self.0.get()
    }

    /// The catalog object ID, which is the inode number.
    pub(crate) fn object(&self) -> u64 {
        // SAFETY: `i_ino` is set before the inode is visible and never
        // changes.
        unsafe { (*self.as_ptr()).i_ino as u64 }
    }

    pub(crate) fn super_block(&self) -> &SuperBlock {
        // SAFETY: The type invariant: the inode's super block is mounted
        // and of this filesystem.
        unsafe { SuperBlock::from_raw((*self.as_ptr()).i_sb) }
    }

    /// The file-type bits of the mode.
    pub(crate) fn file_type(&self) -> u32 {
        // SAFETY: The file type never changes after the inode is filled.
        unsafe { (*self.as_ptr()).i_mode as u32 & bindings::S_IFMT }
    }

    pub(crate) fn is_dir(&self) -> bool {
        self.file_type() == bindings::S_IFDIR
    }

    /// The size the page cache sees.
    pub(crate) fn size(&self) -> u64 {
        // SAFETY: `i_size_read` reads the size consistently.
        unsafe { glue::carryfs_i_size_read(self.as_ptr()) as u64 }
    }

    pub(crate) fn mapping(&self) -> *mut bindings::address_space {
        // SAFETY: A regular file's mapping is set at creation and fixed.
        unsafe { (*self.as_ptr()).i_mapping }
    }

    /// Modification and change times, as seconds and nanoseconds.
    pub(crate) fn times(&self) -> ((i64, u32), (i64, u32)) {
        // SAFETY: Times are plain fields; a torn read only mixes two
        // valid times, as `generic_fillattr` may.
        unsafe {
            let inode = self.as_ptr();
            (
                ((*inode).i_mtime_sec, (*inode).i_mtime_nsec),
                ((*inode).i_ctime_sec, (*inode).i_ctime_nsec),
            )
        }
    }

    /// Whether the inode was unlinked.
    pub(crate) fn unlinked(&self) -> bool {
        // SAFETY: Reading the link count is always valid.
        unsafe { glue::carryfs_inode_nlink(self.as_ptr()) == 0 }
    }

    /// Mark the inode unlinked; the caller holds it locked.
    pub(crate) fn clear_nlink(&self) {
        // SAFETY: The inode is referenced; `clear_nlink` updates it.
        unsafe { bindings::clear_nlink(self.as_ptr()) }
    }

    /// Bump the version, which invalidates negative dentries and listings;
    /// the caller holds the directory locked exclusively.
    pub(crate) fn bump_version(&self) {
        // SAFETY: The inode is referenced.
        unsafe { glue::carryfs_inode_inc_iversion(self.as_ptr()) }
    }

    pub(crate) fn version(&self) -> u64 {
        // SAFETY: The inode is referenced.
        unsafe { glue::carryfs_inode_query_iversion(self.as_ptr()) }
    }

    pub(crate) fn version_is(&self, version: u64) -> bool {
        // SAFETY: The inode is referenced.
        unsafe { glue::carryfs_inode_eq_iversion(self.as_ptr(), version) }
    }

    /// The private pointer a directory keeps its listing in (dir.rs).
    pub(crate) fn listing(&self) -> *mut c_void {
        // SAFETY: The inode is referenced; the field is read atomically.
        unsafe { glue::carryfs_dir_cache(self.as_ptr()) }
    }

    pub(crate) fn publish_listing(&self, listing: *mut c_void) -> bool {
        // SAFETY: As above; the field is swapped atomically.
        unsafe { glue::carryfs_dir_cache_publish(self.as_ptr(), listing) }
    }

    pub(crate) fn take_listing(&self) -> *mut c_void {
        // SAFETY: As above.
        unsafe { glue::carryfs_dir_cache_take(self.as_ptr()) }
    }
}

/// A dentry of this filesystem.
///
/// # Invariants
///
/// It is referenced for as long as the borrow lasts, and its name is
/// stable: the VFS holds what locks name changes need.
#[repr(transparent)]
pub(crate) struct Dentry(Opaque<bindings::dentry>);

impl Dentry {
    /// # Safety
    ///
    /// `ptr` must be a referenced dentry whose name stays stable for `'a`.
    pub(crate) unsafe fn from_raw<'a>(ptr: *mut bindings::dentry) -> &'a Self {
        // SAFETY: As for `SuperBlock::from_raw`.
        unsafe { &*ptr.cast() }
    }

    pub(crate) fn as_ptr(&self) -> *mut bindings::dentry {
        self.0.get()
    }

    pub(crate) fn name(&self) -> &[u8] {
        // SAFETY: The type invariant keeps the name stable while borrowed.
        unsafe { glue::qstr_bytes(glue::carryfs_dentry_name(self.as_ptr())) }
    }

    /// The dentry's inode, if positive.
    pub(crate) fn inode(&self) -> Option<&Inode> {
        // SAFETY: A positive dentry holds its inode referenced.
        unsafe {
            (*self.as_ptr())
                .d_inode
                .as_mut()
                .map(|inode| Inode::from_raw(inode))
        }
    }
}

/// A locked, order-0 page-cache folio.
///
/// # Invariants
///
/// It is locked, or under writeback, for as long as the borrow lasts.
#[repr(transparent)]
pub(crate) struct Folio(Opaque<bindings::folio>);

impl Folio {
    /// # Safety
    ///
    /// `ptr` must be a locked page-cache folio, valid for `'a`.
    pub(crate) unsafe fn from_raw<'a>(ptr: *mut bindings::folio) -> &'a Self {
        // SAFETY: As for `SuperBlock::from_raw`.
        unsafe { &*ptr.cast() }
    }

    pub(crate) fn as_ptr(&self) -> *mut bindings::folio {
        self.0.get()
    }

    /// The folio's byte position in its file.
    pub(crate) fn pos(&self) -> u64 {
        // SAFETY: The type invariant: the folio is in the page cache.
        unsafe { glue::carryfs_folio_pos(self.as_ptr()) as u64 }
    }

    /// The inode whose page cache holds the folio.
    pub(crate) fn inode(&self) -> &Inode {
        // SAFETY: A locked page-cache folio's mapping and host are stable,
        // and the host is referenced while it has pages.
        unsafe { Inode::from_raw(glue::carryfs_folio_inode(self.as_ptr())) }
    }

    /// Calls `f` with the folio's page mapped.
    pub(crate) fn with_bytes<T>(&self, f: impl FnOnce(&mut [u8]) -> T) -> T {
        // SAFETY: A locked order-0 folio maps one page, which no one else
        // writes while it is locked; the mapping ends below.
        unsafe {
            let address = glue::carryfs_kmap_local_folio(self.as_ptr(), 0);
            let result = f(core::slice::from_raw_parts_mut(
                address.cast::<u8>(),
                crate::ondisk::format::BLOCK_BYTES,
            ));
            glue::carryfs_kunmap_local(address);
            result
        }
    }

    /// End a read: mark the folio uptodate on success, and unlock it.
    pub(crate) fn end_read(&self, ok: bool) {
        // SAFETY: The type invariant: the folio is locked for a read.
        unsafe { bindings::folio_end_read(self.as_ptr(), ok) }
    }
}

/// A directory iteration's context.
#[repr(transparent)]
pub(crate) struct DirContext(Opaque<bindings::dir_context>);

impl DirContext {
    /// # Safety
    ///
    /// `ptr` must be the context of an iteration that lasts for `'a`.
    pub(crate) unsafe fn from_raw<'a>(ptr: *mut bindings::dir_context) -> &'a Self {
        // SAFETY: As for `SuperBlock::from_raw`.
        unsafe { &*ptr.cast() }
    }

    pub(crate) fn pos(&self) -> i64 {
        // SAFETY: Only this iteration uses its context.
        unsafe { (*self.0.get()).pos }
    }

    /// Emit an entry and advance the position; `false` when the caller's
    /// buffer is full.
    pub(crate) fn emit(&self, name: &[u8], ino: u64, kind: u8) -> bool {
        let file_type = match kind {
            1 => bindings::DT_REG,
            2 => bindings::DT_DIR,
            _ => bindings::DT_LNK,
        };
        // SAFETY: Only this iteration uses its context; names are at most
        // 255 bytes.
        unsafe {
            if !glue::carryfs_dir_emit(
                self.0.get(),
                name.as_ptr(),
                name.len() as c_int,
                ino,
                file_type,
            ) {
                return false;
            }
            (*self.0.get()).pos += 1;
        }
        true
    }
}
