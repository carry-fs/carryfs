// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Inodes: loading catalog inode records, lookup, attributes, eviction, and
//! operation tables.

use core::ffi::c_void;
use core::ptr;

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::glue;
use crate::namei::{self, DIRECTORY, FILE, SYMLINK};
use crate::ondisk::error::Error as Encoding;
use crate::tree::{le_u64, Tree};
use crate::vfs::{Dentry, Inode};
use crate::{corrupt, dir, file, names, volume};

/// The root directory's object ID.
pub(crate) const ROOT: u64 = 1;

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

/// Copy a record's times into the VFS inode. Access times are not stored,
/// so the access time reports the modification time.
///
/// # Safety
///
/// `inode` must be valid and not concurrently changing its times.
unsafe fn set_inode_times(inode: *mut bindings::inode, record: &[u8]) {
    let (mtime, mtime_nanos) = timestamp(record, 56);
    let (ctime, ctime_nanos) = timestamp(record, 72);
    // SAFETY: The caller guarantees the inode is valid.
    unsafe {
        (*inode).i_atime_sec = mtime;
        (*inode).i_atime_nsec = mtime_nanos;
        (*inode).i_mtime_sec = mtime;
        (*inode).i_mtime_nsec = mtime_nanos;
        (*inode).i_ctime_sec = ctime;
        (*inode).i_ctime_nsec = ctime_nanos;
    }
}

/// Reload `inode`'s times after a change to its record; the caller holds
/// it locked.
pub(crate) fn refresh_times(inode: &Inode) {
    let sb = inode.super_block();
    let state = sb.volume().lock();
    if let Ok(record) = namei::read_record(sb.as_ptr(), &state, inode.object()) {
        // SAFETY: The caller holds the inode locked, so no other writer
        // changes its times.
        unsafe { set_inode_times(inode.as_ptr(), &record) };
    }
}

/// # Safety
///
/// `inode` must be a new, locked inode of `sb`.
unsafe fn fill(sb: *mut bindings::super_block, inode: *mut bindings::inode, object: u64) -> Result {
    let volume = volume::of(sb);
    let record = {
        let state = volume.lock();
        namei::read_record(sb, &state, object).map_err(|error| {
            if error == ENOENT {
                corrupt(
                    state.root.catalog.block,
                    Encoding::Corrupt("missing inode record"),
                )
            } else {
                error
            }
        })?
    };
    // `decode_page` validated the record: its type, length, flags, and
    // timestamp ranges.
    let kind = record.first().copied().unwrap_or(0);
    let (mode, iop, fop, aops): (
        u32,
        *const bindings::inode_operations,
        *const bindings::file_operations,
        *const bindings::address_space_operations,
    ) = match kind {
        FILE => (
            bindings::S_IFREG | 0o644,
            &FILE_INODE_OPERATIONS.0 as *const _,
            &file::FILE_OPERATIONS.0 as *const _,
            &file::ADDRESS_SPACE_OPERATIONS.0 as *const _,
        ),
        DIRECTORY => (
            bindings::S_IFDIR | 0o755,
            &namei::DIRECTORY_INODE_OPERATIONS.0 as *const _,
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
    // SAFETY: The caller owns the new inode.
    unsafe {
        (*inode).i_mode = mode as _;
        (*inode).i_uid = volume.uid;
        (*inode).i_gid = volume.gid;
        bindings::set_nlink(inode, 1);
        glue::carryfs_i_size_write(inode, le_u64(&record, 16) as i64);
        (*inode).i_blocks = le_u64(&record, namei::ALLOCATED).saturating_mul(8) as _;
        set_inode_times(inode, &record);
        (*inode).i_op = iop;
        if !fop.is_null() {
            (*inode).__bindgen_anon_3.i_fop = fop;
        }
        if !aops.is_null() {
            (*(*inode).i_mapping).a_ops = aops;
        }
    }
    if kind == SYMLINK {
        let target = record.get(128..).unwrap_or_default();
        let mut link = KVec::with_capacity(target.len() + 1, GFP_KERNEL)?;
        link.extend_from_slice(target, GFP_KERNEL)?;
        link.push(0, GFP_KERNEL)?;
        let (link, _, _) = link.into_raw_parts();
        // SAFETY: `free_inode` frees the NUL-terminated target.
        unsafe { (*inode).__bindgen_anon_5.i_link = link.cast() };
    }
    Ok(())
}

/// Free an inode after its RCU grace period, with its symlink target.
pub(crate) unsafe extern "C" fn free_inode(inode: *mut bindings::inode) {
    // SAFETY: The VFS passes an inode of this filesystem with no users left.
    unsafe {
        if (*inode).i_mode as u32 & bindings::S_IFMT == bindings::S_IFLNK {
            bindings::kfree((*inode).__bindgen_anon_5.i_link as *const c_void);
        }
        bindings::free_inode_nonrcu(inode);
    }
}

/// Drop an inode's pages; remove the object if it was unlinked.
pub(crate) unsafe extern "C" fn evict_inode(raw: *mut bindings::inode) {
    // SAFETY: The VFS passes an inode it is evicting, which no one else
    // uses.
    let inode = unsafe {
        bindings::truncate_inode_pages_final(&mut (*raw).i_data);
        Inode::from_raw(raw)
    };
    if inode.is_dir() {
        dir::forget(inode);
    }
    let sb = inode.super_block();
    if !inode.is_dir() {
        // Its pages are gone, dirty ones too.
        sb.volume()
            .lock()
            .reservations
            .release_from(inode.object(), 0);
    }
    if inode.unlinked() && sb.volume().writable {
        if let Err(error) = namei::remove_object(sb.as_ptr(), inode.object()) {
            pr_err!("cannot remove object {}: {:?}\n", inode.object(), error);
        }
    }
    // SAFETY: Eviction ends here.
    unsafe { bindings::clear_inode(raw) };
}

pub(crate) unsafe extern "C" fn setattr(
    idmap: *mut bindings::mnt_idmap,
    dentry: *mut bindings::dentry,
    attr: *mut bindings::iattr,
) -> c_int {
    // SAFETY: The VFS passes a referenced dentry and its locked inode.
    match unsafe { change_attributes(idmap, dentry, attr) } {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// # Safety
///
/// As for `setattr`.
unsafe fn change_attributes(
    idmap: *mut bindings::mnt_idmap,
    dentry: *mut bindings::dentry,
    attr: *mut bindings::iattr,
) -> Result {
    // SAFETY: The caller guarantees the arguments are valid.
    unsafe {
        kernel::error::to_result(bindings::setattr_prepare(idmap, dentry, attr))?;
        let inode = (*dentry).d_inode;
        let valid = (*attr).ia_valid;
        // Owners and permissions are not stored; only no-op changes pass.
        if (valid & bindings::ATTR_MODE != 0 && (*attr).ia_mode != (*inode).i_mode)
            || (valid & bindings::ATTR_UID != 0
                && (*attr).__bindgen_anon_1.ia_uid.val != (*inode).i_uid.val)
            || (valid & bindings::ATTR_GID != 0
                && (*attr).__bindgen_anon_2.ia_gid.val != (*inode).i_gid.val)
        {
            return Err(EPERM);
        }
        if valid & bindings::ATTR_SIZE != 0 {
            crate::write::set_size(Inode::from_raw(inode), (*attr).ia_size as u64)?;
        }
        if valid & (bindings::ATTR_MTIME | bindings::ATTR_CTIME) != 0 {
            let modified = (valid & bindings::ATTR_MTIME != 0)
                .then(|| ((*attr).ia_mtime.tv_sec, (*attr).ia_mtime.tv_nsec as u32));
            namei::set_times((*inode).i_sb, (*inode).i_ino as u64, modified)?;
        }
        bindings::setattr_copy(idmap, inode, attr);
        refresh_times(Inode::from_raw(inode));
    }
    Ok(())
}

pub(crate) unsafe extern "C" fn lookup(
    dir: *mut bindings::inode,
    dentry: *mut bindings::dentry,
    _flags: u32,
) -> *mut bindings::dentry {
    // SAFETY: The VFS holds `dir` locked and `dentry` referenced.
    let (dir_ref, dentry_ref) = unsafe { (Inode::from_raw(dir), Dentry::from_raw(dentry)) };
    match lookup_inode(dir_ref, dentry_ref) {
        Ok(inode) => {
            if inode.is_null() {
                // SAFETY: As above; negative dentries remember the directory
                // version they saw.
                unsafe { (*dentry).d_time = glue::carryfs_inode_query_iversion(dir) as _ };
            }
            // SAFETY: `inode` is referenced or null; `d_splice_alias`
            // consumes it.
            unsafe { bindings::d_splice_alias(inode, dentry) }
        }
        Err(error) => error.to_ptr(),
    }
}

/// The inode `dentry` names in `dir`, referenced, or null when there is
/// none.
fn lookup_inode(dir: &Inode, dentry: &Dentry) -> Result<*mut bindings::inode> {
    let (sb, parent, name) = (dir.super_block().as_ptr(), dir.object(), dentry.name());
    let Some(key) = names::entry_key(parent, name)? else {
        // No stored name folds to this one, so it cannot exist.
        return Ok(ptr::null_mut());
    };
    // Look the entry up under the lock, but get its inode after: filling an
    // inode takes the lock, and another thread may be filling this one.
    let entry = {
        let state = volume::of(sb).lock();
        Tree::catalog(sb, &state).get(&key, |value| {
            Ok((le_u64(value, 0), value.get(8).copied().unwrap_or(0)))
        })?
    };
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
fn get_typed(
    sb: *mut bindings::super_block,
    object: u64,
    mode: u32,
) -> Result<*mut bindings::inode> {
    let inode = get(sb, object)?;
    // SAFETY: `get` returned a referenced inode.
    if unsafe { (*inode).i_mode } as u32 & bindings::S_IFMT != mode {
        // SAFETY: Drop the reference `get` took.
        unsafe { bindings::iput(inode) };
        return Err(corrupt(
            object,
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

pub(crate) static FILE_INODE_OPERATIONS: Ops<bindings::inode_operations> =
    Ops(bindings::inode_operations {
        setattr: Some(setattr),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });

pub(crate) static SYMLINK_INODE_OPERATIONS: Ops<bindings::inode_operations> =
    Ops(bindings::inode_operations {
        get_link: Some(bindings::simple_get_link),
        setattr: Some(setattr),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
