// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Inodes: loading catalog inode records, lookup, and operation tables.

use core::ffi::c_void;
use core::ptr;

use kernel::bindings;
use kernel::prelude::*;

use crate::glue::{self, qstr_bytes};
use crate::ondisk::error::Error as Encoding;
use crate::tree::{self, Tree, le_u64};
use crate::{corrupt, dir, file, names, volume};

/// The root directory's object ID.
pub(crate) const ROOT: u64 = 1;

/// Catalog object types (FORMAT.md section 5, "Inode").
const FILE: u8 = 1;
const DIRECTORY: u8 = 2;
const SYMLINK: u8 = 3;

/// Operation tables hold raw pointers, such as `owner`, so they need an
/// explicit `Sync` to be statics. They are immutable.
#[repr(transparent)]
pub(crate) struct Ops<T>(pub(crate) T);

// SAFETY: The tables are never written after compilation, and the VFS only
// reads them.
unsafe impl<T> Sync for Ops<T> {}

/// A referenced inode for `object`, read from the catalog unless cached.
pub(crate) fn get(sb: *mut bindings::super_block, object: u64) -> Result<*mut bindings::inode> {
    // SAFETY: `sb` is an active super block of this filesystem.
    let inode = unsafe { bindings::iget_locked(sb, object as _) };
    if inode.is_null() {
        return Err(ENOMEM);
    }
    // SAFETY: `iget_locked` returned a referenced inode.
    if !unsafe { glue::carryfs_inode_is_new(inode) } {
        return Ok(inode);
    }
    // SAFETY: The inode is new and locked, so only this thread fills it.
    match unsafe { fill(sb, inode, object) } {
        Ok(()) => {
            // SAFETY: The inode is new and now complete.
            unsafe { bindings::unlock_new_inode(inode) };
            Ok(inode)
        }
        Err(error) => {
            // SAFETY: The inode is new; `iget_failed` unlocks and drops it.
            unsafe { bindings::iget_failed(inode) };
            Err(error)
        }
    }
}

/// A timestamp field of an inode record: signed seconds, then nanoseconds.
fn timestamp(value: &[u8], offset: usize) -> (i64, u32) {
    let seconds = le_u64(value, offset) as i64;
    let nanos = value
        .get(offset + 8..offset + 12)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u32::from_le_bytes);
    (seconds, nanos)
}

/// # Safety
///
/// `inode` must be a new, locked inode of `sb`.
unsafe fn fill(sb: *mut bindings::super_block, inode: *mut bindings::inode, object: u64) -> Result {
    let volume = volume::of(sb);
    let key = tree::prefix(object, tree::INODE);
    let found = Tree::catalog(sb, volume).get(&key, |value| {
        // `decode_page` validated the record: its type, length, flags, and
        // timestamp ranges.
        let kind = value.first().copied().unwrap_or(0);
        let (mode, iop, fop, aops): (u32, _, _, _) = match kind {
            FILE => (
                bindings::S_IFREG | 0o644,
                &FILE_INODE_OPERATIONS.0 as *const _,
                &file::FILE_OPERATIONS.0 as *const _,
                &file::ADDRESS_SPACE_OPERATIONS.0 as *const _,
            ),
            DIRECTORY => (
                bindings::S_IFDIR | 0o755,
                &DIRECTORY_INODE_OPERATIONS.0 as *const _,
                &dir::FILE_OPERATIONS.0 as *const _,
                ptr::null(),
            ),
            SYMLINK => (
                bindings::S_IFLNK | 0o777,
                &SYMLINK_INODE_OPERATIONS.0 as *const _,
                ptr::null(),
                ptr::null(),
            ),
            _ => return Err(corrupt(object, Encoding::Corrupt("invalid inode type"))),
        };
        let size = le_u64(value, 16);
        let (mtime, mtime_nanos) = timestamp(value, 56);
        let (ctime, ctime_nanos) = timestamp(value, 72);
        // SAFETY: The caller owns the new inode.
        unsafe {
            (*inode).i_mode = mode as _;
            (*inode).i_uid = volume.uid;
            (*inode).i_gid = volume.gid;
            bindings::set_nlink(inode, 1);
            glue::carryfs_i_size_write(inode, size as i64);
            (*inode).i_blocks = le_u64(value, 24).saturating_mul(8) as _;
            // Access times are not stored; report the modification time.
            (*inode).i_atime_sec = mtime;
            (*inode).i_atime_nsec = mtime_nanos;
            (*inode).i_mtime_sec = mtime;
            (*inode).i_mtime_nsec = mtime_nanos;
            (*inode).i_ctime_sec = ctime;
            (*inode).i_ctime_nsec = ctime_nanos;
            glue::carryfs_inode_set_ops(inode, iop, fop, aops);
        }
        if kind == SYMLINK {
            let target = value.get(128..).unwrap_or_default();
            let mut link = KVec::with_capacity(target.len() + 1, GFP_KERNEL)?;
            link.extend_from_slice(target, GFP_KERNEL)?;
            link.push(0, GFP_KERNEL)?;
            let (link, _, _) = link.into_raw_parts();
            // SAFETY: `free_inode` frees the NUL-terminated target.
            unsafe { glue::carryfs_inode_set_link(inode, link) };
        }
        Ok(())
    })?;
    found.ok_or_else(|| corrupt(volume.root.catalog.block, Encoding::Corrupt("missing inode record")))
}

/// Free an inode after its RCU grace period, with its symlink target.
pub(crate) unsafe extern "C" fn free_inode(inode: *mut bindings::inode) {
    // SAFETY: The VFS passes an inode of this filesystem with no users left.
    unsafe {
        if (*inode).i_mode as u32 & bindings::S_IFMT == bindings::S_IFLNK {
            bindings::kfree(glue::carryfs_inode_link(inode) as *const c_void);
        }
        bindings::free_inode_nonrcu(inode);
    }
}

unsafe extern "C" fn lookup(
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    _flags: u32,
) -> *mut bindings::dentry {
    // SAFETY: The VFS holds `dir` locked and `dentry` referenced.
    match unsafe { lookup_inode(dir, dentry) } {
        // SAFETY: `inode` is referenced or null; `d_splice_alias` consumes it.
        Ok(inode) => unsafe { bindings::d_splice_alias(inode, dentry) },
        Err(error) => error.to_ptr(),
    }
}

/// The inode `dentry` names in `dir`, or null when there is none.
///
/// # Safety
///
/// `dir` and `dentry` must be valid for the duration of the call.
unsafe fn lookup_inode(
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
) -> Result<*mut bindings::inode> {
    // SAFETY: The caller guarantees both are valid.
    let (sb, parent, name) = unsafe {
        (
            (*dir).i_sb,
            (*dir).i_ino as u64,
            qstr_bytes(glue::carryfs_dentry_name(dentry)),
        )
    };
    let Some(key) = names::entry_key(parent, name)? else {
        // No stored name folds to this one, so it cannot exist.
        return Ok(ptr::null_mut());
    };
    let volume = volume::of(sb);
    let entry = Tree::catalog(sb, volume).get(&key, |value| {
        Ok((le_u64(value, 0), value.get(8).copied().unwrap_or(0)))
    })?;
    let Some((target, kind)) = entry else {
        return Ok(ptr::null_mut());
    };
    get_typed(sb, target, kind_mode(kind))
}

/// The file-type mode bits of a directory-entry type.
pub(crate) fn kind_mode(kind: u8) -> u32 {
    match kind {
        FILE => bindings::S_IFREG,
        DIRECTORY => bindings::S_IFDIR,
        _ => bindings::S_IFLNK,
    }
}

/// `get`, failing as corruption unless the inode has file type `mode`, as
/// the directory entry naming it records.
fn get_typed(sb: *mut bindings::super_block, object: u64, mode: u32) -> Result<*mut bindings::inode> {
    let inode = get(sb, object)?;
    // SAFETY: `get` returned a referenced inode.
    if unsafe { (*inode).i_mode } as u32 & bindings::S_IFMT != mode {
        // SAFETY: Drop the reference `get` took.
        unsafe { bindings::iput(inode) };
        return Err(corrupt(
            volume::of(sb).root.catalog.block,
            Encoding::Corrupt("directory entry type differs from its inode"),
        ));
    }
    Ok(inode)
}

/// `get_typed` for glue.c, which primes dentries from readdir: a referenced
/// inode or an `ERR_PTR`.
#[no_mangle]
unsafe extern "C" fn carryfs_iget(
    sb: *mut bindings::super_block,
    object: u64,
    mode: bindings::umode_t,
) -> *mut bindings::inode {
    match get_typed(sb, object, u32::from(mode)) {
        Ok(inode) => inode,
        Err(error) => error.to_ptr(),
    }
}

pub(crate) static DIRECTORY_INODE_OPERATIONS: Ops<bindings::inode_operations> =
    Ops(bindings::inode_operations {
        lookup: Some(lookup),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });

pub(crate) static FILE_INODE_OPERATIONS: Ops<bindings::inode_operations> =
    // SAFETY: All-zero entries are absent operations; files use the defaults.
    Ops(unsafe { core::mem::zeroed() });

pub(crate) static SYMLINK_INODE_OPERATIONS: Ops<bindings::inode_operations> =
    Ops(bindings::inode_operations {
        get_link: Some(bindings::simple_get_link),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
