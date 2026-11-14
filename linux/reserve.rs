// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Space for dirty page-cache data.
//!
//! Reserve a whole 64 KiB unit before dirtying it, so writes report ENOSPC
//! before accepting data that writeback cannot allocate.
//!
//! `write_begin` and `page_mkwrite` reserve with the folio locked. Dirty or
//! locked folios keep the reservation so an unfinished write cannot lose it.
//! Writeback allocates against its own reservations and releases clean units.
//! Truncation and eviction release reservations past EOF. Sweep idle
//! reservations before reporting ENOSPC, including those left by failed copies.

use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use crate::file::UNIT_BLOCKS;
use crate::glue;
use crate::vfs::Inode;
use crate::volume::{Locked, State};
use crate::{commit, write};

/// One inode's reserved units, sorted.
struct Entry {
    object: u64,
    /// The inode's mapping, as an address so `State` stays `Send`. The inode
    /// lives while it holds reservations: eviction releases them.
    mapping: usize,
    units: KVec<(u64, u64)>,
}

/// Every unit holding a reservation, by inode.
// Sorted vectors make insertion linear in reserved files and units.
#[derive(Default)]
pub(crate) struct Reservations {
    entries: KVec<Entry>,
    /// Blocks reserved, `UNIT_BLOCKS` per unit.
    pub(crate) blocks: u64,
}

impl Reservations {
    fn entry(&self, object: u64) -> core::result::Result<usize, usize> {
        self.entries
            .binary_search_by_key(&object, |entry| entry.object)
    }

    fn holds(&self, object: u64, unit: u64) -> bool {
        self.entry(object)
            .is_ok_and(|at| self.entries[at].units.binary_search_by_key(&unit, |(unit, _)| *unit).is_ok())
    }

    fn add(&mut self, inode: &Inode, unit: u64, blocks: u64) -> Result {
        let at = match self.entry(inode.object()) {
            Ok(at) => at,
            Err(at) => {
                self.entries.reserve(1, GFP_KERNEL)?;
                let entry = Entry {
                    object: inode.object(),
                    mapping: inode.mapping() as usize,
                    units: KVec::new(),
                };
                self.entries.insert_within_capacity(at, entry)?;
                at
            }
        };
        let units = &mut self.entries[at].units;
        if let Err(position) = units.binary_search_by_key(&unit, |(unit, _)| *unit) {
            units.reserve(1, GFP_KERNEL)?;
            units.insert_within_capacity(position, (unit, blocks))?;
            self.blocks += blocks;
        }
        Ok(())
    }

    /// Blocks `object` holds for `units`.
    pub(crate) fn held(&self, object: u64, units: impl Iterator<Item = u64>) -> u64 {
        let Ok(at) = self.entry(object) else { return 0; };
        let held = &self.entries[at].units;
        units.filter_map(|unit| held.binary_search_by_key(&unit, |(unit, _)| *unit).ok().map(|at| held[at].1)).sum()
    }

    /// Release `object`'s units for which `keep` is false.
    fn retain(&mut self, object: u64, mut keep: impl FnMut(usize, u64) -> bool) {
        let Ok(at) = self.entry(object) else {
            return;
        };
        let entry = &mut self.entries[at];
        let mapping = entry.mapping;
        // Before testing which units are busy (glue.c's reserved hint).
        // SAFETY: An inode holding reservations is live, and so is its mapping.
        unsafe {
            glue::carryfs_clear_reserved_hint(
                core::ptr::with_exposed_provenance_mut::<bindings::address_space>(mapping),
            )
        };
        let mut released = 0;
        entry.units.retain(|(unit, blocks)| {
            if keep(mapping, *unit) { true } else { released += *blocks; false }
        });
        self.blocks -= released;
        if entry.units.is_empty() {
            // Drop the entry: move it last, then pop it.
            self.entries[at..].rotate_left(1);
            self.entries.pop();
        }
    }

    /// Release `object`'s units from `first` on: truncate and eviction.
    pub(crate) fn release_from(&mut self, object: u64, first: u64) {
        self.retain(object, |_, unit| unit < first);
    }

    /// Release `object`'s `published` units that are clean and unlocked:
    /// the writeback writing them took their folios.
    pub(crate) fn release_clean(&mut self, object: u64, published: impl Fn(u64) -> bool) {
        self.retain(object, |mapping, unit| {
            !published(unit) || busy(mapping, unit, false)
        });
    }

    /// Release every unit that is clean, unlocked, and not under writeback:
    /// a writeback that took a unit's folios has not allocated it yet.
    fn sweep(&mut self) {
        // Backwards: releasing an inode's last unit drops its entry.
        for at in (0..self.entries.len()).rev() {
            let object = self.entries[at].object;
            self.retain(object, |mapping, unit| busy(mapping, unit, true));
        }
    }
}

/// Whether a folio of the unit is dirty, locked by a write about to dirty
/// it, or, if `writeback`, under writeback.
fn busy(mapping: usize, unit: u64, writeback: bool) -> bool {
    let mapping = core::ptr::with_exposed_provenance_mut::<bindings::address_space>(mapping);
    let (first, last) = (unit * UNIT_BLOCKS, (unit + 1) * UNIT_BLOCKS - 1);
    // SAFETY: An inode holding reservations is live, and so is its mapping.
    unsafe { glue::carryfs_range_busy(mapping, first, last, writeback) }
}

/// Free blocks no reservation holds.
pub(crate) fn available(state: &State) -> u64 {
    state
        .free_blocks()
        .saturating_sub(state.reservations.blocks)
}

/// Free blocks a writeback of `object`'s `units` may take: its own
/// reservations, and none of anyone else's.
pub(crate) fn available_to(state: &State, object: u64, units: impl Iterator<Item = u64>) -> u64 {
    let own = state.reservations.held(object, units);
    (state.free_blocks() + own).saturating_sub(state.reservations.blocks)
}

/// Reserve `units` of `inode` that hold no reservation, with a folio in one
/// of them locked. Waits for data writes in flight, commits to reclaim
/// retired data, then sweeps idle reservations, before failing with ENOSPC.
pub(crate) fn reserve(
    sb: *mut bindings::super_block,
    state: &mut Locked<'_>,
    inode: &Inode,
    units: &[u64],
) -> Result {
    let object = inode.object();
    let mut wanted = KVec::new();
    let mut needed = 0;
    for unit in units {
        if state.reservations.holds(object, *unit) { continue; }
        let first = *unit * UNIT_BLOCKS;
        let features = state.txn.as_ref().map_or(state.root.features, |txn| txn.features);
        let reserved = if features & crate::ondisk::format::FEATURE_PREALLOCATION != 0 {
            write::mapped_blocks(sb, state, object, first, first + UNIT_BLOCKS)?
        } else { 0 };
        let blocks = UNIT_BLOCKS - reserved;
        wanted.push((*unit, blocks), GFP_KERNEL)?;
        needed += blocks;
    }
    if wanted.is_empty() { return Ok(()); }
    let mut swept = false;
    while needed > 0 && available(state) < needed + write::HEADROOM {
        if state.writing > 0 {
            // A unit dirtied again during its data write is counted twice
            // until the write is recorded and its old blocks reclaimed.
            state.wait_for_writes();
        } else if state.reclaimable() {
            commit::commit(sb, state)?;
        } else if !swept {
            state.reservations.sweep();
            swept = true;
        } else {
            return Err(ENOSPC);
        }
    }
    for (unit, blocks) in wanted.iter().copied() {
        state.reservations.add(inode, unit, blocks)?;
    }
    Ok(())
}

/// Reserve the unit of `inode`'s folio at `index`, which `page_mkwrite` in
/// glue.c holds locked before dirtying it through a shared mapping.
#[no_mangle]
unsafe extern "C" fn carryfs_reserve_folio(inode: *mut bindings::inode, index: u64) -> c_int {
    // SAFETY: glue.c passes the referenced inode of a mapped file.
    let inode = unsafe { Inode::from_raw(inode) };
    let sb = inode.super_block().as_ptr();
    let volume = inode.super_block().volume();
    let mut state = volume.lock();
    if state.faulted {
        return EIO.to_errno();
    }
    match reserve(sb, &mut state, inode, &[index / UNIT_BLOCKS]) {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}
