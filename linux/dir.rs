// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Directory iteration.
//!
//! Entries are catalog records keyed by comparison key, so a position is
//! only an ordinal. Each open directory keeps the key of the last entry it
//! returned; reading on from that position resumes after the key, and any
//! other position, after a seek, counts entries from the start.
//!
//! Listing from the catalog also instantiates each entry's dentry, as NFS
//! and CIFS do for readdirplus, because a listing is usually followed by a
//! stat of each entry.
//!
//! A pass that lists a directory whole keeps the result on its inode, as
//! FUSE keeps readdir pages, and later listings read it without the lock
//! or the catalog. Changes to a directory hold its inode lock exclusively
//! and drop the copy; listings hold it shared, so none is using it then.
//! The inode version catches changes between the calls of one pass.

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::glue;
use crate::inode::Ops;
use crate::ondisk::name::validate_disk_name_characters;
use crate::tree::{self, le_u64, Tree};
use crate::vfs::{DirContext, Inode};
use crate::{corrupt, inode};

/// Entries read per hold of the volume lock (FORMAT.md section 6's
/// directory page).
const BATCH: usize = 64;

/// An entry read for emitting: its key and name, consecutive in the
/// batch's byte buffer from `at`.
struct Item {
    at: usize,
    key: usize,
    name: usize,
    target: u64,
    kind: u8,
}

/// Most name bytes a kept listing holds; larger directories are listed
/// from the catalog each time.
const LISTING_BYTES: usize = 2 << 20;

/// A directory's entries in catalog order, kept on its inode.
pub(crate) struct Listing {
    names: KVec<u8>,
    entries: KVec<Kept>,
}

/// One kept entry: its name at `at` in the listing's names.
struct Kept {
    at: u32,
    len: u16,
    kind: u8,
    target: u64,
}

impl Listing {
    fn add(&mut self, name: &[u8], kind: u8, target: u64) -> Result<bool> {
        if self.names.len() + name.len() > LISTING_BYTES {
            return Ok(false);
        }
        let at = self.names.len() as u32;
        tree::append(&mut self.names, name)?;
        self.entries.push(
            Kept {
                at,
                len: name.len() as u16,
                kind,
                target,
            },
            GFP_KERNEL,
        )?;
        Ok(true)
    }
}

/// Drop `dir`'s kept listing. Callers hold its inode lock exclusively, or
/// are evicting it, so no listing is reading the copy.
pub(crate) fn forget(dir: &Inode) {
    let listing = dir.take_listing() as *mut Listing;
    if !listing.is_null() {
        // SAFETY: `keep` published it with `KBox::into_raw`, and taking it
        // made this the only owner.
        drop(unsafe { KBox::from_raw(listing) });
    }
}

/// Keep `listing` on `dir` unless another pass kept one first.
fn keep(dir: &Inode, listing: KBox<Listing>) {
    let listing = KBox::into_raw(listing);
    if !dir.publish_listing(listing.cast()) {
        // SAFETY: Made just above and not published.
        drop(unsafe { KBox::from_raw(listing) });
    }
}

/// Where an open directory stopped: the position after its last entry and
/// that entry's key, and the listing a pass from the first entry is
/// building, with the inode version it began at.
struct Cursor {
    pos: i64,
    key: KVec<u8>,
    building: Option<(KBox<Listing>, u64)>,
}

unsafe extern "C" fn iterate(file: *mut bindings::file, ctx: *mut bindings::dir_context) -> c_int {
    // SAFETY: The VFS passes an open directory of this filesystem and its
    // context. It holds the directory's inode lock shared and the file's
    // position lock, so this call alone uses the context and the cursor,
    // and no change can drop the kept listing while it is read. The open
    // file pins its inode and dentry.
    let (dir, context, parent, kept, mut cursor) = unsafe {
        if !glue::carryfs_dir_emit_dots(file, ctx) {
            return 0;
        }
        let dir = Inode::from_raw((*file).f_inode);
        let kept = (dir.listing() as *const Listing).as_ref();
        let cursor = (*file).private_data as *mut Cursor;
        let cursor = (!cursor.is_null()).then(|| KBox::from_raw(cursor));
        (
            dir,
            DirContext::from_raw(ctx),
            glue::carryfs_file_dentry(file),
            kept,
            cursor,
        )
    };
    let prime = |name: &[u8], target: u64, kind: u8| {
        // SAFETY: The open directory pins its dentry; names are at most
        // 255 bytes.
        unsafe {
            glue::carryfs_prime_dentry(
                parent,
                name.as_ptr(),
                name.len() as c_int,
                target,
                inode::kind_mode(kind) as u16,
            )
        }
    };
    let result = match kept {
        Some(listing) => {
            emit_kept(listing, context);
            Ok(())
        }
        None => list(dir, context, &mut cursor, &prime),
    };
    // SAFETY: As above; `release` frees the cursor with the file.
    unsafe {
        (*file).private_data = cursor.map_or(core::ptr::null_mut(), |cursor| {
            KBox::into_raw(cursor).cast()
        })
    };
    match result {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// Emit a kept listing from the context's position.
fn emit_kept(listing: &Listing, context: &DirContext) {
    let first = usize::try_from(context.pos() - 2).unwrap_or(usize::MAX);
    for entry in listing.entries.iter().skip(first) {
        let name = &listing.names[entry.at as usize..entry.at as usize + entry.len as usize];
        if !context.emit(name, entry.target, entry.kind) {
            break;
        }
    }
}

/// List `dir` from the catalog, from the context's position.
fn list(
    dir: &Inode,
    context: &DirContext,
    cursor: &mut Option<KBox<Cursor>>,
    prime: &dyn Fn(&[u8], u64, u8),
) -> Result {
    let pos = context.pos();
    let prefix = tree::prefix(dir.object(), tree::ENTRY);
    let resuming = cursor.as_deref().is_some_and(|cursor| cursor.pos == pos);
    // A pass from the first entry builds a listing to keep; resuming one
    // continues it.
    let mut building = match cursor.as_deref_mut() {
        Some(cursor) if resuming => cursor.building.take(),
        _ if pos == 2 => Some((
            KBox::new(
                Listing {
                    names: KVec::new(),
                    entries: KVec::new(),
                },
                GFP_KERNEL,
            )?,
            dir.version(),
        )),
        _ => None,
    };
    let (start, resume, mut skip): (&[u8], bool, i64) = match cursor.as_deref() {
        Some(cursor) if resuming => (&cursor.key, true, 0),
        _ => (&prefix, false, pos - 2),
    };
    let mut ended = false;
    // Set when the caller's buffer filled before a batch was emitted whole.
    let mut stopped = false;
    let mut resume = resume;
    let mut last = KVec::new();
    tree::append(&mut last, start)?;
    let mut emitted = 0;
    let sb = dir.super_block();
    let volume = sb.volume();
    // Entries are read under the volume lock in batches, then emitted and
    // primed without it: priming fills inodes, which takes the lock.
    // One buffer for the batch's keys and names, reused across batches.
    let mut bytes: KVec<u8> = KVec::new();
    let mut items: KVec<Item> = KVec::with_capacity(BATCH, GFP_KERNEL)?;
    'listing: loop {
        bytes.clear();
        items.clear();
        {
            let state = volume.lock();
            let catalog = state.root.catalog.block;
            Tree::catalog(sb.as_ptr(), &state).scan(&last, &mut |key, value| {
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
                let at = bytes.len();
                tree::append(&mut bytes, key)?;
                tree::append(&mut bytes, name)?;
                items.push(
                    Item {
                        at,
                        key: key.len(),
                        name: name.len(),
                        target: le_u64(value, 0),
                        kind: value.get(8).copied().unwrap_or(0),
                    },
                    GFP_KERNEL,
                )?;
                Ok(items.len() < BATCH)
            })?;
        }
        let full = items.len() == BATCH;
        let mut emitted_in_batch: usize = 0;
        for entry in items.iter() {
            let name = &bytes[entry.at + entry.key..entry.at + entry.key + entry.name];
            if !context.emit(name, entry.target, entry.kind) {
                stopped = true;
                break;
            }
            prime(name, entry.target, entry.kind);
            if let Some((listing, _)) = &mut building {
                if !listing.add(name, entry.kind, entry.target)? {
                    building = None;
                }
            }
            emitted_in_batch += 1;
            emitted += 1;
        }
        // The last emitted entry's key, copied once per batch for resuming.
        if let Some(entry) = emitted_in_batch
            .checked_sub(1)
            .and_then(|index| items.get(index))
        {
            last.clear();
            tree::append(&mut last, &bytes[entry.at..entry.at + entry.key])?;
        }
        if stopped {
            break 'listing;
        }
        if ended || !full {
            break;
        }
        resume = true;
    }
    if ended && !stopped {
        // Keep the listing once every entry is in it, unless the directory
        // changed while it was built. A pass can reach the end of the
        // directory in a batch the caller's buffer could not take whole.
        if let Some((listing, version)) = building.take() {
            if dir.version_is(version) {
                keep(dir, listing);
            }
        }
    }
    if emitted == 0 && building.is_none() {
        return Ok(());
    }
    let pos = context.pos();
    match cursor {
        Some(cursor) => {
            cursor.pos = pos;
            cursor.key = last;
            cursor.building = building;
        }
        None => {
            *cursor = Some(KBox::new(
                Cursor {
                    pos,
                    key: last,
                    building,
                },
                GFP_KERNEL,
            )?);
        }
    }
    Ok(())
}

unsafe extern "C" fn release(_inode: *mut bindings::inode, file: *mut bindings::file) -> c_int {
    // SAFETY: The last user of the file is closing it.
    let cursor = unsafe { (*file).private_data } as *mut Cursor;
    if !cursor.is_null() {
        // SAFETY: `iterate` made it with `KBox::into_raw`.
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
