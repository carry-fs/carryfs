// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Buffered writes, writeback, and size changes (FORMAT.md section 5,
//! "Read, write, and truncate rules").
//!
//! Writeback copies each dirty 64 KiB unit whole, filling clean pages from
//! the page cache or the verified old unit, writes it to new blocks, and
//! then replaces the unit's extents and checksum under the volume lock. The
//! old blocks are retired. A unit is always rewritten whole: FORMAT.md lets
//! a writer keep unchanged blocks, but does not require it.
//!
//! The volume lock is held to allocate and to record, not while the data is
//! written; commits wait for writes in between (`State::writing`), so no
//! publication sees blocks allocated but unowned. Writeback returns once
//! the writes are submitted, and the I/O workqueue records them
//! (`carryfs_write_done`). A claim on the object keeps one writer or size
//! change per file at a time, so its writes record in order.

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::page::Page;
use kernel::prelude::*;

use crate::edit::Disk;
use crate::file::{read_unit, Unit, UNIT_BLOCKS};
use crate::glue;
use crate::namei::{
    edit, mutate, now, read_record, set_time, set_u64, txn, write_record, ALLOCATED,
};
use crate::ondisk::crc::{DataUnitCrc, UNIT_BYTES};
use crate::ondisk::format::{
    checksum_slot, compact_group, set_checksum_slot, TreeKind, BLOCK_BYTES, CHECKSUM_GROUP_BYTES,
    MAINTENANCE_RESERVE,
};
use crate::tree::{self, be_u64, le_u64, Tree};
use crate::vfs::{Folio, Inode};
use crate::volume::{self, State};
use crate::{commit, errno, reserve};

/// Units one hold of the volume lock writes: 4 MiB, enough to keep a
/// device busy between waits.
const BATCH: usize = 64;
const SECTORS_PER_BLOCK: u64 = (BLOCK_BYTES / 512) as u64;
/// Free blocks a new reservation, or a write without one, must leave.
pub(crate) const HEADROOM: u64 = MAINTENANCE_RESERVE + 8_192;

/// A unit being written: its content and the folios under writeback for it.
struct Pending {
    index: u64,
    pages: KVec<Page>,
    present: u16,
    folios: KVec<*mut bindings::folio>,
}

/// The size field of an inode record.
fn record_size(record: &[u8]) -> u64 {
    le_u64(record, 16)
}

/// Copy a folio into a bounce page.
fn copy_folio(folio: &Folio, page: &Page) {
    folio.with_bytes(|source| page_bytes(page).copy_from_slice(source));
}

fn page_bytes(page: &Page) -> &mut [u8] {
    // SAFETY: The page is private to the writer and holds `BLOCK_BYTES`.
    unsafe {
        core::slice::from_raw_parts_mut(
            glue::carryfs_page_address(page.as_ptr()).cast::<u8>(),
            BLOCK_BYTES,
        )
    }
}

/// Collects dirty units of one file and writes them in batches.
struct Writer<'a> {
    inode: &'a Inode,
    sb: *mut bindings::super_block,
    mapping: *mut bindings::address_space,
    object: u64,
    units: KVec<Pending>,
    /// Truncate's tail rewrite, the one write that may lower the stored size.
    truncate: bool,
    /// Writeback: return once the writes are submitted.
    asynchronous: bool,
}

impl<'a> Writer<'a> {
    fn new(inode: &'a Inode) -> Self {
        Self {
            inode,
            sb: inode.super_block().as_ptr(),
            mapping: inode.mapping(),
            object: inode.object(),
            units: KVec::new(),
            truncate: false,
            asynchronous: false,
        }
    }

    /// The pending unit `index`, added in order if new.
    fn pending(&mut self, index: u64) -> Result<&mut Pending> {
        let position = self.units.iter().position(|unit| unit.index >= index);
        let position = match position {
            Some(position) if self.units[position].index == index => position,
            position => {
                let mut pages = KVec::with_capacity(UNIT_BLOCKS as usize, GFP_KERNEL)?;
                for _ in 0..UNIT_BLOCKS {
                    pages.push(Page::alloc_page(GFP_KERNEL | __GFP_ZERO)?, GFP_KERNEL)?;
                }
                let unit = Pending {
                    index,
                    pages,
                    present: 0,
                    folios: KVec::new(),
                };
                let position = position.unwrap_or(self.units.len());
                self.units.reserve(1, GFP_KERNEL)?;
                self.units.insert_within_capacity(position, unit)?;
                position
            }
        };
        Ok(&mut self.units[position])
    }

    /// Add the stored partial tail unit when the file grew past it: its
    /// valid length, and so its checksum and block count, change with the
    /// size (FORMAT.md section 5, "Growing a file").
    fn add_tail(&mut self) -> Result {
        let stored = {
            let state = volume::of(self.sb).lock();
            record_size(&read_record(self.sb, &state, self.object)?)
        };
        let size = self.inode.size();
        if size > stored && stored % UNIT_BYTES as u64 != 0 {
            self.pending(stored / UNIT_BYTES as u64)?;
        }
        Ok(())
    }

    /// Take a dirty folio from writeback: copy it, start its writeback, and
    /// unlock it.
    fn add_folio(&mut self, folio: &Folio) -> Result {
        let block = folio.pos() / BLOCK_BYTES as u64;
        let unit = self.pending(block / UNIT_BLOCKS)?;
        let slot = (block % UNIT_BLOCKS) as usize;
        copy_folio(folio, &unit.pages[slot]);
        unit.present |= 1 << slot;
        unit.folios.push(folio.as_ptr(), GFP_KERNEL)?;
        // SAFETY: Writeback handed the folio over locked; its writeback
        // ends after the batch is written, in `flush`.
        unsafe { glue::carryfs_folio_start_writeback(folio.as_ptr()) };
        Ok(())
    }

    /// Fill each unit's missing pages: uptodate page-cache pages first, then
    /// the old unit, read and verified, then zeros. `held` is the block of a
    /// folio the caller holds locked, which is still dirty: it is filled from
    /// the old unit, and written with its own unit later.
    fn fill(&mut self, held: Option<u64>) -> Result {
        let source_size = {
            let state = volume::of(self.sb).lock();
            record_size(&read_record(self.sb, &state, self.object)?)
        };
        for unit in self.units.iter_mut() {
            let mut old: Option<Unit> = None;
            for slot in 0..UNIT_BLOCKS as usize {
                if unit.present & (1 << slot) != 0 {
                    continue;
                }
                let block = unit.index * UNIT_BLOCKS + slot as u64;
                let folio = if held == Some(block) {
                    core::ptr::null_mut()
                } else {
                    // SAFETY: The mapping belongs to a referenced inode.
                    unsafe { glue::carryfs_cached_folio(self.mapping, block) }
                };
                if !folio.is_null() {
                    // SAFETY: `carryfs_cached_folio` returned it locked.
                    copy_folio(unsafe { Folio::from_raw(folio) }, &unit.pages[slot]);
                    // SAFETY: `carryfs_cached_folio` locked and referenced it.
                    unsafe { glue::carryfs_folio_unlock_put(folio) };
                } else if block * (BLOCK_BYTES as u64) < source_size {
                    if old.is_none() {
                        old = Some(read_unit(self.sb, self.object, source_size, unit.index)?);
                    }
                    if let Some(old) = &old {
                        old.copy_into(slot, page_bytes(&unit.pages[slot]));
                    }
                } else {
                    page_bytes(&unit.pages[slot]).fill(0);
                }
                unit.present |= 1 << slot;
            }
        }
        Ok(())
    }

    /// Write the collected units and record them, then end their folios'
    /// writeback, now or, for writeback, once the writes complete. Returns
    /// the error to report on the mapping, if any. `held` is as for `fill`.
    fn flush(&mut self, held: Option<u64>) -> Result {
        if self.units.is_empty() {
            return Ok(());
        }
        volume::of(self.sb).lock().claim(self.object);
        // A size change between reading the tail and allocating retries.
        let mut result = Err(EAGAIN);
        let mut allocation = None;
        for _ in 0..3 {
            result = self
                .add_tail()
                .and_then(|()| self.fill(held))
                .and_then(|()| self.allocate())
                .map(|allocated| allocation = Some(allocated));
            if result != Err(EAGAIN) {
                break;
            }
        }
        let Some(allocation) = allocation else {
            return complete(self.inode, &mut self.units, None, result);
        };
        if self.asynchronous {
            if let Ok(mut inflight) = KBox::new(
                Inflight {
                    inode: self.inode.as_ptr(),
                    units: KVec::new(),
                    allocation: None,
                },
                GFP_KERNEL,
            ) {
                inflight.units = core::mem::take(&mut self.units);
                let bio = queue_units(self.sb, &inflight.units, &allocation);
                inflight.allocation = Some(allocation);
                if bio.is_null() {
                    return KBox::into_inner(inflight).complete(Ok(()));
                }
                let raw = KBox::into_raw(inflight);
                // SAFETY: `bio` is the last of the chain, not yet submitted;
                // the batch lives until `carryfs_write_done` takes it back.
                if unsafe { glue::carryfs_submit_write(bio, raw.cast()) } == 0 {
                    return Ok(());
                }
                // SAFETY: Not submitted: the batch is still ours.
                let inflight = KBox::into_inner(unsafe { KBox::from_raw(raw) });
                // SAFETY: As above.
                let written = unsafe { wait(bio) };
                return inflight.complete(written);
            }
        }
        let bio = queue_units(self.sb, &self.units, &allocation);
        // SAFETY: `bio` is null or the last of the chain, not yet submitted.
        let written = if bio.is_null() { Ok(()) } else { unsafe { wait(bio) } };
        complete(self.inode, &mut self.units, Some(allocation), written)
    }

    /// Allocate blocks for the collected units, under the volume lock. Until
    /// `complete` records or releases them, `State::writing` holds commits
    /// back, so no publication sees blocks allocated but unowned.
    fn allocate(&mut self) -> Result<Allocation> {
        let (sb, object, inode, truncate) = (self.sb, self.object, self.inode, self.truncate);
        // A reclaiming commit may wait for writes in flight if reclaim
        // re-entered the filesystem.
        let _nofs = volume::NoFs::new();
        let units = &self.units;
        // Dirty units reserved their blocks, so writeback may use the
        // reserve kept for metadata edits rather than lose accepted data.
        // The stored partial tail it rewrites when a file grew has no
        // reservation of its own; the headroom reservations leave covers
        // it. A truncate's rewrite has none and keeps the headroom.
        let keep = if truncate { HEADROOM } else { MAINTENANCE_RESERVE };
        let spare = |state: &State, needed: u64| {
            let free = reserve::available_to(state, object, units.iter().map(|unit| unit.index));
            free >= needed + keep
        };
        {
            // Retired data may still be reclaimable: commit to reclaim it,
            // as `write_begin` does, rather than fail writeback with ENOSPC.
            let size = inode.size();
            let needed: u64 = units.iter().map(|unit| unit_blocks(size, unit.index)).sum();
            let mut state = volume::of(sb).lock();
            while !spare(&state, needed) && state.reclaimable() {
                commit::commit(sb, &mut state)?;
            }
        }
        let allocation = mutate(sb, |state, disk| {
            let stored = record_size(&read_record(sb, state, object)?);
            // A smaller i_size is a truncate in progress, or one whose tail
            // rewrite failed: extents past it remain until truncate removes
            // them, so keep the stored size (fstests generic/476).
            let size = if truncate {
                inode.size()
            } else {
                inode.size().max(stored)
            };
            if size > stored
                && stored % UNIT_BYTES as u64 != 0
                && !units
                    .iter()
                    .any(|unit| unit.index == stored / UNIT_BYTES as u64)
            {
                // The tail moved since `add_tail`; retry before any edit.
                return Err(EAGAIN);
            }
            let needed: u64 = units.iter().map(|unit| unit_blocks(size, unit.index)).sum();
            if !spare(state, needed) {
                return Err(ENOSPC);
            }
            let mut runs: KVec<(usize, u64, u64)> = KVec::new();
            for (position, unit) in units.iter().enumerate() {
                let blocks = unit_blocks(size, unit.index);
                let mut done = 0;
                while done < blocks {
                    runs.reserve(1, GFP_KERNEL).or_else(|error| {
                        release(state, disk, &runs)?;
                        Err(Error::from(error))
                    })?;
                    match txn(state).allocate_data(disk, blocks - done) {
                        Ok((first, count)) => {
                            runs.push_within_capacity((position, first, count))
                                .map_err(|_| ENOMEM)?;
                            done += count;
                        }
                        Err(error) => {
                            release(state, disk, &runs)?;
                            return Err(error);
                        }
                    }
                }
            }
            // The allocation takes over the reservations of units with no
            // folio dirty again, so the blocks are not counted twice while
            // the data is written.
            state.reservations.release_clean(object, |unit| {
                units.binary_search_by_key(&unit, |pending| pending.index).is_ok()
            });
            state.writing += 1;
            Ok(Allocation { size, stored, runs })
        })?;
        Ok(allocation)
    }
}

/// Blocks allocated for a writer's units, for a file of `size` bytes whose
/// record held `stored`.
struct Allocation {
    size: u64,
    stored: u64,
    /// Unit position, first block, and count.
    runs: KVec<(usize, u64, u64)>,
}

/// A writeback's units whose writes are in flight; `carryfs_write_done`
/// completes them.
struct Inflight {
    /// Live while its folios are under writeback: eviction waits for them.
    inode: *mut bindings::inode,
    units: KVec<Pending>,
    allocation: Option<Allocation>,
}

impl Inflight {
    fn complete(mut self, written: Result) -> Result {
        // SAFETY: The folios under writeback keep the inode alive.
        let inode = unsafe { Inode::from_raw(self.inode) };
        complete(inode, &mut self.units, self.allocation.take(), written)
    }
}

/// Completion of a write submitted by `Writer::flush`, on the I/O
/// workqueue. Errors went to the mapping, for fsync.
#[no_mangle]
unsafe extern "C" fn carryfs_write_done(batch: *mut core::ffi::c_void, status: c_int) {
    // SAFETY: `flush` passed a batch it leaked for this call.
    let inflight = KBox::into_inner(unsafe { KBox::from_raw(batch.cast::<Inflight>()) });
    let written = if status == 0 { Ok(()) } else { Err(EIO) };
    let _ = inflight.complete(written);
}

/// Record written units, or release their blocks if the write failed;
/// release their reservations and the object's claim; then end their
/// folios' writeback, setting the mapping's error if dirty data was lost.
/// Without `allocation`, `written` is why nothing was written.
fn complete(
    inode: &Inode,
    units: &mut KVec<Pending>,
    allocation: Option<Allocation>,
    written: Result,
) -> Result {
    let (sb, object) = (inode.super_block().as_ptr(), inode.object());
    let mut state = volume::of(sb).lock();
    let result = match &allocation {
        Some(allocation) => {
            let result = edit(sb, &mut state, |state, disk| {
                record(sb, state, disk, inode, units, allocation, written)
            });
            state.end_write();
            result.map(Some)
        }
        None => written.map(|()| None),
    };
    // Recorded or lost, the units' taken folios are clean now.
    state.reservations.release_clean(object, |unit| {
        units.binary_search_by_key(&unit, |pending| pending.index).is_ok()
    });
    state.unclaim(object);
    drop(state);
    if let Ok(Some(allocated)) = result {
        inode.set_blocks(allocated);
    }
    // Only lost dirty folios are the mapping's error, for a later fsync;
    // truncate's tail rewrite has none and reports to its caller. Set it
    // while the folios under writeback still keep the inode alive.
    let dirty = units.iter().any(|unit| !unit.folios.is_empty());
    if let (Err(error), true) = (result, dirty) {
        // SAFETY: As above.
        unsafe { glue::carryfs_mapping_error(inode.mapping(), error.to_errno()) };
    }
    for unit in units.iter() {
        for folio in unit.folios.iter() {
            // SAFETY: `add_folio` started this folio's writeback. The inode
            // may go once the last ends.
            unsafe { bindings::folio_end_writeback(*folio) };
        }
    }
    units.clear();
    result.map(drop)
}

/// Record the written units: extents, checksums, then the inode; the
/// blocks the file then has. A failed write releases its blocks.
fn record(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    inode: &Inode,
    units: &[Pending],
    allocation: &Allocation,
    written: Result,
) -> Result<u64> {
    let (object, size, runs) = (inode.object(), allocation.size, &allocation.runs);
    if let Err(error) = written {
        pr_err!("data write failed in object {object}\n");
        release(state, disk, runs)?;
        return Err(error);
    }
    let mut record = read_record(sb, state, object)?;
    if record_size(&record) != allocation.stored {
        // Claims order an object's writes and size changes; this would be
        // a bug, so fail rather than record units sized for another size.
        pr_err!("size of object {object} changed during a data write\n");
        release(state, disk, runs)?;
        return Err(EIO);
    }
    let uuid = state.root.uuid;
    let mut allocated = le_u64(&record, ALLOCATED);
    for (position, unit) in units.iter().enumerate() {
        let valid = unit_valid(size, unit.index);
        let first_block = unit.index * UNIT_BLOCKS;
        allocated -= remove_extents(sb, state, disk, object, first_block, first_block + UNIT_BLOCKS)?;
        if valid == 0 {
            set_checksum(sb, state, disk, object, unit.index, None)?;
            continue;
        }
        let mut logical = first_block;
        for (_, first, count) in runs.iter().filter(|(owner, _, _)| *owner == position) {
            put_extent(sb, state, disk, object, logical, *first, *count)?;
            logical += count;
            allocated += count;
        }
        let mut crc = DataUnitCrc::new(&uuid, object, unit.index, valid).map_err(errno)?;
        for (index, page) in unit
            .pages
            .iter()
            .enumerate()
            .take(valid.div_ceil(BLOCK_BYTES))
        {
            let len = (valid - index * BLOCK_BYTES).min(BLOCK_BYTES);
            crc.update(&page_bytes(page)[..len]);
        }
        let crc = crc.finish().map_err(errno)?;
        set_checksum(sb, state, disk, object, unit.index, Some(crc))?;
    }
    set_u64(&mut record, 16, size);
    set_u64(&mut record, ALLOCATED, allocated);
    // Writes stamped the inode's times.
    let (modified, changed) = inode.times();
    set_time(&mut record, 56, modified);
    set_time(&mut record, 72, changed);
    write_record(state, disk, object, &record)?;
    Ok(allocated)
}

/// Queue writes of the units to the runs allocated for them, plugged so the
/// block layer merges adjacent units; the last bio of the chain, not yet
/// submitted, or null if there is nothing to write.
fn queue_units(
    sb: *mut bindings::super_block,
    units: &[Pending],
    allocation: &Allocation,
) -> *mut bindings::bio {
    let (size, runs) = (allocation.size, &allocation.runs);
    for unit in units {
        // Zero the last block past the unit's valid bytes.
        if let Some(last) = unit_blocks(size, unit.index).checked_sub(1) {
            let valid = unit_valid(size, unit.index);
            page_bytes(&unit.pages[last as usize])[valid - last as usize * BLOCK_BYTES..].fill(0);
        }
    }
    // SAFETY: Finished below, on this task.
    let plug = unsafe { glue::carryfs_plug_start() };
    let mut bio: *mut bindings::bio = core::ptr::null_mut();
    // Each unit's runs are consecutive; `done` counts its blocks queued.
    let mut done = 0;
    for (index, (position, first, count)) in runs.iter().copied().enumerate() {
        if index > 0 && runs[index - 1].0 != position {
            done = 0;
        }
        // SAFETY: `sb` holds its block device.
        unsafe { glue::carryfs_clean_aliases(sb, first, count) };
        let pages = &units[position].pages[done as usize..(done + count) as usize];
        bio = write_run(sb, bio, first, pages);
        done += count;
    }
    // SAFETY: Started above.
    unsafe { glue::carryfs_plug_finish(plug) };
    bio
}

/// Submit `bio`, the last of a chain, and wait for the chain.
///
/// # Safety
///
/// `bio` must be the unsubmitted last bio of a chain whose pages outlive
/// the call.
unsafe fn wait(bio: *mut bindings::bio) -> Result {
    // SAFETY: As the caller promised.
    let status = unsafe {
        let status = bindings::submit_bio_wait(bio);
        bindings::bio_put(bio);
        status
    };
    if status != 0 {
        return Err(EIO);
    }
    Ok(())
}

/// Bytes of unit `index` within a file of `size` bytes.
fn unit_valid(size: u64, index: u64) -> usize {
    size.saturating_sub(index * UNIT_BYTES as u64)
        .min(UNIT_BYTES as u64) as usize
}

/// Blocks unit `index` needs within a file of `size` bytes.
fn unit_blocks(size: u64, index: u64) -> u64 {
    unit_valid(size, index).div_ceil(BLOCK_BYTES) as u64
}

/// Queue a write of `pages` to the blocks from `first`, chained after
/// `previous`, which is submitted now; returns the new last bio.
fn write_run(
    sb: *mut bindings::super_block,
    previous: *mut bindings::bio,
    first: u64,
    pages: &[Page],
) -> *mut bindings::bio {
    // SAFETY: `sb` holds its block device; GFP_KERNEL allocation does not fail.
    let bio =
        unsafe { glue::carryfs_bio_alloc_write(sb, first * SECTORS_PER_BLOCK, pages.len() as u32) };
    for page in pages {
        // SAFETY: The bio was sized for these pages, so each fits.
        let added = unsafe { bindings::bio_add_page(bio, page.as_ptr(), BLOCK_BYTES as u32, 0) };
        debug_assert_eq!(added, BLOCK_BYTES as i32);
    }
    if !previous.is_null() {
        // SAFETY: `bio` completes only after `previous`, submitted here.
        unsafe {
            bindings::bio_chain(previous, bio);
            bindings::submit_bio(previous);
        }
    }
    bio
}

/// Remove `object`'s extents within logical blocks `start..end`, keeping
/// the parts outside and retiring the blocks inside; the blocks removed.
fn remove_extents(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    object: u64,
    start: u64,
    end: u64,
) -> Result<u64> {
    let prefix = tree::prefix(object, tree::EXTENT);
    let mut found: KVec<(u64, u64, u64)> = KVec::new();
    {
        let catalog = Tree::catalog(sb, state);
        let floor_key = tree::numbered_key(object, tree::EXTENT, start);
        catalog.floor(&floor_key, |key, value| {
            if key.starts_with(&prefix) {
                let logical = be_u64(key, 9);
                let count = le_u64(value, 8) & u64::from(u32::MAX);
                if logical + count > start {
                    found.push((logical, le_u64(value, 0), count), GFP_KERNEL)?;
                }
            }
            Ok(())
        })?;
        let from = tree::numbered_key(object, tree::EXTENT, start + 1);
        catalog.scan(&from, &mut |key, value| {
            if !key.starts_with(&prefix) || be_u64(key, 9) >= end {
                return Ok(false);
            }
            found.push(
                (
                    be_u64(key, 9),
                    le_u64(value, 0),
                    le_u64(value, 8) & u64::from(u32::MAX),
                ),
                GFP_KERNEL,
            )?;
            Ok(true)
        })?;
    }
    let mut removed = 0;
    for (logical, physical, count) in found.iter().copied() {
        let overlap_start = logical.max(start);
        let overlap_end = (logical + count).min(end);
        if overlap_start >= overlap_end {
            continue;
        }
        txn(state).delete(
            disk,
            TreeKind::Catalog,
            &tree::numbered_key(object, tree::EXTENT, logical),
        )?;
        txn(state).retire(
            disk,
            physical + (overlap_start - logical),
            overlap_end - overlap_start,
        )?;
        removed += overlap_end - overlap_start;
        if logical < start {
            put_raw_extent(state, disk, object, logical, physical, start - logical)?;
        }
        if logical + count > end {
            put_raw_extent(
                state,
                disk,
                object,
                end,
                physical + (end - logical),
                logical + count - end,
            )?;
        }
    }
    Ok(removed)
}

fn put_raw_extent(
    state: &mut State,
    disk: &Disk,
    object: u64,
    logical: u64,
    physical: u64,
    count: u64,
) -> Result {
    let mut value = [0u8; 16];
    value[..8].copy_from_slice(&physical.to_le_bytes());
    value[8..12].copy_from_slice(&(count as u32).to_le_bytes());
    txn(state).put(
        disk,
        TreeKind::Catalog,
        &tree::numbered_key(object, tree::EXTENT, logical),
        &value,
    )
}

/// Map `count` blocks from `logical` to `physical`, extending the extent
/// that ends just before when both runs continue it.
fn put_extent(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    object: u64,
    logical: u64,
    physical: u64,
    count: u64,
) -> Result {
    let prefix = tree::prefix(object, tree::EXTENT);
    let before = Tree::catalog(sb, state)
        .floor(
            &tree::numbered_key(object, tree::EXTENT, logical),
            |key, value| {
                Ok(key.starts_with(&prefix).then(|| {
                    (
                        be_u64(key, 9),
                        le_u64(value, 0),
                        le_u64(value, 8) & u64::from(u32::MAX),
                    )
                }))
            },
        )?
        .flatten();
    if let Some((start, first, length)) = before {
        if start + length == logical
            && first + length == physical
            && length + count <= u64::from(u32::MAX)
        {
            return put_raw_extent(state, disk, object, start, first, length + count);
        }
    }
    put_raw_extent(state, disk, object, logical, physical, count)
}

/// Set or clear the checksum slot of `unit`.
fn set_checksum(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    object: u64,
    unit: u64,
    crc: Option<u32>,
) -> Result {
    let key = tree::numbered_key(object, tree::CHECKSUMS, unit / 256);
    let mut group = KBox::new([0u8; CHECKSUM_GROUP_BYTES], GFP_KERNEL)?;
    Tree::catalog(sb, state).get(&key, |value| {
        let len = value.len().min(CHECKSUM_GROUP_BYTES);
        group[..len].copy_from_slice(&value[..len]);
        Ok(())
    })?;
    set_checksum_slot(&mut group, (unit % 256) as usize, crc);
    let stored = compact_group(&group);
    if stored.len() > 32 {
        txn(state).put(disk, TreeKind::Catalog, &key, stored)
    } else {
        txn(state).delete(disk, TreeKind::Catalog, &key).map(drop)
    }
}

pub(crate) unsafe extern "C" fn writepages(
    mapping: *mut bindings::address_space,
    wbc: *mut bindings::writeback_control,
) -> c_int {
    // Reclaim must not re-enter this filesystem from writeback.
    let _nofs = crate::volume::NoFs::new();
    // SAFETY: The VFS passes a mapping of a referenced regular file.
    let inode = unsafe { Inode::from_raw(glue::carryfs_mapping_host(mapping)) };
    // A unit's write locks its clean folios while the dirty ones it took
    // are under writeback. A second writeback of the same file, which
    // fsync and the flusher can start together, may hold one of those
    // locks while waiting for that writeback in WB_SYNC_ALL mode: a
    // deadlock (fstests generic/127). One writeback at a time cannot.
    // serializes writeback of all files on the volume; per-inode
    // locks, or unit-sized folios as LBS XFS uses, if parallel writeback
    // matters.
    let _writeback = inode.super_block().volume().writeback.lock();
    let mut writer = Writer::new(inode);
    writer.asynchronous = true;
    let mut error: c_int = 0;
    let mut folio = core::ptr::null_mut();
    loop {
        // SAFETY: `writeback_iter` returns each dirty folio locked.
        folio = unsafe { glue::carryfs_writeback_iter(mapping, wbc, folio, &mut error) };
        if folio.is_null() {
            break;
        }
        // SAFETY: As above.
        let locked = unsafe { Folio::from_raw(folio) };
        let unit = locked.pos() / BLOCK_BYTES as u64 / UNIT_BLOCKS;
        if writer.units.last().is_some_and(|last| last.index != unit) && writer.units.len() == BATCH
        {
            // The folio may lie in the stored tail unit, which `flush` adds.
            let held = locked.pos() / BLOCK_BYTES as u64;
            if let Err(failure) = writer.flush(Some(held)) {
                error = failure.to_errno();
            }
        }
        if let Err(failure) = writer.add_folio(locked) {
            // SAFETY: The folio is still locked; leave it dirty for later.
            unsafe {
                glue::carryfs_folio_redirty(wbc, folio);
                bindings::folio_unlock(folio);
            }
            error = failure.to_errno();
        }
    }
    if let Err(failure) = writer.flush(None) {
        error = failure.to_errno();
    }
    error
}

pub(crate) unsafe extern "C" fn write_begin(
    _iocb: *const bindings::kiocb,
    mapping: *mut bindings::address_space,
    pos: i64,
    len: u32,
    foliop: *mut *mut bindings::folio,
    _fsdata: *mut *mut core::ffi::c_void,
) -> c_int {
    // SAFETY: The VFS passes the mapping of a file it holds locked.
    let inode = unsafe { glue::carryfs_mapping_host(mapping) };
    // SAFETY: As above.
    let sb = unsafe { (*inode).i_sb };
    // SAFETY: As above.
    let host = unsafe { Inode::from_raw(inode) };
    let mut retried = false;
    let folio = loop {
        let size = host.size();
        if pos as u64 > size {
            zero_past_eof(host);
        }
        // SAFETY: As above.
        let folio = unsafe { glue::carryfs_write_folio(mapping, pos) };
        if let Err(error) = kernel::error::from_err_ptr(folio) {
            return error.to_errno();
        }
        // Reserve with the folio locked: until the write dirties it, the
        // lock keeps writeback from releasing the reservation (reserve.rs).
        let (unit, tail) = (pos as u64 / UNIT_BYTES as u64, size / UNIT_BYTES as u64);
        let units = [unit, tail];
        // Writing past a partial last unit rewrites that unit too (`add_tail`).
        let units = if unit > tail && size % UNIT_BYTES as u64 != 0 {
            &units[..]
        } else {
            &units[..1]
        };
        // A write into the unit this file last reserved, still held, skips
        // the volume lock (glue.c's reserved hint).
        // SAFETY: The folio is locked, which keeps a held unit's reservation.
        let hinted = units.len() == 1
            && !volume::of(sb).faulted()
            && unsafe { glue::carryfs_reserved_hint(inode, unit) };
        let reserved = if hinted {
            Ok(())
        } else {
            let mut state = volume::of(sb).lock();
            if state.faulted {
                Err(EIO)
            } else {
                reserve::reserve(sb, &mut state, host, units).inspect(|()| {
                    if units.len() == 1 {
                        // SAFETY: Under the volume lock, which releases take.
                        unsafe { glue::carryfs_set_reserved_hint(inode, unit) };
                    }
                })
            }
        };
        match reserved {
            Ok(()) => break folio,
            Err(error) => {
                // SAFETY: Release what `carryfs_write_folio` took.
                unsafe { glue::carryfs_folio_unlock_put(folio) };
                if error != ENOSPC || retried {
                    return error.to_errno();
                }
            }
        }
        // A unit written back while it was being filled holds both its new
        // blocks and its reservation until written again. Write back dirty
        // data, which settles that, and retry once, as btrfs flushes
        // delalloc and ext4 retries allocation before reporting ENOSPC.
        retried = true;
        // SAFETY: `sb` is the super block of a file being written; no folio
        // is held.
        unsafe { glue::carryfs_write_dirty_inodes(sb) };
    };
    // A partial write into a page not in the cache reads its old content.
    // SAFETY: The folio is locked and referenced.
    if !unsafe { glue::carryfs_folio_uptodate(folio) } && len as usize != BLOCK_BYTES {
        // SAFETY: The VFS holds the file's inode and the new folio locked.
        let filled =
            unsafe { crate::file::fill_folio(Inode::from_raw(inode), Folio::from_raw(folio)) };
        if let Err(error) = filled {
            // SAFETY: Release what `carryfs_write_folio` took.
            unsafe { glue::carryfs_folio_unlock_put(folio) };
            return error.to_errno();
        }
        // SAFETY: As above.
        unsafe { glue::carryfs_folio_mark_uptodate(folio) };
    }
    // SAFETY: The VFS passes where to return the folio.
    unsafe { *foliop = folio };
    0
}

/// Free the runs a failed `publish` allocated: no record refers to them,
/// and the transaction would otherwise commit them as owned (fstests
/// generic/441).
fn release(state: &mut State, disk: &Disk, runs: &[(usize, u64, u64)]) -> Result {
    for (_, first, count) in runs {
        txn(state).retire(disk, *first, *count)?;
    }
    Ok(())
}

/// Zero the cached last page past the size, before the size grows over it:
/// a shared mapping may have written there (fstests generic/363). The disk
/// needs nothing, as reads stop at a unit's valid length.
fn zero_past_eof(inode: &Inode) {
    let size = inode.size();
    let offset = (size % BLOCK_BYTES as u64) as usize;
    if offset == 0 {
        return;
    }
    // SAFETY: The mapping belongs to a referenced inode.
    let folio = unsafe { glue::carryfs_cached_folio(inode.mapping(), size / BLOCK_BYTES as u64) };
    if folio.is_null() {
        return;
    }
    // SAFETY: `carryfs_cached_folio` returned it locked and referenced.
    unsafe {
        Folio::from_raw(folio).with_bytes(|bytes| bytes[offset..].fill(0));
        glue::carryfs_folio_unlock_put(folio);
    }
}

/// Change a regular file's size under the inode lock, as truncate and
/// `setattr` do.
pub(crate) fn set_size(inode: &Inode, size: u64) -> Result {
    let (sb, object, mapping) = (
        inode.super_block().as_ptr(),
        inode.object(),
        inode.mapping(),
    );
    // Write dirty pages first, so the catalog holds all data to keep.
    // SAFETY: As above.
    kernel::error::to_result(unsafe {
        bindings::filemap_write_and_wait_range(mapping, 0, i64::MAX)
    })?;
    if size > inode.size() {
        zero_past_eof(inode);
    }
    // SAFETY: As above; drops pages past a smaller size and zeroes the
    // partial last page, waiting for their writeback.
    unsafe { bindings::truncate_setsize(inode.as_ptr(), size as i64) };
    // Claimed after truncating the page cache, which waits for writebacks
    // that may be waiting for the claim.
    let old = {
        let mut state = volume::of(sb).lock();
        state.claim(object);
        state
            .reservations
            .release_from(object, size.div_ceil(UNIT_BYTES as u64));
        read_record(sb, &state, object).map(|record| record_size(&record))
    };
    let changed = old.and_then(|old| change_size(inode, old, size).map(|()| old));
    volume::of(sb).lock().unclaim(object);
    let old = changed?;
    // The tail unit's valid length changes: rewrite it whole, which also
    // records the new size. Without a partial tail only the size changes.
    let tail = (size % UNIT_BYTES as u64 != 0).then_some(size / UNIT_BYTES as u64);
    let old_tail = (old % UNIT_BYTES as u64 != 0).then_some(old / UNIT_BYTES as u64);
    let rewrite = if size < old { tail } else { old_tail };
    if let (Some(unit), false) = (rewrite, size == old) {
        let mut writer = Writer::new(inode);
        writer.truncate = true;
        writer.pending(unit)?;
        return writer.flush(None);
    }
    Ok(())
}

/// Shrink `inode`'s extents to `size`, or record a size without a partial
/// tail, holding its claim. The tail rewrite follows.
fn change_size(inode: &Inode, old: u64, size: u64) -> Result {
    let (sb, object) = (inode.super_block().as_ptr(), inode.object());
    if size < old {
        // Shrink in unit-aligned steps from the end, each a consistent state.
        let keep = size.div_ceil(UNIT_BYTES as u64) * UNIT_BLOCKS;
        loop {
            let (done, allocated) = mutate(sb, |state, disk| {
                shrink_step(sb, state, disk, object, keep, size)
            })?;
            inode.set_blocks(allocated);
            if done {
                break;
            }
        }
        return Ok(());
    }
    let old_tail = (old % UNIT_BYTES as u64 != 0).then_some(old / UNIT_BYTES as u64);
    if size == old || old_tail.is_some() {
        // Nothing changes, or the tail rewrite records the new size.
        return Ok(());
    }
    mutate(sb, |state, disk| {
        let mut record = read_record(sb, state, object)?;
        set_u64(&mut record, 16, size);
        let time = now();
        set_time(&mut record, 56, time);
        set_time(&mut record, 72, time);
        write_record(state, disk, object, &record)
    })
}

/// Extent records one shrink step removes.
const SHRINK_BATCH: usize = 256;

/// Remove a batch of `object`'s extents at or past logical block `keep`,
/// with their units' checksums; whether none remain past `keep`, and the
/// blocks the file still has. Once none do, the size becomes `size`
/// rounded up to a whole unit, so that the kept
/// tail unit stays consistent until it is rewritten. Each step leaves a
/// valid file: removed units read as holes.
fn shrink_step(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    object: u64,
    keep: u64,
    size: u64,
) -> Result<(bool, u64)> {
    let prefix = tree::prefix(object, tree::EXTENT);
    let mut seen = 0;
    let mut batch_end = u64::MAX / 2;
    Tree::catalog(sb, state).scan(
        &tree::numbered_key(object, tree::EXTENT, keep),
        &mut |key, _| {
            if !key.starts_with(&prefix) {
                return Ok(false);
            }
            if seen == SHRINK_BATCH {
                batch_end = be_u64(key, 9) / UNIT_BLOCKS * UNIT_BLOCKS;
                return Ok(false);
            }
            seen += 1;
            Ok(true)
        },
    )?;
    let done = batch_end == u64::MAX / 2;
    let batch_end = batch_end.max(keep + UNIT_BLOCKS);
    let removed = remove_extents(sb, state, disk, object, keep, batch_end)?;
    clear_checksums(
        sb,
        state,
        disk,
        object,
        keep / UNIT_BLOCKS,
        batch_end / UNIT_BLOCKS,
    )?;
    let mut record = read_record(sb, state, object)?;
    if done {
        let whole_units = size.div_ceil(UNIT_BYTES as u64) * UNIT_BYTES as u64;
        let current = record_size(&record);
        set_u64(&mut record, 16, current.min(whole_units));
    }
    let allocated = le_u64(&record, ALLOCATED).saturating_sub(removed);
    set_u64(&mut record, ALLOCATED, allocated);
    let time = now();
    set_time(&mut record, 56, time);
    set_time(&mut record, 72, time);
    write_record(state, disk, object, &record)?;
    Ok((done, allocated))
}

/// Clear the checksum slots of units `first..end`, dropping emptied groups.
fn clear_checksums(
    sb: *mut bindings::super_block,
    state: &mut State,
    disk: &Disk,
    object: u64,
    first: u64,
    end: u64,
) -> Result {
    let prefix = tree::prefix(object, tree::CHECKSUMS);
    let mut groups: KVec<u64> = KVec::new();
    Tree::catalog(sb, state).scan(
        &tree::numbered_key(object, tree::CHECKSUMS, first / 256),
        &mut |key, _| {
            let group = be_u64(key, 9);
            if !key.starts_with(&prefix) || group * 256 >= end {
                return Ok(false);
            }
            groups.push(group, GFP_KERNEL)?;
            Ok(true)
        },
    )?;
    for group in groups.iter().copied() {
        let key = tree::numbered_key(object, tree::CHECKSUMS, group);
        let mut full = KBox::new([0u8; CHECKSUM_GROUP_BYTES], GFP_KERNEL)?;
        Tree::catalog(sb, state).get(&key, |value| {
            let len = value.len().min(CHECKSUM_GROUP_BYTES);
            full[..len].copy_from_slice(&value[..len]);
            Ok(())
        })?;
        let low = first.max(group * 256) - group * 256;
        let high = end.min((group + 1) * 256) - group * 256;
        for slot in low..high {
            if checksum_slot(&full[..], slot as usize)
                .ok()
                .flatten()
                .is_some()
            {
                set_checksum_slot(&mut full, slot as usize, None);
            }
        }
        let stored = compact_group(&full);
        if stored.len() > 32 {
            txn(state).put(disk, TreeKind::Catalog, &key, stored)?;
        } else {
            txn(state).delete(disk, TreeKind::Catalog, &key)?;
        }
    }
    Ok(())
}

pub(crate) unsafe extern "C" fn fsync(
    file: *mut bindings::file,
    start: i64,
    end: i64,
    _datasync: c_int,
) -> c_int {
    // SAFETY: The VFS passes an open file of this filesystem.
    let status = unsafe { bindings::file_write_and_wait_range(file, start, end) };
    if status != 0 {
        return status;
    }
    // SAFETY: As above.
    let sb = unsafe { (*(*file).f_inode).i_sb };
    let volume = volume::of(sb);
    if !volume.writable || volume.lock().txn.is_none() {
        return 0;
    }
    // A commit publishes the whole volume, so write back every dirty file
    // first: the fsyncs that follow, as after copying many files, then find
    // nothing left to commit. The FUSE adapter publishes all pending changes
    // on fsync too.
    // SAFETY: `sb` is the super block of an open file.
    let status = unsafe { glue::carryfs_write_dirty_inodes(sb) };
    if status != 0 {
        return status;
    }
    match commit::commit(sb, &mut volume.lock()) {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

pub(crate) unsafe extern "C" fn write_end(
    iocb: *const bindings::kiocb,
    mapping: *mut bindings::address_space,
    pos: i64,
    len: u32,
    copied: u32,
    folio: *mut bindings::folio,
    fsdata: *mut core::ffi::c_void,
) -> c_int {
    // SAFETY: The VFS passes what `write_begin` returned.
    unsafe { glue::carryfs_write_end(iocb, mapping, pos, len, copied, folio, fsdata) }
}
