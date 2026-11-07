// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Directory iteration.
//!
//! Positions count entries. Each open directory keeps its last emitted key
//! to resume without rescanning. Seeking to another position scans from the start.
//!
//! Listings prime dentries so later stat calls avoid catalog lookups.
//!
//! Complete listings stay on the inode until a change or eviction. Listings
//! hold the inode lock shared; mutations hold it exclusively and drop the copy.
//! The inode version detects changes between calls in one listing pass.

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
    /// Where the target's inode record is in the batch's bytes, if read.
    record: Option<(usize, usize)>,
}

/// Catalog records `read_records` visits per entry before giving up: the
/// records of the objects between the targets it looks for.
const RECORDS_PER_ENTRY: usize = 16;

/// Entries `Ahead` keeps scanned past those the listing emitted: about
/// sixteen leaves.
const AHEAD_ENTRIES: usize = 1024;
/// Entry leaves `Ahead` scans at a time, reading their children's leaves in
/// one merged pass; the next group's entry leaves are read ahead meanwhile.
const AHEAD_LEAVES: usize = 16;

/// Read-ahead for a first listing of a directory, kept with its open file.
///
/// Read entry leaves in groups, then read their targets' inode leaves.
/// This keeps about `AHEAD_ENTRIES` ahead without delaying the first entries
/// behind a whole-directory read. A catalog change invalidates saved positions.
struct Ahead {
    stamp: (u64, u64),
    leaves: KVec<tree::Position>,
    /// Leaves scanned, and leaves whose reads were started.
    scanned: usize,
    issued: usize,
    /// Entries in the scanned leaves, and entries the listing emitted.
    entries: usize,
    emitted: usize,
}

impl Ahead {
    fn new(sb: *mut bindings::super_block, state: &crate::volume::State, prefix: &[u8], dir: u64) -> Result<Self> {
        let leaves = Tree::catalog(sb, state).leaves_in(prefix, &tree::prefix(dir, tree::ENTRY + 1))?;
        Ok(Self {
            stamp: state.stamp(),
            leaves,
            scanned: 0,
            issued: 0,
            entries: 0,
            emitted: 0,
        })
    }

    /// Scan entry leaves until `AHEAD_ENTRIES` past those emitted, a group
    /// of `AHEAD_LEAVES` at a time: read the next group's entry leaves
    /// ahead, then the group's children's leaves.
    fn advance(&mut self, sb: *mut bindings::super_block, state: &crate::volume::State, prefix: &[u8]) -> Result {
        if state.stamp() != self.stamp {
            self.leaves.clear();
        }
        let tree = Tree::catalog(sb, state);
        while self.scanned < self.leaves.len() && self.entries < self.emitted + AHEAD_ENTRIES {
            let group = (self.scanned + AHEAD_LEAVES).min(self.leaves.len());
            let want = (group + AHEAD_LEAVES).min(self.leaves.len());
            if self.issued < want {
                tree.read_ahead_merged(&self.leaves[self.issued..want])?;
                self.issued = want;
            }
            let (mut low, mut high, mut count) = (u64::MAX, 0, 0);
            for at in self.leaves[self.scanned..group].iter() {
                tree.leaf_records(*at, &mut |key, value| {
                    if key.starts_with(prefix) {
                        let target = le_u64(value, 0);
                        (low, high, count) = (low.min(target), high.max(target), count + 1);
                    }
                })?;
            }
            self.scanned = group;
            self.entries += count;
            if low <= high {
                tree.prefetch_range(&low.to_be_bytes(), &high.saturating_add(1).to_be_bytes())?;
            }
        }
        Ok(())
    }
}

/// Copy the inode records of `items`' targets into `bytes`, in one walk in
/// object order rather than a search each, as XFS bulkstat walks inode
/// clusters. Targets created together are neighbors; spread ones stop the
/// walk, and priming searches for those.
fn read_records(
    sb: *mut bindings::super_block,
    state: &crate::volume::State,
    items: &mut [Item],
    bytes: &mut KVec<u8>,
) -> Result {
    // Insertion sort of at most `BATCH` indices, by target.
    let mut order = [0usize; BATCH];
    let order = &mut order[..items.len()];
    for index in 0..order.len() {
        order[index] = index;
        let mut at = index;
        while at > 0 && items[order[at - 1]].target > items[order[at]].target {
            order.swap(at - 1, at);
            at -= 1;
        }
    }
    let Some(&first) = order.first() else {
        return Ok(());
    };
    let start = tree::prefix(items[first].target, tree::INODE);
    let (mut next, mut budget) = (0, order.len() * RECORDS_PER_ENTRY);
    Tree::catalog(sb, state).scan(&start, &mut |key, value| {
        let owner = tree::be_u64(key, 0);
        while next < order.len() && items[order[next]].target < owner {
            next += 1;
        }
        if next == order.len() || budget == 0 {
            return Ok(false);
        }
        budget -= 1;
        if key.len() == 9 && key[8] == tree::INODE && owner == items[order[next]].target {
            let at = bytes.len();
            tree::append(bytes, value)?;
            items[order[next]].record = Some((at, value.len()));
            next += 1;
        }
        Ok(true)
    })
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
    ahead: Option<KBox<Ahead>>,
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
            (*file).__bindgen_anon_1.f_path.dentry,
            kept,
            cursor,
        )
    };
    let prime = |name: &[u8], target: u64, kind: u8, known: Option<&inode::Known<'_>>| {
        let known = known.map_or(core::ptr::null(), |known| core::ptr::from_ref(known).cast());
        // SAFETY: The open directory pins its dentry; names are at most
        // 255 bytes; `known` outlives the call.
        unsafe {
            glue::carryfs_prime_dentry(
                parent,
                name.as_ptr(),
                name.len() as c_int,
                target,
                inode::kind_mode(kind) as u16,
                known,
            )
        }
    };
    let result = match kept {
        Some(listing) => {
            emit_kept(listing, context);
            Ok(())
        }
        None => {
            let _boost = glue::CallerBoost::new();
            list(dir, context, &mut cursor, &prime)
        }
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
    prime: &dyn Fn(&[u8], u64, u8, Option<&inode::Known<'_>>),
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
    let mut ahead = match cursor.as_deref_mut() {
        Some(cursor) if resuming => cursor.ahead.take(),
        _ if pos == 2 && !resume => Some(KBox::new(
            Ahead::new(sb.as_ptr(), &volume.lock(), &prefix, dir.object())?,
            GFP_KERNEL,
        )?),
        _ => None,
    };
    // Entries are read under the volume lock in batches, then emitted and
    // primed without it: priming fills inodes, which takes the lock.
    // One buffer for the batch's keys and names, reused across batches.
    let mut bytes: KVec<u8> = KVec::new();
    let mut items: KVec<Item> = KVec::with_capacity(BATCH, GFP_KERNEL)?;
    'listing: loop {
        bytes.clear();
        items.clear();
        let stamp = {
            let state = volume.lock();
            if let Some(ahead) = ahead.as_deref_mut() {
                ahead.advance(sb.as_ptr(), &state, &prefix)?;
            }
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
                        record: None,
                    },
                    GFP_KERNEL,
                )?;
                Ok(items.len() < BATCH)
            })?;
            read_records(sb.as_ptr(), &state, &mut items, &mut bytes)?;
            state.stamp()
        };
        let full = items.len() == BATCH;
        let mut emitted_in_batch: usize = 0;
        for entry in items.iter() {
            let name = &bytes[entry.at + entry.key..entry.at + entry.key + entry.name];
            if !context.emit(name, entry.target, entry.kind) {
                stopped = true;
                break;
            }
            let known = entry.record.map(|(at, len)| inode::Known {
                stamp,
                record: &bytes[at..at + len],
            });
            prime(name, entry.target, entry.kind, known.as_ref());
            if let Some((listing, _)) = &mut building {
                if !listing.add(name, entry.kind, entry.target)? {
                    building = None;
                }
            }
            emitted_in_batch += 1;
            emitted += 1;
        }
        if let Some(ahead) = ahead.as_deref_mut() {
            ahead.emitted += emitted_in_batch;
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
            cursor.ahead = ahead;
        }
        None => {
            *cursor = Some(KBox::new(
                Cursor {
                    pos,
                    key: last,
                    building,
                    ahead,
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
