// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Allocation within a transaction (FORMAT.md section 6).
//!
//! Each allocation group a transaction touches keeps its committed bitmap
//! and its current one. A block is allocatable only when free in both: the
//! committed bitmap is the mirrored state allocations must come from, and
//! the current one excludes what this transaction already took. Blocks this
//! transaction retires or reclaims stay unavailable to it.

use kernel::prelude::*;

use crate::commit::implicit_bitmap;
use crate::edit::{Disk, Txn};
use crate::ondisk::format::{TreeKind, ALLOCATION_GROUP_BLOCKS};
use crate::tree::{Link, Tree};

const BITMAP: usize = 2048;
/// Retired blocks one commit reclaims at most: 256 MiB.
const RECLAIM_BLOCKS: u64 = 65_536;

/// One allocation group a transaction touched.
pub(crate) struct Group {
    index: u64,
    committed: KBox<[u8; BITMAP]>,
    current: KBox<[u8; BITMAP]>,
}

fn bit(bitmap: &[u8; BITMAP], block: u64) -> bool {
    let bit = (block % ALLOCATION_GROUP_BLOCKS) as usize;
    bitmap[bit / 8] & (1 << (bit % 8)) != 0
}

fn set_bit(bitmap: &mut [u8; BITMAP], block: u64, value: bool) {
    let bit = (block % ALLOCATION_GROUP_BLOCKS) as usize;
    if value {
        bitmap[bit / 8] |= 1 << (bit % 8);
    } else {
        bitmap[bit / 8] &= !(1 << (bit % 8));
    }
}

/// The group after `index` of `groups`, wrapping. A remainder by a runtime
/// divisor would add a division-by-zero panic path, which objtool does not
/// know never returns.
fn next_group(index: u64, groups: u64) -> u64 {
    if index + 1 >= groups {
        0
    } else {
        index + 1
    }
}

impl Group {
    fn free(&self, block: u64) -> bool {
        !bit(&self.committed, block) && !bit(&self.current, block)
    }
}

impl Txn {
    /// The touched group `index`, read from the committed allocation tree
    /// on first use.
    fn group(&mut self, disk: &Disk, index: u64) -> Result<usize> {
        if let Some(position) = self.groups.iter().position(|group| group.index == index) {
            return Ok(position);
        }
        let mut committed = KBox::new([0u8; BITMAP], GFP_KERNEL)?;
        let tree = Tree {
            sb: disk.sb,
            uuid: disk.uuid,
            blocks: disk.blocks,
            kind: TreeKind::Allocation,
            root: Link::Disk(self.committed_allocation),
            generation: disk.generation,
            drafts: &[],
        };
        let stored = tree.get(&index.to_be_bytes(), |value| {
            committed.copy_from_slice(value.get(..BITMAP).ok_or(EIO)?);
            Ok(())
        })?;
        if stored.is_none() {
            implicit_bitmap(index, disk.blocks, &mut committed);
        }
        let mut current = KBox::new([0u8; BITMAP], GFP_KERNEL)?;
        current.copy_from_slice(&committed[..]);
        self.groups.push(
            Group {
                index,
                committed,
                current,
            },
            GFP_KERNEL,
        )?;
        Ok(self.groups.len() - 1)
    }

    /// Set or clear the current allocation bits of `blocks`.
    pub(crate) fn set_allocation(&mut self, disk: &Disk, blocks: &[u64], value: bool) -> Result {
        for &block in blocks {
            let group = self.group(disk, block / ALLOCATION_GROUP_BLOCKS)?;
            set_bit(&mut self.groups[group].current, block, value);
        }
        Ok(())
    }

    /// Allocate up to `count` contiguous data blocks, searching from the
    /// transaction's hint; the first block and how many were found.
    pub(crate) fn allocate_data(&mut self, disk: &Disk, count: u64) -> Result<(u64, u64)> {
        let groups = disk.blocks.div_ceil(ALLOCATION_GROUP_BLOCKS);
        let mut group_index = self.hint / ALLOCATION_GROUP_BLOCKS;
        for _ in 0..=groups {
            let group = self.group(disk, group_index)?;
            let first = (group_index * ALLOCATION_GROUP_BLOCKS).max(self.hint);
            let end = ((group_index + 1) * ALLOCATION_GROUP_BLOCKS).min(disk.blocks);
            let mut block = first;
            while block < end {
                if !self.groups[group].free(block) {
                    block += 1;
                    continue;
                }
                let mut run = 1;
                while run < count && block + run < end && self.groups[group].free(block + run) {
                    run += 1;
                }
                for taken in block..block + run {
                    set_bit(&mut self.groups[group].current, taken, true);
                }
                self.data_allocated += run;
                self.hint = block + run;
                return Ok((block, run));
            }
            group_index = next_group(group_index, groups);
            self.hint = group_index * ALLOCATION_GROUP_BLOCKS;
        }
        Err(ENOSPC)
    }

    /// Take up to `count` raw free blocks for the workspace, scanning at
    /// most `count` groups from `cursor`; the cursor to continue from.
    pub(crate) fn refill(
        &mut self,
        disk: &Disk,
        slots: &[u64],
        mut cursor: u64,
        count: usize,
        out: &mut KVec<u64>,
    ) -> Result<u64> {
        let groups = disk.blocks.div_ceil(ALLOCATION_GROUP_BLOCKS);
        for _ in 0..groups.min(count as u64) {
            let index = cursor;
            cursor = next_group(index, groups);
            let group = self.group(disk, index)?;
            let end = ((index + 1) * ALLOCATION_GROUP_BLOCKS).min(disk.blocks);
            for block in index * ALLOCATION_GROUP_BLOCKS..end {
                if self.groups[group].free(block) && slots.binary_search(&block).is_err() {
                    out.push(block, GFP_KERNEL)?;
                    if out.len() == count {
                        return Ok(cursor);
                    }
                }
            }
        }
        Ok(cursor)
    }

    /// Release data blocks this transaction's writes superseded. Blocks the
    /// committed state owns stay allocated until a later transaction
    /// reclaims them; blocks this transaction allocated, which no committed
    /// root refers to, are free again at once.
    pub(crate) fn retire(&mut self, disk: &Disk, first: u64, count: u64) -> Result {
        for block in first..first + count {
            let group = self.group(disk, block / ALLOCATION_GROUP_BLOCKS)?;
            if !bit(&self.groups[group].committed, block) {
                set_bit(&mut self.groups[group].current, block, false);
                self.data_allocated = self.data_allocated.saturating_sub(1);
                continue;
            }
            match self.retired.last_mut() {
                Some((start, length))
                    if *start + *length == block && *length < u64::from(u32::MAX) =>
                {
                    *length += 1
                }
                _ => self.retired.push((block, 1), GFP_KERNEL)?,
            }
        }
        Ok(())
    }

    /// Write the touched groups into the allocation tree and the retired
    /// ranges into the cleanup tree, as commit begins.
    pub(crate) fn flush_space(&mut self, disk: &Disk) -> Result {
        let mut retired = KVec::new();
        core::mem::swap(&mut retired, &mut self.retired);
        for (first, count) in retired.iter() {
            let mut key = [0u8; 17];
            key[0] = 1;
            key[1..9].copy_from_slice(&self.generation.to_be_bytes());
            key[9..].copy_from_slice(&first.to_be_bytes());
            let mut value = [0u8; 8];
            value[..4].copy_from_slice(&(*count as u32).to_le_bytes());
            self.put(disk, TreeKind::Cleanup, &key, &value)?;
        }
        let mut groups = KVec::new();
        core::mem::swap(&mut groups, &mut self.groups);
        for group in groups.iter() {
            if group.current[..] != group.committed[..] {
                self.put(
                    disk,
                    TreeKind::Allocation,
                    &group.index.to_be_bytes(),
                    &group.current[..],
                )?;
            }
        }
        Ok(())
    }

    /// Reclaim retired data from generations both roots already hold,
    /// removing the records and clearing the blocks' bits; whether the batch
    /// filled, so that more may remain.
    ///
    /// FORMAT.md section 6 sets an initial batch of 64 records and 1,024
    /// blocks. Bitmaps live in memory here, so a batch takes 64 records and
    /// `RECLAIM_BLOCKS`: rewrites retire faster than 1,024 blocks a commit
    /// would return. A record past the block budget is split: its prefix is
    /// reclaimed and the rest stays, so a range of any size drains.
    pub(crate) fn reclaim(
        &mut self,
        disk: &Disk,
        committed_cleanup: crate::ondisk::format::TreeRef,
    ) -> Result<bool> {
        // Key, first block, block count, and blocks to reclaim now.
        let mut records: KVec<([u8; 17], u64, u64, u64)> = KVec::new();
        let mut blocks = 0;
        let tree = Tree {
            sb: disk.sb,
            uuid: disk.uuid,
            blocks: disk.blocks,
            kind: TreeKind::Cleanup,
            root: Link::Disk(committed_cleanup),
            generation: disk.generation,
            drafts: &[],
        };
        tree.scan(&[1], &mut |key, value| {
            if key.first() != Some(&1) || key.len() != 17 {
                return Ok(false);
            }
            if crate::tree::be_u64(key, 1) > disk.generation {
                return Ok(false);
            }
            if records.len() == 64 || blocks == RECLAIM_BLOCKS {
                return Ok(false);
            }
            let count = value
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, |bytes| u64::from(u32::from_le_bytes(bytes)));
            let take = count.min(RECLAIM_BLOCKS - blocks);
            let mut copy = [0u8; 17];
            copy.copy_from_slice(key);
            records.push((copy, crate::tree::be_u64(key, 9), count, take), GFP_KERNEL)?;
            blocks += take;
            Ok(true)
        })?;
        for (key, first, count, take) in records.iter() {
            self.delete(disk, TreeKind::Cleanup, key)?;
            if take < count {
                let mut rest = *key;
                rest[9..].copy_from_slice(&(first + take).to_be_bytes());
                let mut value = [0u8; 8];
                value[..4].copy_from_slice(&((count - take) as u32).to_le_bytes());
                self.put(disk, TreeKind::Cleanup, &rest, &value)?;
            }
            for block in *first..*first + *take {
                let group = self.group(disk, block / ALLOCATION_GROUP_BLOCKS)?;
                set_bit(&mut self.groups[group].current, block, false);
            }
            self.reclaimed += take;
        }
        // A filled batch may have split the last record, so it says nothing
        // of what follows: report more, at worst one empty pass.
        Ok(records.len() == 64 || blocks == RECLAIM_BLOCKS)
    }

    /// Free blocks once this transaction commits, counting its live drafts.
    pub(crate) fn free_blocks(&self, committed: u64) -> u64 {
        (committed + self.reclaimed + self.superseded.len() as u64)
            .saturating_sub(self.data_allocated + self.live_drafts() as u64)
    }
}
