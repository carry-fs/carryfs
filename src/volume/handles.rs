//! Bounded registry of opaque handles, scoped to one mount instance.
//!
//! A handle pins its object in memory (see FORMAT.md's orphan-pin rule) but
//! never touches disk. Slots use a generation counter so a stale copy of a
//! closed handle can never revalidate, even if its index is reused.

use std::collections::BTreeSet;

use super::{Access, ObjectId};
use crate::{Error, Result};

const MAX_HANDLES: usize = 4096;

/// An opaque handle to an open or retained catalog object.
///
/// Bound to one mount instance and registry slot. It cannot be cloned, name a
/// different mount's object, or be reused after close.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct Handle {
    mount: u64,
    index: u32,
    generation: u32,
}

struct Slot {
    generation: u32,
    object: Option<(ObjectId, Access)>,
}

pub(crate) struct Registry {
    mount: u64,
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl Registry {
    pub(crate) fn new(mount: u64) -> Self {
        Self {
            mount,
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().all(|slot| slot.object.is_none())
    }

    pub(crate) fn has_room(&self) -> bool {
        !self.free.is_empty() || self.slots.len() < MAX_HANDLES
    }

    pub(crate) fn open(&mut self, object: ObjectId, access: Access) -> Result<Handle> {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.object = Some((object, access));
            return Ok(Handle {
                mount: self.mount,
                index,
                generation: slot.generation,
            });
        }
        if self.slots.len() >= MAX_HANDLES {
            return Err(Error::LimitExceeded);
        }
        let index = u32::try_from(self.slots.len()).map_err(|_| Error::LimitExceeded)?;
        self.slots.push(Slot {
            generation: 0,
            object: Some((object, access)),
        });
        Ok(Handle {
            mount: self.mount,
            index,
            generation: 0,
        })
    }

    pub(crate) fn validate(&self, handle: &Handle) -> Result<ObjectId> {
        self.validate_access(handle).map(|(object, _)| object)
    }

    pub(crate) fn validate_access(&self, handle: &Handle) -> Result<(ObjectId, Access)> {
        if handle.mount != self.mount {
            return Err(Error::InvalidHandle);
        }
        let slot = self
            .slots
            .get(handle.index as usize)
            .filter(|slot| slot.generation == handle.generation)
            .ok_or(Error::InvalidHandle)?;
        slot.object.ok_or(Error::InvalidHandle)
    }

    pub(crate) fn close(&mut self, handle: Handle) -> Result<()> {
        self.validate(&handle)?;
        let slot = &mut self.slots[handle.index as usize];
        slot.object = None;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(handle.index);
        Ok(())
    }

    /// Object IDs currently pinned by an open handle in this registry.
    pub(crate) fn pinned(&self) -> BTreeSet<u64> {
        self.slots
            .iter()
            .filter_map(|slot| slot.object.map(|(object, _)| object.0))
            .collect()
    }
}
