// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Space for dirty page-cache data.
//!
//! Writeback rewrites a whole 64 KiB unit to new blocks, so the write that
//! first dirties a unit reserves the unit's blocks. A write that finds no
//! space fails with ENOSPC, as the FUSE adapter's does, instead of losing
//! its data when writeback cannot allocate.
//!
//! A reservation is taken with the dirtying folio locked, in `write_begin`
//! and `page_mkwrite`, and released only when none of the unit's folios is
//! dirty or locked. So a folio a write is about to dirty keeps its unit's
//! reservation. Writeback counts its own units' reservations as free, and
//! releases them once the unit is clean; truncate and eviction release
//! units past the end. A reservation that outlives its write, as after a
//! failed copy, is swept before a reservation fails.

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
    units: KVec<u64>,
}

/// Every unit holding a reservation, by inode.
// sorted vectors, O(n) inserts; an rbtree or xarray if many
// files hold many scattered dirty units at once.
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
            .is_ok_and(|at| self.entries[at].units.binary_search(&unit).is_ok())
    }

    fn add(&mut self, inode: &Inode, unit: u64) -> Result {
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
        if let Err(position) = units.binary_search(&unit) {
            units.reserve(1, GFP_KERNEL)?;
            units.insert_within_capacity(position, unit)?;
            self.blocks += UNIT_BLOCKS;
        }
        Ok(())
    }

    /// Blocks `object` holds for `units`.
    pub(crate) fn held(&self, object: u64, units: impl Iterator<Item = u64>) -> u64 {
        units.filter(|unit| self.holds(object, *unit)).count() as u64 * UNIT_BLOCKS
    }

    /// Release `object`'s units for which `keep` is false.
    fn retain(&mut self, object: u64, mut keep: impl FnMut(usize, u64) -> bool) {
        let Ok(at) = self.entry(object) else {
            return;
        };
        let entry = &mut self.entries[at];
        let mapping = entry.mapping;
        let before = entry.units.len();
        entry.units.retain(|unit| keep(mapping, *unit));
        self.blocks -= (before - entry.units.len()) as u64 * UNIT_BLOCKS;
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
    let missing = units
        .iter()
        .filter(|unit| !state.reservations.holds(object, **unit));
    let needed = missing.count() as u64 * UNIT_BLOCKS;
    if needed == 0 {
        return Ok(());
    }
    let mut swept = false;
    while available(state) < needed + write::HEADROOM {
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
    for unit in units {
        state.reservations.add(inode, *unit)?;
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
