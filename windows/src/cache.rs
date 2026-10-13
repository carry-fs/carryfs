//! A bounded cache of 4 KiB device blocks.
//!
//! The core re-reads metadata pages on every operation. Linux serves those
//! reads from the kernel page cache, but Windows does not cache I/O on a volume
//! handle, so each re-read reaches the disk. The disk is locked to this
//! process and every write goes through the cache, so a cached block always
//! equals what was last read or written. The core still verifies checksums on
//! the returned bytes; damage that appears on the medium after a block was
//! cached is found when the block is next read from the disk.
//!
//! Written blocks are also held as dirty until the next durable flush and then
//! written in contiguous runs: each device command costs far more than its
//! 4 KiB. The core allows any order among writes before a flush, and the flush
//! barrier itself is unchanged.

use std::collections::{BTreeSet, HashMap, VecDeque};

pub const BLOCK: usize = 4096;
/// 64 MiB of clean blocks; dirty blocks are never evicted.
const CAPACITY: usize = 16_384;
/// Longest run written in one device call.
pub const MAX_RUN: usize = 1 << 20;
/// Dirty bytes held before they are written out ahead of the flush. With
/// deferred checkpoints a flush can be a second of writes away, and holding
/// more costs memory and eviction scans without saving device commands.
pub const MAX_DIRTY: usize = 4 << 20;

#[derive(Default)]
pub struct Cache {
    blocks: HashMap<u64, Box<[u8]>>,
    /// Insertion order for eviction; may name blocks already removed.
    order: VecDeque<u64>,
    /// Written but not yet sent to the device.
    dirty: BTreeSet<u64>,
    /// Sent to the background writer but not yet written, with the number of
    /// queued runs covering each block. The device still holds older bytes,
    /// so these blocks must stay cached for `merge` to cover them.
    in_flight: HashMap<u64, u32>,
}

impl Cache {
    /// Copy `out.len()` bytes at `offset` from the cache when every block is
    /// present. Only whole blocks are cached.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> bool {
        if !aligned(offset, out.len()) {
            return false;
        }
        let blocks = (out.len() / BLOCK) as u64;
        // Check coverage first: copying a prefix before a miss costs more than the second lookup.
        if !(0..blocks).all(|index| self.blocks.contains_key(&(offset + index * BLOCK as u64))) {
            return false;
        }
        for (delta, chunk) in (0..).step_by(BLOCK).zip(out.chunks_mut(BLOCK)) {
            chunk.copy_from_slice(&self.blocks[&(offset + delta as u64)]);
        }
        true
    }

    /// Record bytes just written, replacing what was cached.
    pub fn store(&mut self, offset: u64, data: &[u8]) {
        if !aligned(offset, data.len()) {
            return;
        }
        for (delta, chunk) in (0..).step_by(BLOCK).zip(data.chunks(BLOCK)) {
            let key = offset + delta as u64;
            if self.blocks.insert(key, chunk.into()).is_none() {
                self.order.push_back(key);
            }
        }
        self.evict();
    }

    /// Combine bytes just read from the device with the cache. A cached block
    /// is newer than the device when it is dirty, and equal otherwise, so it
    /// replaces the bytes read; blocks not cached are stored.
    pub fn merge(&mut self, offset: u64, data: &mut [u8]) {
        if !aligned(offset, data.len()) {
            return;
        }
        for (delta, chunk) in (0..).step_by(BLOCK).zip(data.chunks_mut(BLOCK)) {
            let key = offset + delta as u64;
            match self.blocks.get(&key) {
                Some(cached) => chunk.copy_from_slice(cached),
                None => {
                    self.blocks.insert(key, (&*chunk).into());
                    self.order.push_back(key);
                }
            }
        }
        self.evict();
    }

    /// Copy the cached blocks of a range over bytes just read from the device,
    /// without caching the rest: for file data, which would evict metadata.
    pub fn overlay(&self, offset: u64, data: &mut [u8]) {
        if !aligned(offset, data.len()) {
            return;
        }
        for (delta, chunk) in (0..).step_by(BLOCK).zip(data.chunks_mut(BLOCK)) {
            if let Some(cached) = self.blocks.get(&(offset + delta as u64)) {
                chunk.copy_from_slice(cached);
            }
        }
    }

    /// Drop the blocks of a written run that are neither held nor queued
    /// again. File data is written in long runs and rarely read back soon;
    /// keeping it evicts the metadata every operation reads.
    pub fn drop_clean(&mut self, offset: u64, len: usize) {
        for key in (offset..offset + len as u64).step_by(BLOCK) {
            if !self.dirty.contains(&key) && !self.in_flight.contains_key(&key) {
                self.blocks.remove(&key);
            }
        }
    }

    fn evict(&mut self) {
        // Rewrites after `forget` requeue a block, so drop stale entries
        // before the queue outgrows the cache.
        if self.order.len() > 2 * CAPACITY {
            let blocks = &self.blocks;
            let mut seen = std::collections::HashSet::new();
            self.order
                .retain(|key| blocks.contains_key(key) && seen.insert(*key));
        }
        // Evict clean blocks in insertion order; reads do not refresh their position.
        let mut checked = 0;
        while self.blocks.len() > CAPACITY + self.dirty.len() + self.in_flight.len()
            && checked < self.order.len()
        {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if self.dirty.contains(&key) || self.in_flight.contains_key(&key) {
                self.order.push_back(key);
                checked += 1;
            } else {
                self.blocks.remove(&key);
            }
        }
    }

    /// Hold whole written blocks until `take_dirty`. Returns false for
    /// unaligned data, which the caller writes directly.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> bool {
        if !aligned(offset, data.len()) {
            return false;
        }
        let blocks = (data.len() / BLOCK) as u64;
        self.dirty
            .extend((0..blocks).map(|index| offset + index * BLOCK as u64));
        self.store(offset, data);
        true
    }

    /// Keep the blocks of a run sent to the background writer cached until
    /// `unpin` reports it written.
    pub fn pin(&mut self, offset: u64, len: usize) {
        for key in (offset..offset + len as u64).step_by(BLOCK) {
            *self.in_flight.entry(key).or_default() += 1;
        }
    }

    pub fn unpin(&mut self, offset: u64, len: usize) {
        for key in (offset..offset + len as u64).step_by(BLOCK) {
            if let Some(count) = self.in_flight.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    self.in_flight.remove(&key);
                }
            }
        }
        self.evict();
    }

    /// Whether any block of the range is held or queued, so the device may not
    /// have its latest bytes yet.
    pub fn pending(&self, offset: u64, len: usize) -> bool {
        let first = offset - offset % BLOCK as u64;
        (first..offset + len as u64)
            .step_by(BLOCK)
            .any(|key| self.dirty.contains(&key) || self.in_flight.contains_key(&key))
    }

    /// Bytes written but not yet sent to the device.
    pub fn dirty_bytes(&self) -> usize {
        self.dirty.len() * BLOCK
    }

    /// Contiguous runs of dirty blocks, each at most `MAX_RUN` bytes, in
    /// offset order. The blocks stay cached and are no longer dirty.
    pub fn take_dirty(&mut self) -> Vec<(u64, Vec<u8>)> {
        let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
        for key in std::mem::take(&mut self.dirty) {
            let block = &self.blocks[&key];
            match runs.last_mut() {
                Some((start, run))
                    if *start + run.len() as u64 == key && run.len() + BLOCK <= MAX_RUN =>
                {
                    run.extend_from_slice(block)
                }
                _ => runs.push((key, block.to_vec())),
            }
        }
        runs
    }

    /// Forget every block overlapping a range whose device contents are about
    /// to change, so a failed write cannot leave stale bytes cached.
    pub fn forget(&mut self, offset: u64, len: usize) {
        let first = offset - offset % BLOCK as u64;
        let end = offset + len as u64;
        let mut key = first;
        while key < end {
            self.blocks.remove(&key);
            self.dirty.remove(&key);
            key += BLOCK as u64;
        }
    }
}

fn aligned(offset: u64, len: usize) -> bool {
    len > 0 && offset.is_multiple_of(BLOCK as u64) && len.is_multiple_of(BLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(byte: u8) -> Vec<u8> {
        vec![byte; BLOCK]
    }

    #[test]
    fn serves_only_fully_cached_aligned_ranges() {
        let mut cache = Cache::default();
        cache.store(0, &[block(1), block(2)].concat());
        let mut out = vec![0; 2 * BLOCK];
        assert!(cache.read(0, &mut out));
        assert_eq!(out, [block(1), block(2)].concat());
        assert!(!cache.read(BLOCK as u64, &mut vec![0; 2 * BLOCK]));
        assert!(!cache.read(1, &mut vec![0; BLOCK]));
        cache.store(12, &[9; 512]);
        assert!(!cache.read(0, &mut vec![0; 512]));
    }

    #[test]
    fn forgotten_blocks_are_never_served() {
        let mut cache = Cache::default();
        cache.store(0, &[block(1), block(2), block(3)].concat());
        // A 512-byte write inside block 1 drops that whole block only.
        cache.forget(BLOCK as u64 + 512, 512);
        assert!(cache.read(0, &mut vec![0; BLOCK]));
        assert!(!cache.read(BLOCK as u64, &mut vec![0; BLOCK]));
        assert!(cache.read(2 * BLOCK as u64, &mut vec![0; BLOCK]));
        cache.store(BLOCK as u64, &block(7));
        let mut out = vec![0; BLOCK];
        assert!(cache.read(BLOCK as u64, &mut out));
        assert_eq!(out, block(7));
    }

    #[test]
    fn device_reads_never_replace_cached_blocks() {
        let mut cache = Cache::default();
        // Block 1 is written but not yet sent; the device still holds old bytes.
        assert!(cache.write(BLOCK as u64, &block(7)));
        let mut read = [block(1), block(2), block(3)].concat();
        cache.merge(0, &mut read);
        assert_eq!(read, [block(1), block(7), block(3)].concat());
        let mut out = vec![0; 3 * BLOCK];
        assert!(cache.read(0, &mut out));
        assert_eq!(out, read);
        assert_eq!(cache.take_dirty(), [(BLOCK as u64, block(7))]);
    }

    #[test]
    fn dirty_blocks_merge_into_runs_and_survive_eviction() {
        let mut cache = Cache::default();
        let base = 10 * BLOCK as u64;
        assert!(cache.write(base, &[block(1), block(2)].concat()));
        assert!(cache.write(base + 3 * BLOCK as u64, &block(4)));
        assert!(cache.write(base + 2 * BLOCK as u64, &block(3)));
        assert!(cache.write(0, &block(9)));
        assert!(!cache.write(base + 1, &[0; 512]));
        // Filling the cache with clean blocks must not evict unwritten data.
        for index in 1000..1000 + CAPACITY as u64 + 5 {
            cache.store(index * BLOCK as u64, &block(0));
        }
        let runs = cache.take_dirty();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], (0, block(9)));
        assert_eq!(
            runs[1],
            (base, [block(1), block(2), block(3), block(4)].concat())
        );
        assert!(cache.dirty.is_empty());
        assert!(cache.take_dirty().is_empty());
    }

    #[test]
    fn blocks_in_flight_survive_eviction_until_unpinned() {
        let mut cache = Cache::default();
        assert!(cache.write(0, &[block(1), block(2)].concat()));
        let runs = cache.take_dirty();
        let (offset, run) = &runs[0];
        cache.pin(*offset, run.len());
        // The same block queued twice needs both runs written.
        cache.pin(0, BLOCK);
        for index in 1000..1000 + CAPACITY as u64 + 5 {
            cache.store(index * BLOCK as u64, &block(0));
        }
        assert!(cache.read(0, &mut vec![0; 2 * BLOCK]));
        cache.unpin(*offset, run.len());
        assert_eq!(cache.in_flight.len(), 1);
        cache.unpin(0, BLOCK);
        assert!(cache.in_flight.is_empty());
        assert_eq!(cache.blocks.len(), CAPACITY);
        // Released, the blocks age out like any clean block.
        for index in 50_000..50_000 + CAPACITY as u64 {
            cache.store(index * BLOCK as u64, &block(0));
        }
        assert!(!cache.read(0, &mut vec![0; BLOCK]));
        assert!(!cache.read(BLOCK as u64, &mut vec![0; BLOCK]));
    }

    #[test]
    fn held_and_queued_blocks_are_pending() {
        let mut cache = Cache::default();
        let far = 100 * BLOCK as u64;
        assert!(!cache.pending(0, 200 * BLOCK));
        assert!(cache.write(far, &block(1)));
        assert!(cache.pending(0, 200 * BLOCK));
        assert!(!cache.pending(0, 100 * BLOCK));
        let runs = cache.take_dirty();
        cache.pin(runs[0].0, runs[0].1.len());
        assert!(cache.pending(far, BLOCK));
        cache.unpin(runs[0].0, runs[0].1.len());
        assert!(!cache.pending(0, 200 * BLOCK));
    }

    #[test]
    fn overlay_prefers_cached_blocks_and_caches_nothing() {
        let mut cache = Cache::default();
        assert!(cache.write(BLOCK as u64, &block(7)));
        let mut read = [block(1), block(2), block(3)].concat();
        cache.overlay(0, &mut read);
        assert_eq!(read, [block(1), block(7), block(3)].concat());
        assert!(!cache.read(0, &mut vec![0; BLOCK]));
        assert!(!cache.read(2 * BLOCK as u64, &mut vec![0; BLOCK]));
    }

    #[test]
    fn written_data_runs_leave_the_cache_unless_written_again() {
        let mut cache = Cache::default();
        assert!(cache.write(0, &vec![1; 4 * BLOCK]));
        let runs = cache.take_dirty();
        cache.pin(0, 4 * BLOCK);
        // Block 2 is rewritten and held while the first run is queued.
        assert!(cache.write(2 * BLOCK as u64, &block(9)));
        cache.unpin(runs[0].0, runs[0].1.len());
        cache.drop_clean(0, 4 * BLOCK);
        assert!(!cache.read(0, &mut vec![0; BLOCK]));
        let mut out = vec![0; BLOCK];
        assert!(cache.read(2 * BLOCK as u64, &mut out));
        assert_eq!(out, block(9));
        assert!(!cache.read(3 * BLOCK as u64, &mut vec![0; BLOCK]));
    }

    #[test]
    fn runs_are_capped() {
        let mut cache = Cache::default();
        let blocks = MAX_RUN / BLOCK + 1;
        assert!(cache.write(0, &vec![5; blocks * BLOCK]));
        let lengths: Vec<usize> = cache
            .take_dirty()
            .iter()
            .map(|(_, run)| run.len())
            .collect();
        assert_eq!(lengths, [MAX_RUN, BLOCK]);
    }

    #[test]
    fn rewriting_one_block_keeps_the_queue_bounded() {
        let mut cache = Cache::default();
        for byte in 0..(3 * CAPACITY) {
            cache.forget(0, BLOCK);
            cache.store(0, &block(byte as u8));
        }
        assert!(cache.order.len() <= 2 * CAPACITY + 1);
    }

    #[test]
    fn eviction_keeps_the_cache_bounded() {
        let mut cache = Cache::default();
        for index in 0..CAPACITY as u64 + 10 {
            cache.store(index * BLOCK as u64, &block(index as u8));
        }
        assert_eq!(cache.blocks.len(), CAPACITY);
        assert!(!cache.read(0, &mut vec![0; BLOCK]));
        let last = (CAPACITY as u64 + 9) * BLOCK as u64;
        assert!(cache.read(last, &mut vec![0; BLOCK]));
    }
}
