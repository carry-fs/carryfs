//! Read-ahead and a metadata block cache for the macOS raw disk node.
//!
//! Linux reads the block device through the page cache, which caches and reads
//! ahead. The macOS raw node does neither, so every 64 KiB data unit and every
//! metadata page was its own device round trip: cold reads ran at about 170
//! MiB/s on a USB SSD. The buffered node was slower still, 85 MiB/s. So a read
//! of a unit or more fetches `READAHEAD` from the read in one call, and a
//! smaller read, metadata, fetches its whole unit into a bounded cache.
//!
//! The daemon owns the device (`Disk::open` locks it), and every write passes
//! through `wrote`, so what is cached equals what was last written. The core
//! still verifies checksums on the bytes it gets back.

use std::collections::{HashMap, HashSet, VecDeque};

use carryfs_core::Result;

const BLOCK: usize = 4096;
/// One checksum unit, the granularity the core reads file data at (FORMAT.md).
const UNIT: usize = 1 << 16;
/// Reads of a unit or more fetch this much, starting at the read.
pub const READAHEAD: usize = 64 * UNIT;
/// File-data windows kept at once.
const WINDOWS: usize = 2;
/// 32 MiB of cached 4 KiB blocks.
const CAPACITY: usize = 8192;
/// A sent run at least this long is file data.
const LONG_RUN: usize = 256 << 10;

pub struct ReadCache {
    byte_len: u64,
    /// The last `WINDOWS` file-data windows and their device offsets, newest
    /// last. A second one keeps a stream whose read jumped to a rewritten
    /// chunk elsewhere and came back.
    windows: VecDeque<(u64, Vec<u8>)>,
    blocks: HashMap<u64, Box<[u8]>>,
    /// Insertion order for eviction; may name blocks already removed.
    order: VecDeque<u64>,
}

impl ReadCache {
    pub fn new(byte_len: u64) -> Self {
        Self {
            byte_len,
            windows: VecDeque::new(),
            blocks: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Fill `out` from the device at `offset`, through `fetch` when neither
    /// the window nor the cache holds it. A new window ends at `limit`, the
    /// end of the file's extent when the caller knows it.
    pub fn read(
        &mut self,
        offset: u64,
        out: &mut [u8],
        limit: u64,
        mut fetch: impl FnMut(u64, &mut [u8]) -> Result<()>,
    ) -> Result<()> {
        let len = out.len() as u64;
        // Any read the window covers is served from it: small files and the
        // tails of units sit beside the data just read.
        if let Some((start, window)) = self.covering(offset, out.len()) {
            let skip = (offset - start) as usize;
            out.copy_from_slice(&window[skip..skip + out.len()]);
            return Ok(());
        }
        if out.len() >= UNIT {
            let end = offset
                .saturating_add(READAHEAD as u64)
                .min(self.byte_len)
                .min(limit);
            if offset.saturating_add(len) > end {
                return fetch(offset, out);
            }
            let mut window = vec![0; (end - offset) as usize];
            fetch(offset, &mut window)?;
            out.copy_from_slice(&window[..out.len()]);
            self.keep_window(offset, window);
            return Ok(());
        }
        if self.get(offset, out) {
            return Ok(());
        }
        let start = offset - offset % UNIT as u64;
        let end = start.saturating_add(UNIT as u64).min(self.byte_len);
        if offset.saturating_add(len) > end {
            fetch(offset, out)?;
            self.put(offset, out);
            return Ok(());
        }
        let mut unit = vec![0; (end - start) as usize];
        fetch(start, &mut unit)?;
        let skip = (offset - start) as usize;
        out.copy_from_slice(&unit[skip..skip + out.len()]);
        self.put(start, &unit);
        Ok(())
    }

    /// The window holding all of `offset..offset + len`, and its offset.
    fn covering(&self, offset: u64, len: usize) -> Option<(u64, &[u8])> {
        self.windows
            .iter()
            .find(|(start, window)| {
                offset >= *start
                    && (offset - start).saturating_add(len as u64) <= window.len() as u64
            })
            .map(|(start, window)| (*start, &window[..]))
    }

    /// Whether a file-data window holds the byte at `at`.
    pub fn holds(&self, at: u64) -> bool {
        self.covering(at, 1).is_some()
    }

    /// Copy the start of `out` from the window holding `at`, as far as that
    /// window reaches, and return the bytes copied: 0 when none holds it.
    pub fn take(&self, at: u64, out: &mut [u8]) -> usize {
        let Some((start, window)) = self.covering(at, 1) else {
            return 0;
        };
        let held = &window[(at - start) as usize..];
        let taken = held.len().min(out.len());
        out[..taken].copy_from_slice(&held[..taken]);
        taken
    }

    /// Keep a file-data window, dropping the oldest of `WINDOWS`.
    pub fn keep_window(&mut self, start: u64, window: Vec<u8>) {
        if self.windows.len() == WINDOWS {
            self.windows.pop_front();
        }
        self.windows.push_back((start, window));
    }

    /// The units under the hinted metadata range `offset..offset + len`
    /// that hold a hinted block not cached, as the `(start, length)` reads a
    /// miss in `read` would make.
    pub fn uncached_units(&self, offset: u64, len: usize) -> Vec<(u64, usize)> {
        let mut units = Vec::new();
        if !Self::aligned(offset, len) {
            return units;
        }
        let end = offset.saturating_add(len as u64).min(self.byte_len);
        let mut start = offset - offset % UNIT as u64;
        while start < end {
            let unit_end = start.saturating_add(UNIT as u64).min(self.byte_len);
            if (start.max(offset)..unit_end.min(end))
                .step_by(BLOCK)
                .any(|at| !self.blocks.contains_key(&at))
            {
                units.push((start, (unit_end - start) as usize));
            }
            start = unit_end;
        }
        units
    }

    /// Cache a unit read ahead of its first use.
    pub fn fill(&mut self, start: u64, unit: &[u8]) {
        self.put(start, unit);
    }

    /// The newest file-data window's device offset and length.
    pub fn window(&self) -> (u64, usize) {
        self.windows
            .back()
            .map_or((0, 0), |(start, window)| (*start, window.len()))
    }

    /// Record a write. Metadata pages stay cached with their new bytes; file
    /// data leaves the cache, so it cannot evict metadata. A window holding
    /// the range is dropped.
    pub fn wrote(&mut self, offset: u64, data: &[u8]) {
        if data.len() < UNIT {
            self.put(offset, data);
        } else {
            self.forget(offset, data.len());
        }
        let end = offset.saturating_add(data.len() as u64);
        self.windows
            .retain(|(start, window)| end <= *start || start + window.len() as u64 <= offset);
    }

    /// Record a run sent to the device. The core writes file data one 4 KiB
    /// page at a time, so `wrote` caches it like metadata; a long contiguous
    /// run is file data, rarely read back soon, and leaves the cache here so
    /// it cannot evict metadata.
    pub fn sent(&mut self, offset: u64, len: usize) {
        if len >= LONG_RUN {
            self.forget(offset, len);
        }
    }

    /// Drop a page no root references; the core writes it before reading it.
    pub fn discarded(&mut self, offset: u64) {
        self.forget(offset, BLOCK);
    }

    fn aligned(offset: u64, len: usize) -> bool {
        offset.is_multiple_of(BLOCK as u64) && len.is_multiple_of(BLOCK) && len != 0
    }

    /// Fill `out` if every block it covers is cached.
    fn get(&self, offset: u64, out: &mut [u8]) -> bool {
        if !Self::aligned(offset, out.len()) {
            return false;
        }
        for (index, slot) in out.chunks_mut(BLOCK).enumerate() {
            match self.blocks.get(&(offset + (index * BLOCK) as u64)) {
                Some(block) => slot.copy_from_slice(block),
                None => return false,
            }
        }
        true
    }

    fn put(&mut self, offset: u64, data: &[u8]) {
        if !Self::aligned(offset, data.len()) {
            // Unaligned bytes cannot be cached, but older cached blocks under
            // them are stale now.
            self.forget(offset - offset % BLOCK as u64, data.len() + BLOCK);
            return;
        }
        for (index, block) in data.chunks(BLOCK).enumerate() {
            let at = offset + (index * BLOCK) as u64;
            if self.blocks.insert(at, block.into()).is_none() {
                self.order.push_back(at);
            }
        }
        while self.blocks.len() > CAPACITY {
            match self.order.pop_front() {
                Some(at) => {
                    self.blocks.remove(&at);
                }
                None => break,
            }
        }
        // `forget` leaves stale names behind; drop them before they outgrow
        // the blocks they once named.
        if self.order.len() > 2 * CAPACITY {
            let mut seen = HashSet::new();
            let blocks = &self.blocks;
            self.order
                .retain(|at| blocks.contains_key(at) && seen.insert(*at));
        }
    }

    fn forget(&mut self, offset: u64, len: usize) {
        let first = offset - offset % BLOCK as u64;
        let end = offset.saturating_add(len as u64);
        let mut at = first;
        while at < end {
            self.blocks.remove(&at);
            at += BLOCK as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device whose byte at `i` is `i % 251`, counting fetches.
    fn device(calls: &mut Vec<(u64, usize)>) -> impl FnMut(u64, &mut [u8]) -> Result<()> + '_ {
        |offset, out| {
            calls.push((offset, out.len()));
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = ((offset + i as u64) % 251) as u8;
            }
            Ok(())
        }
    }

    fn expected(offset: u64, len: usize) -> Vec<u8> {
        (0..len as u64)
            .map(|i| ((offset + i) % 251) as u8)
            .collect()
    }

    #[test]
    fn sequential_unit_reads_share_one_window() {
        let mut cache = ReadCache::new(64 << 20);
        let mut calls = Vec::new();
        let mut out = vec![0; UNIT];
        for unit in 0..64 {
            let at = 4096 + unit * UNIT as u64;
            cache
                .read(at, &mut out, u64::MAX, device(&mut calls))
                .unwrap();
            assert_eq!(out, expected(at, UNIT));
        }
        assert_eq!(calls, [(4096, READAHEAD)]);
    }

    #[test]
    fn a_window_stops_at_the_device_end_and_longer_reads_go_out_as_asked() {
        let mut cache = ReadCache::new(2 * UNIT as u64);
        let mut calls = Vec::new();
        let mut out = vec![0; UNIT];
        cache
            .read(0, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        assert_eq!(calls, [(0, 2 * UNIT)]);
        let mut long = vec![0; 3 * UNIT];
        let mut calls = Vec::new();
        cache
            .read(0, &mut long[..], u64::MAX, device(&mut calls))
            .ok();
        assert_eq!(calls, [(0, 3 * UNIT)]);
    }

    #[test]
    fn a_stream_survives_one_jump_elsewhere() {
        let mut cache = ReadCache::new(64 << 20);
        let mut calls = Vec::new();
        let mut out = vec![0; UNIT];
        cache
            .read(0, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        cache
            .read(32 << 20, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        cache
            .read(2 * UNIT as u64, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        assert_eq!(out, expected(2 * UNIT as u64, UNIT));
        assert_eq!(calls, [(0, READAHEAD), (32 << 20, READAHEAD)]);
        // A third window evicts the oldest.
        cache
            .read(48 << 20, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        cache
            .read(2 * UNIT as u64, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        assert_eq!(calls.len(), 4);
    }

    #[test]
    fn small_reads_inside_the_window_are_served_from_it() {
        let mut cache = ReadCache::new(64 << 20);
        let mut calls = Vec::new();
        let mut out = vec![0; UNIT];
        cache
            .read(0, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        let mut small = vec![0; 12288];
        cache
            .read(
                3 * UNIT as u64 + 4096,
                &mut small,
                u64::MAX,
                device(&mut calls),
            )
            .unwrap();
        assert_eq!(small, expected(3 * UNIT as u64 + 4096, 12288));
        assert_eq!(calls, [(0, READAHEAD)]);
    }

    #[test]
    fn a_read_running_off_a_window_takes_what_it_holds() {
        let mut cache = ReadCache::new(64 << 20);
        let mut calls = Vec::new();
        let mut out = vec![0; UNIT];
        cache
            .read(4096, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        let end = 4096 + READAHEAD as u64;
        assert!(cache.holds(end - 1) && !cache.holds(end) && !cache.holds(0));
        let mut across = vec![0; 3 * UNIT];
        assert_eq!(cache.take(end - UNIT as u64, &mut across), UNIT);
        assert_eq!(across[..UNIT], expected(end - UNIT as u64, UNIT));
        assert_eq!(cache.take(end, &mut across), 0);
    }

    #[test]
    fn a_window_stops_at_its_limit() {
        let mut cache = ReadCache::new(64 << 20);
        let mut calls = Vec::new();
        let mut out = vec![0; UNIT];
        let limit = 4096 + 3 * UNIT as u64;
        cache
            .read(4096, &mut out, limit, device(&mut calls))
            .unwrap();
        assert_eq!(calls, [(4096, 3 * UNIT)]);
        assert_eq!(cache.window(), (4096, 3 * UNIT));
        // A limit short of the read fetches the read alone.
        let mut calls = Vec::new();
        cache
            .read(32 << 20, &mut out, 32 << 20, device(&mut calls))
            .unwrap();
        assert_eq!(calls, [(32 << 20, UNIT)]);
    }

    #[test]
    fn metadata_reads_fetch_their_unit_once() {
        let mut cache = ReadCache::new(64 << 20);
        let mut calls = Vec::new();
        let mut page = vec![0; BLOCK];
        for at in [UNIT as u64 + 8192, UNIT as u64, 2 * UNIT as u64 - 4096] {
            cache
                .read(at, &mut page, u64::MAX, device(&mut calls))
                .unwrap();
            assert_eq!(page, expected(at, BLOCK));
        }
        assert_eq!(calls, [(UNIT as u64, UNIT)]);
    }

    #[test]
    fn hinted_ranges_name_the_units_a_miss_would_read() {
        let mut cache = ReadCache::new(4 * UNIT as u64 + 8192);
        let unit = UNIT as u64;
        assert_eq!(
            cache.uncached_units(unit + 8192, 3 * UNIT),
            [
                (unit, UNIT),
                (2 * unit, UNIT),
                (3 * unit, UNIT),
                (4 * unit, 8192)
            ]
        );
        cache.fill(2 * unit, &expected(2 * unit, UNIT));
        assert_eq!(
            cache.uncached_units(unit + 8192, 2 * UNIT),
            [(unit, UNIT), (3 * unit, UNIT)]
        );
        // A filled unit is served without a fetch.
        let mut calls = Vec::new();
        let mut page = vec![0; BLOCK];
        cache
            .read(2 * unit + 4096, &mut page, u64::MAX, device(&mut calls))
            .unwrap();
        assert_eq!(page, expected(2 * unit + 4096, BLOCK));
        assert!(calls.is_empty());
    }

    #[test]
    fn writes_keep_metadata_current_and_evict_data_and_windows() {
        let mut cache = ReadCache::new(64 << 20);
        let mut calls = Vec::new();
        let mut page = vec![0; BLOCK];
        cache
            .read(0, &mut page, u64::MAX, device(&mut calls))
            .unwrap();
        cache.wrote(4096, &[7; BLOCK]);
        cache
            .read(4096, &mut page, u64::MAX, device(&mut calls))
            .unwrap();
        assert_eq!(page, [7; BLOCK]);
        // A data-sized write over cached blocks drops them.
        cache.wrote(0, &vec![9; UNIT]);
        cache
            .read(4096, &mut page, u64::MAX, device(&mut calls))
            .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(page, expected(4096, BLOCK));
        // A write inside the window drops it.
        let mut out = vec![0; UNIT];
        cache
            .read(UNIT as u64, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        cache.wrote(2 * UNIT as u64, &[1; BLOCK]);
        cache
            .read(2 * UNIT as u64, &mut out, u64::MAX, device(&mut calls))
            .unwrap();
        assert_eq!(calls.len(), 4);
    }

    #[test]
    fn long_sent_runs_leave_the_cache_and_short_ones_stay() {
        let mut cache = ReadCache::new(64 << 20);
        for page in 0..(LONG_RUN / BLOCK) as u64 + 1 {
            cache.wrote(page * BLOCK as u64, &[5; BLOCK]);
        }
        cache.sent(0, BLOCK);
        assert!(cache.blocks.contains_key(&0));
        cache.sent(0, LONG_RUN);
        assert!(!cache.blocks.contains_key(&0));
        assert!(cache.blocks.contains_key(&(LONG_RUN as u64)));
    }

    #[test]
    fn the_cache_stays_bounded() {
        let mut cache = ReadCache::new(1 << 40);
        for block in 0..3 * CAPACITY as u64 {
            cache.wrote(block * BLOCK as u64, &[1; BLOCK]);
        }
        assert_eq!(cache.blocks.len(), CAPACITY);
        assert!(cache.order.len() <= 2 * CAPACITY);
    }
}
