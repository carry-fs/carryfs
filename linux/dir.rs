// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Directory iteration.
//!
//! Entries are catalog records keyed by comparison key, so a position is
//! only an ordinal. Each open directory keeps the key of the last entry it
//! returned; reading on from that position resumes after the key, and any
//! other position, after a seek, counts entries from the start.
//!
//! Listing also instantiates each entry's dentry, as NFS and CIFS do for
//! readdirplus, because a listing is usually followed by a stat of each
//! entry. A directory listed whole that way is marked primed until its
//! inode is evicted, and later listings skip the work.

use core::ffi::c_void;

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::glue;
use crate::inode::Ops;
use crate::ondisk::name::validate_disk_name_characters;
use crate::tree::{self, le_u64, Tree};
use crate::{corrupt, inode, volume};

/// Entries read per hold of the volume lock (FORMAT.md section 6's
/// directory page).
const BATCH: usize = 64;

/// An entry read for emitting.
struct Entry {
    key: KVec<u8>,
    name: KVec<u8>,
    target: u64,
    kind: u8,
}

/// Where an open directory stopped: the position after its last entry,
/// that entry's key, and whether this pass began at the first entry and
/// primed every entry since.
struct Cursor {
    pos: i64,
    key: KVec<u8>,
    priming_whole: bool,
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
    // SAFETY: An open file pins its dentry.
    let parent = unsafe { glue::carryfs_file_dentry(file) };
    // SAFETY: The open file holds its inode.
    let prime = !unsafe { glue::carryfs_dir_primed(inode) };
    let (start, resume, mut skip, whole): (&[u8], bool, i64, bool) = match cursor.as_deref() {
        Some(cursor) if cursor.pos == pos => (&cursor.key, true, 0, cursor.priming_whole),
        _ => (&prefix, false, pos - 2, pos == 2),
    };
    let whole = whole && prime;
    let mut ended = false;
    let mut resume = resume;
    let mut last = KVec::new();
    last.extend_from_slice(start, GFP_KERNEL)?;
    let mut emitted = 0;
    let volume = volume::of(sb);
    // Entries are read under the volume lock in batches, then emitted and
    // primed without it: priming fills inodes, which takes the lock.
    'listing: loop {
        let mut batch: KVec<Entry> = KVec::new();
        {
            let state = volume.lock();
            let catalog = state.root.catalog.block;
            Tree::catalog(sb, &state).scan(&last, &mut |key, value| {
                if !key.starts_with(&prefix) {
                    ended = true;
                    return Ok(false);
                }
                if resume && key == &last[..] {
                    return Ok(true);
                }
                if skip > 0 {
                    skip -= 1;
                    return Ok(true);
                }
                let name = value.get(12..).unwrap_or_default();
                validate_disk_name_characters(name).map_err(|error| corrupt(catalog, error))?;
                let mut entry = Entry {
                    key: KVec::new(),
                    name: KVec::new(),
                    target: le_u64(value, 0),
                    kind: value.get(8).copied().unwrap_or(0),
                };
                entry.key.extend_from_slice(key, GFP_KERNEL)?;
                entry.name.extend_from_slice(name, GFP_KERNEL)?;
                batch.push(entry, GFP_KERNEL)?;
                Ok(batch.len() < BATCH)
            })?;
        }
        let full = batch.len() == BATCH;
        for entry in batch {
            let dt = match entry.kind {
                1 => bindings::DT_REG,
                2 => bindings::DT_DIR,
                _ => bindings::DT_LNK,
            };
            let name = &entry.name;
            // SAFETY: `ctx` is valid; the name is at most 255 bytes.
            if !unsafe {
                glue::carryfs_dir_emit(ctx, name.as_ptr(), name.len() as c_int, entry.target, dt)
            } {
                break 'listing;
            }
            // SAFETY: `ctx` is valid, and the open directory pins its dentry.
            unsafe {
                (*ctx).pos += 1;
                if prime {
                    glue::carryfs_prime_dentry(
                        parent,
                        name.as_ptr(),
                        name.len() as c_int,
                        entry.target,
                        inode::kind_mode(entry.kind) as u16,
                    );
                }
            }
            last = entry.key;
            emitted += 1;
        }
        if ended || !full {
            break;
        }
        resume = true;
    }
    if ended && whole {
        // SAFETY: As above.
        unsafe { glue::carryfs_set_dir_primed(inode) };
    }
    if emitted == 0 {
        return Ok(());
    }
    // SAFETY: As above.
    let pos = unsafe { (*ctx).pos };
    match cursor {
        Some(cursor) => {
            cursor.pos = pos;
            cursor.key = last;
            cursor.priming_whole = whole;
        }
        None => {
            let cursor = KBox::new(
                Cursor {
                    pos,
                    key: last,
                    priming_whole: whole,
                },
                GFP_KERNEL,
            )?;
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
        fsync: Some(crate::write::fsync),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
