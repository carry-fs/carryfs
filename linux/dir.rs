// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Directory iteration.
//!
//! Entries are catalog records keyed by comparison key, so a position is
//! only an ordinal. Each open directory keeps the key of the last entry it
//! returned; reading on from that position resumes after the key, and any
//! other position, after a seek, counts entries from the start.

use core::ffi::c_void;

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::glue;
use crate::inode::Ops;
use crate::ondisk::name::validate_disk_name_characters;
use crate::tree::{self, Tree, le_u64};
use crate::{corrupt, volume};

/// Where an open directory stopped: the position after its last entry, and
/// that entry's key.
struct Cursor {
    pos: i64,
    key: KVec<u8>,
}

unsafe extern "C" fn iterate(file: *mut bindings::file, ctx: *mut bindings::dir_context) -> c_int {
    // SAFETY: The VFS passes an open directory and its context, and holds
    // the position lock, so this call alone uses the cursor.
    match unsafe { iterate_entries(file, ctx) } {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// # Safety
///
/// `file` must be an open directory of this filesystem, used by no other
/// iteration, and `ctx` its context.
unsafe fn iterate_entries(file: *mut bindings::file, ctx: *mut bindings::dir_context) -> Result {
    // SAFETY: The caller guarantees both are valid.
    if !unsafe { glue::carryfs_dir_emit_dots(file, ctx) } {
        return Ok(());
    }
    // SAFETY: As above.
    let (inode, cursor) = unsafe { ((*file).f_inode, &mut (*file).private_data) };
    // SAFETY: An open file holds its inode.
    let (sb, dir) = unsafe { ((*inode).i_sb, (*inode).i_ino as u64) };
    // SAFETY: `private_data` is null or a `Cursor` this file owns.
    let cursor = unsafe { (*cursor as *mut Cursor).as_mut() };
    // SAFETY: As above.
    let pos = unsafe { (*ctx).pos };
    let prefix = tree::prefix(dir, tree::ENTRY);
    let (start, resume, mut skip): (&[u8], bool, i64) = match cursor.as_deref() {
        Some(cursor) if cursor.pos == pos => (&cursor.key, true, 0),
        _ => (&prefix, false, pos - 2),
    };
    let mut last = KVec::new();
    last.extend_from_slice(start, GFP_KERNEL)?;
    let mut emitted = 0;
    let volume = volume::of(sb);
    Tree::catalog(sb, volume).scan(start, &mut |key, value| {
        if !key.starts_with(&prefix) {
            return Ok(false);
        }
        if resume && key == start {
            return Ok(true);
        }
        if skip > 0 {
            skip -= 1;
            return Ok(true);
        }
        let name = value.get(12..).unwrap_or_default();
        validate_disk_name_characters(name)
            .map_err(|error| corrupt(volume.root.catalog.block, error))?;
        let kind = match value.get(8) {
            Some(1) => bindings::DT_REG,
            Some(2) => bindings::DT_DIR,
            _ => bindings::DT_LNK,
        };
        // SAFETY: `ctx` is valid; the name is at most 255 bytes.
        if !unsafe {
            glue::carryfs_dir_emit(ctx, name.as_ptr(), name.len() as c_int, le_u64(value, 0), kind)
        } {
            return Ok(false);
        }
        // SAFETY: As above.
        unsafe { (*ctx).pos += 1 };
        last.clear();
        last.extend_from_slice(key, GFP_KERNEL)?;
        emitted += 1;
        Ok(true)
    })?;
    if emitted == 0 {
        return Ok(());
    }
    // SAFETY: As above.
    let pos = unsafe { (*ctx).pos };
    match cursor {
        Some(cursor) => {
            cursor.pos = pos;
            cursor.key = last;
        }
        None => {
            let cursor = KBox::new(Cursor { pos, key: last }, GFP_KERNEL)?;
            // SAFETY: `release` frees it with the file.
            unsafe { (*file).private_data = KBox::into_raw(cursor) as *mut c_void };
        }
    }
    Ok(())
}

unsafe extern "C" fn release(_inode: *mut bindings::inode, file: *mut bindings::file) -> c_int {
    // SAFETY: The last user of the file is closing it.
    let cursor = unsafe { (*file).private_data } as *mut Cursor;
    if !cursor.is_null() {
        // SAFETY: `iterate_entries` made it with `KBox::into_raw`.
        drop(unsafe { KBox::from_raw(cursor) });
    }
    0
}

pub(crate) static FILE_OPERATIONS: Ops<bindings::file_operations> =
    Ops(bindings::file_operations {
        llseek: Some(bindings::generic_file_llseek),
        read: Some(bindings::generic_read_dir),
        iterate_shared: Some(iterate),
        release: Some(release),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
