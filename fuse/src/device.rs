//! Disk backend for the Linux block device or the macOS raw disk node. On
//! Linux, ownership comes from the kernel's `fuseblk` mount claim, taken
//! before this device is opened. macFUSE mounts are not tied to a device, so
//! on macOS `Disk::open` takes an exclusive `flock` on the raw node instead.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use carryfs_core::{BlockDevice, Error, Geometry, MountMode, Result};

#[cfg(target_os = "macos")]
use crate::read_cache::{READAHEAD, ReadCache};

/// Longest run sent to the device in one call.
const MAX_RUN: usize = 1 << 20;
/// Threads writing queued runs at once. On macOS one synchronous `pwritev`
/// left a USB SSD idle between runs: a 3 GiB write ran at 216–220 MiB/s
/// with one writer, 254–255 with two, and 246–259 with three. Linux is
/// unmeasured with more than one.
#[cfg(target_os = "macos")]
const WRITERS: usize = 2;
#[cfg(target_os = "linux")]
const WRITERS: usize = 1;
/// Concurrent reads a read-ahead window is split into. Cold sequential reads
/// on a USB SSD: 252 MiB/s with one, 270 with two, 282 with four, 277 with
/// eight.
const READERS: usize = 4;
/// Background window reads kept at once: `AHEAD_WINDOWS` for each of the
/// read cache's two recent windows.
#[cfg(target_os = "macos")]
const PREFETCHES: usize = 2 * AHEAD_WINDOWS as usize;
/// Windows kept requested past a stream that has consumed a prefetch, while
/// they start inside the extent being read. With one, the next window was
/// requested only after the request thread had copied the last, and a USB
/// SSD idled in between: cold reads ran at 321 MiB/s. Its raw node
/// gives 350 MiB/s to batches of four 1 MiB reads, 371 with twelve in
/// flight and 378 with sixteen, and no more beyond.
#[cfg(target_os = "macos")]
const AHEAD_WINDOWS: u64 = 4;
/// Hinted metadata units read in the background at once. The writable
/// open's check of 100,000 files reads about 1,150 scattered 64 KiB units,
/// which one at a time cost a USB SSD about 0.5 ms each.
#[cfg(target_os = "macos")]
const METADATA_READS: usize = 8;
/// Held bytes that force a send before the next flush, so a long write between
/// checkpoints is written back as it goes instead of all at `fsync`. 4 MiB
/// keeps the daemon at 15.7 MiB; 32 MiB took it to 70.5 MiB for about 9% more
/// file creations, because pages sent early can no longer be discarded.
const MAX_HELD: usize = 4 << 20;
/// Sent bytes the writer thread may have queued before a sender waits, so
/// preparing a held batch can overlap the device writing this queue.
#[cfg(target_os = "macos")]
const MAX_QUEUED: usize = 8 << 20;
// Large enough to match exFAT's buffered writes on a USB SSD, with the daemon
// below 174 MiB RSS.
#[cfg(target_os = "linux")]
const MAX_QUEUED: usize = 160 << 20;
/// Device bytes kept requested past a sequential file-data read, in
/// `AHEAD_STEP` hints. One USB bridge reports a 32 MiB optimal transfer, so
/// the kernel's own read-ahead grows to 64 MiB windows and its first 64 MiB
/// arrive at about 150 MiB/s. Hints keep the device busy from the start.
#[cfg(target_os = "linux")]
const AHEAD: u64 = 8 << 20;
#[cfg(target_os = "linux")]
const AHEAD_STEP: u64 = 4 << 20;
/// The core reads file data in whole 64 KiB checksum units (FORMAT.md);
/// smaller reads are metadata pages, which never start or extend a stream.
#[cfg(target_os = "linux")]
const DATA_READ: usize = 1 << 16;

#[cfg(target_os = "linux")]
fn advise(file: &File, offset: u64, len: u64, advice: i32) -> Result<()> {
    let offset =
        i64::try_from(offset).map_err(|_| Error::InvalidInput("read hint offset overflow"))?;
    let len = i64::try_from(len).map_err(|_| Error::InvalidInput("read hint length overflow"))?;
    // SAFETY: the descriptor is open and fadvise takes no pointers.
    let code = unsafe { libc::posix_fadvise(file.as_raw_fd(), offset, len, advice) };
    if code != 0 {
        return Err(Error::Io {
            operation: "advise device reader",
            source: io::Error::from_raw_os_error(code),
        });
    }
    Ok(())
}

/// A whole partition or disk opened without `O_EXCL`, which would conflict with
/// the mount's own claim. Unmounted callers such as `probe` only read.
///
/// Writes are held until the next durable flush and then sent as contiguous
/// runs: a 1 MiB core write is 291 page writes. Reads see held bytes instead of
/// forcing them out. Sending on every overlapping read cost two early sends per
/// file creation and let every `discard` arrive after its page had gone.
/// The core allows any order among writes before a
/// flush, and the flush barrier itself is unchanged. Held writes are lost on
/// drop, as they may be in a crash; sent ones are written first.
///
/// Sent runs go to writer threads (see [`Queue`]), so the request thread
/// prepares the next batch while the device copies this one.
pub struct Disk {
    file: Arc<File>,
    #[cfg(target_os = "linux")]
    metadata_file: Option<File>,
    /// Where the last file-data read ended, and how far hints reach.
    #[cfg(target_os = "linux")]
    stream_end: u64,
    #[cfg(target_os = "linux")]
    hinted: u64,
    /// The disk seeks: metadata hints are batched, and the read-only
    /// metadata reader keeps read-ahead.
    #[cfg(target_os = "linux")]
    rotational: bool,
    /// Metadata hints a rotating disk holds until the next read, to issue
    /// them sorted and merged.
    #[cfg(target_os = "linux")]
    hints: Vec<(u64, u64)>,
    mode: MountMode,
    geometry: Geometry,
    pending: BTreeMap<u64, Vec<u8>>,
    held: usize,
    queue: Arc<Queue>,
    writers: Vec<JoinHandle<()>>,
    #[cfg(target_os = "macos")]
    reads: ReadCache,
    /// Background reads of the windows after recent ones, newest last.
    #[cfg(target_os = "macos")]
    prefetch: Vec<Prefetch>,
    /// Background reads of hinted metadata units, oldest first.
    #[cfg(target_os = "macos")]
    metadata: VecDeque<Prefetch>,
}

impl Disk {
    pub fn open(path: &Path, mode: MountMode) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(mode == MountMode::ReadWrite)
            .open(path)
            .map_err(|source| Error::Io {
                operation: "open block device",
                source,
            })?;
        let geometry = geometry(&file)?;
        // A second daemon on the same disk would publish over this one.
        // SAFETY: flock takes an open descriptor and no pointers.
        #[cfg(target_os = "macos")]
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::Io {
                operation: "lock device",
                source: io::Error::last_os_error(),
            });
        }
        Self::with_file(file, mode, geometry)
    }

    /// Adopt an opened device. A read-only one never writes, so it gets no
    /// writer threads.
    fn with_file(file: File, mode: MountMode, geometry: Geometry) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let rotational = rotational(&file);
        let file = Arc::new(file);
        let queue = Arc::new(Queue::default());
        let count = if mode == MountMode::ReadWrite {
            WRITERS
        } else {
            0
        };
        let writers = (0..count)
            .map(|_| {
                let (queue, file) = (Arc::clone(&queue), Arc::clone(&file));
                std::thread::Builder::new()
                    .name("carryfs-writer".into())
                    .spawn(move || {
                        queue.write(|offset, pieces| {
                            write_run(&file, offset, pieces)?;
                            #[cfg(target_os = "linux")]
                            start_writeback(&file, offset, run_len(pieces))?;
                            Ok(())
                        })
                    })
            })
            .collect::<io::Result<Vec<_>>>()
            .map_err(|source| Error::Io {
                operation: "start writer",
                source,
            })?;
        Ok(Self {
            file,
            #[cfg(target_os = "linux")]
            metadata_file: None,
            #[cfg(target_os = "linux")]
            stream_end: u64::MAX,
            #[cfg(target_os = "linux")]
            hinted: 0,
            #[cfg(target_os = "linux")]
            rotational,
            #[cfg(target_os = "linux")]
            hints: Vec::new(),
            mode,
            geometry,
            pending: BTreeMap::new(),
            held: 0,
            queue,
            writers,
            #[cfg(target_os = "macos")]
            reads: ReadCache::new(geometry.byte_len),
            #[cfg(target_os = "macos")]
            prefetch: Vec::new(),
            #[cfg(target_os = "macos")]
            metadata: VecDeque::new(),
        })
    }

    /// Whether the disk seeks, so that reading ahead of requests would make
    /// them wait behind it.
    #[cfg(target_os = "linux")]
    pub fn seeks(&self) -> bool {
        self.rotational
    }

    /// Use exact metadata hints on cached read-only Linux mounts.
    #[cfg(target_os = "linux")]
    pub fn enable_metadata_prefetch(&mut self, path: &Path) -> Result<()> {
        if self.mode != MountMode::ReadOnly {
            return Err(Error::InvalidInput(
                "metadata prefetch requires read-only mode",
            ));
        }
        // Open separately: duplicated descriptors share read-ahead state.
        // File data keeps the normal policy on `self.file`.
        let file = File::open(path).map_err(|source| Error::Io {
            operation: "open metadata reader",
            source,
        })?;
        // A rotating disk keeps read-ahead: neighbouring pages cost little
        // next to a seek each.
        if !self.rotational {
            advise(&file, 0, 0, libc::POSIX_FADV_RANDOM)?;
        }
        self.metadata_file = Some(file);
        Ok(())
    }

    fn write_pending(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.pending);
        self.held = 0;
        for (offset, pieces) in runs(pending) {
            #[cfg(target_os = "macos")]
            self.reads.sent(offset, run_len(&pieces));
            self.queue.send(offset, pieces)?;
        }
        Ok(())
    }

    /// Hold one write until the next flush or `MAX_HELD`.
    fn hold(&mut self, offset: u64, data: Vec<u8>) -> Result<()> {
        if self.mode == MountMode::ReadOnly {
            return Err(Error::ReadOnly);
        }
        self.check_range(offset, data.len())?;
        let end = offset + data.len() as u64;
        // Core writes never overlap before a flush; send any that would, so a
        // later write always wins.
        let overlaps = self
            .pending
            .range(..end)
            .next_back()
            .is_some_and(|(start, old)| start + old.len() as u64 > offset);
        if overlaps {
            self.write_pending()?;
        }
        #[cfg(target_os = "macos")]
        {
            self.reads.wrote(offset, &data);
            // A prefetch listed its overlays before this write existed.
            self.prefetch
                .retain(|ahead| !ahead.overlaps(offset, data.len()));
            self.metadata
                .retain(|ahead| !ahead.overlaps(offset, data.len()));
        }
        let len = data.len();
        self.pending.insert(offset, data);
        self.held += len;
        if self.held >= MAX_HELD {
            self.write_pending()?;
        }
        Ok(())
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<()> {
        let sector = u64::from(self.geometry.logical_sector_bytes);
        let len = u64::try_from(len).map_err(|_| Error::InvalidInput("I/O length overflow"))?;
        let end = offset
            .checked_add(len)
            .ok_or(Error::InvalidInput("I/O range overflow"))?;
        if sector == 0 || !offset.is_multiple_of(sector) || !len.is_multiple_of(sector) {
            return Err(Error::InvalidInput("I/O is not sector aligned"));
        }
        if end > self.geometry.byte_len {
            return Err(Error::InvalidInput("I/O range is outside device"));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Disk {
    /// Read the window at `start` in the background, as far as `end`, so the
    /// device works while the caller consumes the one before it.
    fn start_prefetch(&mut self, start: u64, end: u64) {
        let len = end
            .min(self.geometry.byte_len)
            .saturating_sub(start)
            .min(READAHEAD as u64) as usize;
        if len == 0 || self.prefetch.iter().any(|ahead| ahead.start == start) {
            return;
        }
        // Without a thread the next window is read when asked for.
        if let Some(ahead) = self.read_ahead(start, len) {
            if self.prefetch.len() == PREFETCHES {
                self.prefetch.remove(0);
            }
            self.prefetch.push(ahead);
        }
    }

    /// Read through the cache. `run` marks file data and carries its hints:
    /// the device offset where its extent ends and, near that end, where the
    /// file continues.
    ///
    /// A data read that no window holds fetches a window from the read and
    /// starts the one after it in the background, so the device works while
    /// the caller consumes this one. Both stop at the extent's end. What
    /// lies past it is another file's, and reading it ahead made every read
    /// of a file under 4 MiB cost 8 MiB of device time: 256 files of 0.2 to
    /// 3 MiB, shuffled, read at 40 MiB/s on a USB SSD against exFAT's 224.
    ///
    /// File data is taken from background windows wherever the read reaches
    /// them, not only at their starts: a file's next extent can begin
    /// mid-unit, and then every later read straddles two windows. A stream
    /// that has used one keeps `AHEAD_WINDOWS` requested so the device never
    /// waits for the caller, and where the file continues is requested as
    /// soon as the core names it.
    fn read_through(
        &mut self,
        offset: u64,
        out: &mut [u8],
        run: Option<(u64, Option<u64>)>,
    ) -> Result<()> {
        self.check_range(offset, out.len())?;
        let before = self.reads.window();
        let mut ahead_windows = 1;
        let mut done = 0;
        while done < out.len() {
            let at = offset + done as u64;
            if run.is_some()
                && !self.reads.holds(at)
                && let Some(index) = self.prefetch.iter().position(|ahead| ahead.holds(at))
            {
                let ahead = self.prefetch.remove(index);
                let start = ahead.start;
                self.reads.keep_window(start, ahead.finish(&self.pending)?);
                ahead_windows = AHEAD_WINDOWS;
            }
            match self.reads.take(at, &mut out[done..]) {
                0 => break,
                taken => done += taken,
            }
        }
        // Metadata names no run, and its windows are not read ahead.
        let (run_end, next_run) = run.unwrap_or_default();
        if done < out.len() {
            let limit = if run.is_some() { run_end } else { u64::MAX };
            let (file, queue, pending) = (&self.file, &self.queue, &self.pending);
            let metadata = &mut self.metadata;
            self.reads
                .read(offset + done as u64, &mut out[done..], limit, |at, buf| {
                    let started = |ahead: &Prefetch| ahead.start == at && ahead.len == buf.len();
                    match metadata
                        .iter()
                        .position(started)
                        .and_then(|index| metadata.remove(index))
                    {
                        Some(ahead) => {
                            buf.copy_from_slice(&ahead.finish(pending)?);
                            Ok(())
                        }
                        None => fetch(file, queue, pending, at, buf),
                    }
                })?;
        }
        // A hint may be any value; one the device could not read is dropped.
        // The next extent's own end is not known yet.
        if let Some(at) = next_run.filter(|at| self.check_range(*at, 0).is_ok()) {
            self.start_prefetch(at, u64::MAX);
        }
        let (start, len) = self.reads.window();
        if (start, len) != before && len > 0 {
            for window in 0..ahead_windows {
                let at = (start + len as u64).saturating_add(window * READAHEAD as u64);
                if at < run_end {
                    self.start_prefetch(at, run_end);
                }
            }
        }
        Ok(())
    }

    /// Start reading `len` bytes at `start` on a thread of its own, listing
    /// queued runs and copying held writes first, as `fetch` does.
    fn read_ahead(&self, start: u64, len: usize) -> Option<Prefetch> {
        let end = start + len as u64;
        // After a failed write the next real read reports it.
        let queued = self.queue.overlapping(start, len).ok()?;
        let held = self
            .pending
            .range(..end)
            .filter(|(at, data)| **at + data.len() as u64 > start)
            .map(|(at, data)| (*at, data.clone()))
            .collect();
        let file = Arc::clone(&self.file);
        let read = std::thread::Builder::new()
            .name("carryfs-prefetch".into())
            .spawn(move || {
                let mut bytes = vec![0; len];
                read_device(&file, start, &mut bytes).map(|()| bytes)
            })
            .ok()?;
        Some(Prefetch {
            start,
            len,
            queued,
            held,
            read,
        })
    }
}

/// A data window read in the background, with the queued runs and held
/// writes that were newer than the device when the read began.
#[cfg(target_os = "macos")]
struct Prefetch {
    start: u64,
    len: usize,
    queued: Vec<Run>,
    held: BTreeMap<u64, Vec<u8>>,
    read: JoinHandle<io::Result<Vec<u8>>>,
}

#[cfg(target_os = "macos")]
impl Prefetch {
    fn overlaps(&self, offset: u64, len: usize) -> bool {
        offset < self.start + self.len as u64 && self.start < offset + len as u64
    }

    fn holds(&self, at: u64) -> bool {
        self.start <= at && at < self.start + self.len as u64
    }

    /// Wait for the read and lay newer bytes over it: the listed runs, the
    /// copied held writes, then writes held now.
    fn finish(self, pending: &BTreeMap<u64, Vec<u8>>) -> Result<Vec<u8>> {
        let mut bytes = self
            .read
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("prefetch thread panicked")))
            .map_err(|source| Error::Io {
                operation: "read_exact_at",
                source,
            })?;
        for run in &self.queued {
            overlay_run(run, self.start, &mut bytes);
        }
        overlay(&self.held, self.start, &mut bytes);
        overlay(pending, self.start, &mut bytes);
        Ok(bytes)
    }
}

impl BlockDevice for Disk {
    fn mode(&self) -> MountMode {
        self.mode
    }

    fn geometry(&mut self) -> Result<Geometry> {
        Ok(self.geometry)
    }

    /// A file-data read that continues the previous one keeps `AHEAD` bytes
    /// after it requested. Hints only fill the page cache; `fetch` still
    /// overlays newer writes.
    #[cfg(target_os = "linux")]
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.check_range(offset, out.len())?;
        self.send_hints()?;
        if out.len() >= DATA_READ {
            let end = offset + out.len() as u64;
            if offset == self.stream_end {
                let target = (end + AHEAD).min(self.geometry.byte_len);
                self.hinted = self.hinted.max(end);
                while self.hinted < target {
                    let len = AHEAD_STEP.min(self.geometry.byte_len - self.hinted);
                    advise(&self.file, self.hinted, len, libc::POSIX_FADV_WILLNEED)?;
                    self.hinted += len;
                }
            }
            self.stream_end = end;
        }
        fetch(&self.file, &self.queue, &self.pending, offset, out)
    }

    #[cfg(target_os = "linux")]
    fn read_metadata_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.check_range(offset, out.len())?;
        self.send_hints()?;
        match &self.metadata_file {
            Some(file) => file.read_exact_at(out, offset).map_err(|source| Error::Io {
                operation: "read metadata",
                source,
            }),
            None => self.read_exact_at(offset, out),
        }
    }

    /// Writable mounts hint through the main descriptor: `WILLNEED` fills
    /// the page cache without changing its read-ahead state, and reads still
    /// overlay held writes. A rotating disk would seek for each scattered
    /// hint in turn, so it holds hints until the next read and sends them in
    /// disk order.
    #[cfg(target_os = "linux")]
    fn prefetch_metadata(&mut self, offset: u64, len: usize) -> Result<()> {
        self.check_range(offset, len)?;
        if len == 0 {
            return Ok(());
        }
        if self.rotational {
            self.hints.push((offset, offset + len as u64));
        } else {
            let file = self.metadata_file.as_ref().unwrap_or(&self.file);
            advise(file, offset, len as u64, libc::POSIX_FADV_WILLNEED)?;
        }
        Ok(())
    }

    /// Metadata, which names no run: the core reads file data through
    /// `read_data_exact_at`.
    #[cfg(target_os = "macos")]
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.read_through(offset, out, None)
    }

    #[cfg(target_os = "macos")]
    fn read_data_exact_at(
        &mut self,
        offset: u64,
        out: &mut [u8],
        run_end: u64,
        next_run: Option<u64>,
    ) -> Result<()> {
        self.read_through(offset, out, Some((run_end, next_run)))
    }

    /// The raw node has no page cache to hint, so the hinted units are read
    /// on background threads, `METADATA_READS` at once; a read that needs one
    /// waits for it. The writable open's full check gains, and so does a cold
    /// listing on a read-only mount, whose pages otherwise arrive one
    /// synchronous unit at a time: 10,000 names on a USB SSD took 16.9 ms
    /// against 29.9.
    #[cfg(target_os = "macos")]
    fn prefetch_metadata(&mut self, offset: u64, len: usize) -> Result<()> {
        self.check_range(offset, len)?;
        for (start, unit) in self.reads.uncached_units(offset, len) {
            if self.metadata.iter().any(|ahead| ahead.start == start) {
                continue;
            }
            if self.metadata.len() == METADATA_READS
                && let Some(oldest) = self.metadata.pop_front()
            {
                let at = oldest.start;
                // A hint never replaces a read: if this one failed, the read
                // that needs the unit fetches it again and reports the error.
                if let Ok(bytes) = oldest.finish(&self.pending) {
                    self.reads.fill(at, &bytes);
                }
            }
            if let Some(ahead) = self.read_ahead(start, unit) {
                self.metadata.push_back(ahead);
            }
        }
        Ok(())
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.hold(offset, data.to_vec())
    }

    /// Keep the page's buffer as the held write; `into_vec` does not copy.
    fn write_page(&mut self, offset: u64, page: Box<[u8; 4096]>) -> Result<()> {
        self.hold(offset, (page as Box<[u8]>).into_vec())
    }

    /// Drop a dead page's held write. Copy-on-write supersedes most metadata
    /// pages within one checkpoint interval, and no root references a
    /// superseded one. A page already sent needs nothing.
    fn discard(&mut self, offset: u64) {
        #[cfg(target_os = "macos")]
        self.reads.discarded(offset);
        if let Some(data) = self.pending.remove(&offset) {
            self.held -= data.len();
        }
    }

    fn flush_durable(&mut self) -> Result<()> {
        self.write_pending()?;
        self.queue.drain()?;
        flush_device(&self.file).map_err(|source| Error::Io {
            operation: "flush_durable",
            source,
        })
    }
}

impl Drop for Disk {
    /// Queued runs were already sent, so the writer finishes them before it
    /// exits. Unmount has flushed them before this point.
    fn drop(&mut self) {
        self.queue.close();
        for writer in self.writers.drain(..) {
            if writer.join().is_err() {
                eprintln!("carryfs: device writer thread panicked");
            }
        }
    }
}

/// Hinted ranges closer than this are read as one on a rotating disk: a
/// seek costs more than reading the gap. 1 MiB beat 256 KiB at 100,000 and
/// 1,000,000 files on a hard disk.
#[cfg(target_os = "linux")]
const HINT_GAP: u64 = 1 << 20;

#[cfg(target_os = "linux")]
impl Disk {
    /// Issue held hints in disk order.
    fn send_hints(&mut self) -> Result<()> {
        let file = self.metadata_file.as_ref().unwrap_or(&self.file);
        for (start, end) in merge_hints(std::mem::take(&mut self.hints)) {
            advise(file, start, end - start, libc::POSIX_FADV_WILLNEED)?;
        }
        Ok(())
    }
}

/// Sort `start..end` ranges and merge those less than `HINT_GAP` apart.
#[cfg(target_os = "linux")]
fn merge_hints(mut hints: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    hints.sort_unstable();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for (start, end) in hints {
        match runs.last_mut() {
            Some(run) if start <= run.1.saturating_add(HINT_GAP) => run.1 = run.1.max(end),
            _ => runs.push((start, end)),
        }
    }
    runs
}

/// Whether the disk under `file` reports `queue/rotational` in sysfs. An
/// image file or an unreadable entry counts as not rotating, which keeps the
/// default hinting.
#[cfg(target_os = "linux")]
fn rotational(file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(number) = file.metadata().map(|metadata| metadata.rdev()) else {
        return false;
    };
    let Ok(device) = Path::new("/sys/dev/block")
        .join(format!("{}:{}", libc::major(number), libc::minor(number)))
        .canonicalize()
    else {
        return false;
    };
    let disk = match device.parent() {
        Some(parent) if device.join("partition").exists() => parent,
        _ => &device,
    };
    std::fs::read_to_string(disk.join("queue/rotational")).is_ok_and(|value| value.trim() == "1")
}

/// Loop's buffered path can report a positive short backing write as complete.
/// One aligned page per request makes backing ENOSPC fail before publication.
/// Restore the previous bound after the writers stop; physical devices are unchanged.
#[cfg(target_os = "linux")]
pub(crate) fn limit_loop_requests(file: &File) -> Result<Option<LoopLimit>> {
    use std::os::unix::fs::MetadataExt;

    let prepare = || -> io::Result<Option<LoopLimit>> {
        let number = file.metadata()?.rdev();
        let device = Path::new("/sys/dev/block")
            .join(format!("{}:{}", libc::major(number), libc::minor(number)))
            .canonicalize()?;
        let partition = device.join("partition").exists();
        let disk = if partition {
            device
                .parent()
                .ok_or_else(|| io::Error::other("partition lacks disk"))?
        } else {
            &device
        };
        if !disk.join("loop").is_dir() {
            return Ok(None);
        }
        let read_number = |path: &Path| -> io::Result<u64> {
            std::fs::read_to_string(path)?.trim().parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid sysfs block number")
            })
        };
        let offset = read_number(&disk.join("loop/offset"))?;
        let start = if partition {
            read_number(&device.join("start"))?
        } else {
            0
        };
        let aligned = start
            .checked_mul(512)
            .and_then(|start| start.checked_add(offset));
        if !aligned.is_some_and(|offset| offset.is_multiple_of(4096)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "writable loop offset must align to 4096 bytes",
            ));
        }
        let limit = disk.join("queue/max_sectors_kb");
        let restore = LoopLimit {
            previous: std::fs::read_to_string(&limit)?,
            path: limit,
        };
        std::fs::write(&restore.path, b"4\n")?;
        if !(1..=4).contains(&read_number(&restore.path)?) {
            return Err(io::Error::other("kernel did not bound loop requests"));
        }
        Ok(Some(restore))
    };
    prepare().map_err(|source| Error::Io {
        operation: "prepare loop device writes",
        source,
    })
}

#[cfg(target_os = "linux")]
pub(crate) struct LoopLimit {
    path: std::path::PathBuf,
    previous: String,
}

#[cfg(target_os = "linux")]
impl Drop for LoopLimit {
    fn drop(&mut self) {
        if let Err(error) = std::fs::write(&self.path, &self.previous)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("carryfs: restore loop request limit: {error}");
        }
    }
}

/// Sector sizes and length of a Linux block device.
#[cfg(target_os = "linux")]
fn geometry(file: &File) -> Result<Geometry> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::FileTypeExt;

    let metadata = file.metadata().map_err(|source| Error::Io {
        operation: "stat block device",
        source,
    })?;
    if !metadata.file_type().is_block_device() {
        return Err(Error::InvalidInput("device is not a block device"));
    }
    // Block devices report their length as the end-of-file seek position.
    let byte_len = (&*file)
        .seek(SeekFrom::End(0))
        .map_err(|source| Error::Io {
            operation: "read block device length",
            source,
        })?;
    let mut logical: libc::c_int = 0;
    let mut physical: libc::c_uint = 0;
    // SAFETY: each request writes one value of the pointed-to type, which
    // lives for the call, and the descriptor is an open block device.
    let status = unsafe {
        [
            libc::ioctl(file.as_raw_fd(), libc::BLKSSZGET, &mut logical),
            libc::ioctl(file.as_raw_fd(), libc::BLKPBSZGET, &mut physical),
        ]
    };
    if status.contains(&-1) {
        return Err(Error::Io {
            operation: "read block device sector sizes",
            source: io::Error::last_os_error(),
        });
    }
    Ok(Geometry {
        byte_len,
        logical_sector_bytes: u32::try_from(logical).map_err(|_| Error::InvalidGeometry)?,
        physical_sector_bytes: physical,
    })
}

// xnu bsd/sys/disk.h, as in macos/src/raw.rs.
#[cfg(target_os = "macos")]
const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
#[cfg(target_os = "macos")]
const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
#[cfg(target_os = "macos")]
const DKIOCGETPHYSICALBLOCKSIZE: libc::c_ulong = 0x4004_644d;
#[cfg(target_os = "macos")]
const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416;

/// Sector sizes and length of a macOS disk node. The daemon opens the raw
/// `/dev/rdiskN` node, so reads and writes bypass the buffer cache that the
/// kernel's own file cache above the mount already provides.
#[cfg(target_os = "macos")]
fn geometry(file: &File) -> Result<Geometry> {
    let fd = file.as_raw_fd();
    let mut logical: u32 = 0;
    let mut physical: u32 = 0;
    let mut blocks: u64 = 0;
    // SAFETY: each call writes one integer of the size its ioctl declares,
    // into a local of exactly that type, on an open descriptor.
    let ok = unsafe {
        libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut logical) == 0
            && libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut blocks) == 0
            && libc::ioctl(fd, DKIOCGETPHYSICALBLOCKSIZE, &mut physical) == 0
    };
    if !ok {
        return Err(Error::Io {
            operation: "read disk geometry",
            source: io::Error::last_os_error(),
        });
    }
    Ok(Geometry {
        byte_len: blocks
            .checked_mul(u64::from(logical))
            .ok_or(Error::InvalidGeometry)?,
        logical_sector_bytes: logical,
        physical_sector_bytes: physical,
    })
}

/// Start writing a run's page-cache pages to the device without waiting.
/// Without this, the first `fsync` of each checkpoint wrote back all it
/// covered on the request thread: then up to 64 MiB, which stalled a 256 MiB write
/// for about 100 ms three times. Started here on the writer thread, a 256 MiB
/// write ran at 416 MiB/s against 327. The barrier
/// is unchanged: `fsync` still waits for every page.
#[cfg(target_os = "linux")]
fn start_writeback(file: &File, offset: u64, len: usize) -> io::Result<()> {
    let overflow = || io::Error::new(io::ErrorKind::InvalidInput, "offset overflow");
    let offset = libc::off64_t::try_from(offset).map_err(|_| overflow())?;
    let len = libc::off64_t::try_from(len).map_err(|_| overflow())?;
    // SAFETY: sync_file_range takes an open descriptor and no pointers.
    if unsafe { libc::sync_file_range(file.as_raw_fd(), offset, len, libc::SYNC_FILE_RANGE_WRITE) }
        != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Ask the drive to write out its volatile cache.
#[cfg(target_os = "linux")]
fn flush_device(file: &File) -> io::Result<()> {
    #[cfg(feature = "fault-injection")]
    injected_failure("CARRYFS_FAIL_FLUSHES")?;
    // fsync on a block device issues a cache flush to the drive.
    file.sync_all()
}

/// Ask the drive to write out its volatile cache. The raw node has no buffer
/// cache of its own, so this is the whole barrier.
#[cfg(target_os = "macos")]
fn flush_device(file: &File) -> io::Result<()> {
    #[cfg(feature = "fault-injection")]
    injected_failure("CARRYFS_FAIL_FLUSHES")?;
    // SAFETY: DKIOCSYNCHRONIZECACHE takes no argument, on an open descriptor.
    if unsafe { libc::ioctl(file.as_raw_fd(), DKIOCSYNCHRONIZECACHE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Fail with EIO while the file that `variable` names exists, as a device that
/// reports errors would. Disk images cannot stand in: on a full backing volume
/// they acknowledge writes and flushes and lose the data.
#[cfg(feature = "fault-injection")]
fn injected_failure(variable: &str) -> io::Result<()> {
    match std::env::var_os(variable) {
        Some(path) if Path::new(&path).exists() => Err(io::Error::from_raw_os_error(libc::EIO)),
        _ => Ok(()),
    }
}

/// Read `out` at `offset`, splitting a read of 2 MiB or more into `READERS`
/// concurrent ones so the device has more than one request in flight.
fn read_device(file: &File, offset: u64, out: &mut [u8]) -> io::Result<()> {
    const SPLIT: usize = 1 << 20;
    if out.len() < 2 * SPLIT {
        return file.read_exact_at(out, offset);
    }
    let part = out.len().div_ceil(READERS).next_multiple_of(4096);
    std::thread::scope(|scope| {
        let reads: Vec<_> = out
            .chunks_mut(part)
            .enumerate()
            .map(|(index, chunk)| {
                scope.spawn(move || file.read_exact_at(chunk, offset + (index * part) as u64))
            })
            .collect();
        reads.into_iter().try_for_each(|read| {
            read.join()
                .unwrap_or_else(|_| Err(io::Error::other("reader panicked")))
        })
    })
}

/// Read from the device, then lay queued runs and held writes over the bytes,
/// so a read never returns bytes older than a write. std retries EINTR and
/// reports zero progress as UnexpectedEof.
fn fetch(
    file: &File,
    queue: &Queue,
    pending: &BTreeMap<u64, Vec<u8>>,
    offset: u64,
    out: &mut [u8],
) -> Result<()> {
    // Listed before the device is read, so a run the writer finishes in
    // between is overlaid with the same bytes rather than missed.
    let queued = queue.overlapping(offset, out.len())?;
    read_device(file, offset, out).map_err(|source| Error::Io {
        operation: "read_exact_at",
        source,
    })?;
    for run in &queued {
        overlay_run(run, offset, out);
    }
    overlay(pending, offset, out);
    Ok(())
}

/// Copy held writes over `out`, which was just read from the device at
/// `offset`, so a read never returns bytes older than a held write. Held
/// writes never overlap one another, so their ends ascend with their
/// starts, and the scan back from `end` stops at the first that ends
/// before `offset`.
fn overlay(pending: &BTreeMap<u64, Vec<u8>>, offset: u64, out: &mut [u8]) {
    let end = offset + out.len() as u64;
    for (&at, data) in pending.range(..end).rev() {
        let data_end = at + data.len() as u64;
        if data_end <= offset {
            break;
        }
        let (low, high) = (at.max(offset), data_end.min(end));
        out[(low - offset) as usize..(high - offset) as usize]
            .copy_from_slice(&data[(low - at) as usize..(high - at) as usize]);
    }
}

/// Group writes that end where the next begins into runs of at most `MAX_RUN`,
/// keeping each write's buffer: the writer sends a run as one `pwritev`, so
/// nothing is copied into a merged buffer. A write longer than that is sent
/// alone.
fn runs(pending: BTreeMap<u64, Vec<u8>>) -> Vec<(u64, Vec<Vec<u8>>)> {
    let mut runs: Vec<(u64, Vec<Vec<u8>>, usize)> = Vec::new();
    for (offset, data) in pending {
        match runs.last_mut() {
            Some((start, pieces, len))
                if *start + *len as u64 == offset && *len + data.len() <= MAX_RUN =>
            {
                *len += data.len();
                pieces.push(data);
            }
            _ => {
                let len = data.len();
                runs.push((offset, vec![data], len));
            }
        }
    }
    runs.into_iter()
        .map(|(offset, pieces, _)| (offset, pieces))
        .collect()
}

/// A sent run: its device offset and its writes in order.
type Run = Arc<(u64, Vec<Vec<u8>>)>;

/// The oldest run that no writer has taken and that overlaps no older run.
fn next_run(state: &Queued) -> Option<Run> {
    let overlaps =
        |a: &Run, b: &Run| a.0 < b.0 + run_len(&b.1) as u64 && b.0 < a.0 + run_len(&a.1) as u64;
    state
        .runs
        .iter()
        .enumerate()
        .find(|(index, run)| {
            !state.writing.iter().any(|taken| Arc::ptr_eq(taken, run))
                && !state
                    .runs
                    .iter()
                    .take(*index)
                    .any(|older| overlaps(older, run))
        })
        .map(|(_, run)| Arc::clone(run))
}

fn run_len(pieces: &[Vec<u8>]) -> usize {
    pieces.iter().map(Vec::len).sum()
}

/// Copy `run` over `out`, which was read at `offset`.
fn overlay_run(run: &Run, offset: u64, out: &mut [u8]) {
    let end = offset + out.len() as u64;
    let mut at = run.0;
    for piece in &run.1 {
        let piece_end = at + piece.len() as u64;
        let (low, high) = (at.max(offset), piece_end.min(end));
        if low < high {
            out[(low - offset) as usize..(high - offset) as usize]
                .copy_from_slice(&piece[(low - at) as usize..(high - at) as usize]);
        }
        at = piece_end;
    }
}

/// Write one run with `pwritev`, retrying `EINTR` and short writes.
fn write_run(file: &File, mut offset: u64, pieces: &[Vec<u8>]) -> io::Result<()> {
    #[cfg(feature = "fault-injection")]
    injected_failure("CARRYFS_FAIL_WRITES")?;
    let mut slices: Vec<IoSlice<'_>> = pieces.iter().map(|piece| IoSlice::new(piece)).collect();
    let mut slices = &mut slices[..];
    while !slices.is_empty() {
        let count = slices.len().min(1024) as libc::c_int; // IOV_MAX on Linux and macOS
        let at = libc::off_t::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
        // SAFETY: `IoSlice` is ABI-compatible with `iovec` on Unix, and the
        // `count` slices it points at stay borrowed for the call.
        let written = unsafe { libc::pwritev(file.as_raw_fd(), slices.as_ptr().cast(), count, at) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        offset += written as u64;
        IoSlice::advance_slices(&mut slices, written as usize);
    }
    Ok(())
}

/// Sent runs, oldest first, and the thread that writes them.
///
/// A synchronous `pwrite` on the FUSE request thread left the device idle
/// while the core prepared data and the core idle while the kernel copied it.
/// A run stays listed until its write
/// returns, so a read that lists the queue before it reads the device sees
/// every byte not yet written. `flush_durable` waits for the queue to empty
/// before `fsync`, so each publication barrier still covers every earlier
/// write.
#[derive(Default)]
struct Queue {
    state: Mutex<Queued>,
    changed: Condvar,
}

#[derive(Default)]
struct Queued {
    /// Runs not yet written, oldest first, including those being written.
    runs: VecDeque<Run>,
    /// Runs a writer thread has taken.
    writing: Vec<Run>,
    bytes: usize,
    /// The first failed write's `errno`. Later runs are dropped: the core
    /// faults the volume when it next sends or flushes.
    failure: Option<i32>,
    closed: bool,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, Queued> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, Queued>) -> MutexGuard<'a, Queued> {
        self.changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a run, waiting while `MAX_QUEUED` bytes are ahead of it.
    fn send(&self, offset: u64, pieces: Vec<Vec<u8>>) -> Result<()> {
        let len = run_len(&pieces);
        let mut state = self.lock();
        while state.failure.is_none() && !state.runs.is_empty() && state.bytes + len > MAX_QUEUED {
            state = self.wait(state);
        }
        if let Some(code) = state.failure {
            return Err(failed(code));
        }
        state.bytes += len;
        state.runs.push_back(Arc::new((offset, pieces)));
        self.changed.notify_all();
        Ok(())
    }

    /// Wait until every queued run is written, or report the first failure.
    fn drain(&self) -> Result<()> {
        let mut state = self.lock();
        while state.failure.is_none() && !state.runs.is_empty() {
            state = self.wait(state);
        }
        match state.failure {
            Some(code) => Err(failed(code)),
            None => Ok(()),
        }
    }

    /// The queued runs overlapping `offset..offset + len`, oldest first. A
    /// failed write cleared the queue, so the device no longer holds what the
    /// core was told it wrote: a read then reports that failure instead of
    /// returning the older bytes.
    fn overlapping(&self, offset: u64, len: usize) -> Result<Vec<Run>> {
        let end = offset + len as u64;
        let state = self.lock();
        if let Some(code) = state.failure {
            return Err(failed(code));
        }
        Ok(state
            .runs
            .iter()
            .filter(|run| run.0 < end && run.0 + run_len(&run.1) as u64 > offset)
            .cloned()
            .collect())
    }

    /// Write queued runs with `write` until closed and empty. Several writers
    /// may run at once: each takes the oldest run no other has taken that
    /// overlaps no older unwritten run, so overlapping runs still reach the
    /// device in order.
    fn write(&self, mut write: impl FnMut(u64, &[Vec<u8>]) -> io::Result<()>) {
        let mut state = self.lock();
        loop {
            let Some(run) = next_run(&state) else {
                if state.closed && state.runs.is_empty() {
                    return;
                }
                state = self.wait(state);
                continue;
            };
            state.writing.push(Arc::clone(&run));
            drop(state);
            // A panic is a failed write. Unwinding out of this thread would
            // leave the run queued, and `send` and `drain` waiting for it.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(run.0, &run.1)))
                    .unwrap_or_else(|_| Err(io::Error::other("device writer panicked")));
            state = self.lock();
            state.writing.retain(|other| !Arc::ptr_eq(other, &run));
            // A failure cleared the queue; this run is no longer in it.
            if let Some(index) = state.runs.iter().position(|other| Arc::ptr_eq(other, &run)) {
                state.runs.remove(index);
                state.bytes -= run_len(&run.1);
            }
            if let Err(error) = result {
                state
                    .failure
                    .get_or_insert(error.raw_os_error().unwrap_or(libc::EIO));
                state.runs.clear();
                state.bytes = 0;
            }
            self.changed.notify_all();
        }
    }

    fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }
}

fn failed(code: i32) -> Error {
    Error::Io {
        operation: "write_all_at",
        source: io::Error::from_raw_os_error(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_writes_merge_into_bounded_runs() {
        let page = |byte| vec![byte; 4096];
        let mut pending = BTreeMap::new();
        for index in 0..300u64 {
            pending.insert(index * 4096, page(index as u8));
        }
        pending.insert(400 * 4096, page(9));
        let runs = runs(pending);
        let shape: Vec<_> = runs
            .iter()
            .map(|(offset, pieces)| (*offset, run_len(pieces)))
            .collect();
        assert_eq!(
            shape,
            [(0, MAX_RUN), (256 * 4096, 44 * 4096), (400 * 4096, 4096)]
        );
        assert_eq!(runs[1].1[0], page(0));
        assert_eq!(runs[1].1[1][0], 1);
        assert_eq!(runs[2].1, [page(9)]);
    }

    /// Only a unit-sized read that continues the last one hints ahead, and
    /// hints stop at the device end.
    #[cfg(target_os = "linux")]
    #[test]
    fn sequential_data_reads_hint_ahead_and_pages_do_not() {
        let path = std::env::temp_dir().join(format!("carryfs-disk-ahead-{}", std::process::id()));
        let mut disk = disk_over(&path);
        let mut unit = vec![0; DATA_READ];
        disk.read_exact_at(0, &mut unit).unwrap();
        assert_eq!(disk.hinted, 0);
        disk.read_exact_at(1 << 16, &mut [0; 4096]).unwrap();
        disk.read_exact_at(1 << 16, &mut unit).unwrap();
        assert_eq!(disk.hinted, (2 << 16) + AHEAD);
        disk.read_exact_at((16 << 20) - (2 << 16), &mut unit)
            .unwrap();
        disk.read_exact_at((16 << 20) - (1 << 16), &mut unit)
            .unwrap();
        assert_eq!(disk.hinted, 16 << 20);
        drop(disk);
        std::fs::remove_file(path).unwrap();
    }

    /// Held hints go out sorted, with near ones merged and far ones apart.
    #[test]
    #[cfg(target_os = "linux")]
    fn rotating_disks_merge_near_hints() {
        let far = 2 * HINT_GAP;
        assert_eq!(
            merge_hints(vec![
                (far, far + 4096),
                (0, 4096),
                (8192, 12288),
                (4096, 8192)
            ]),
            [(0, 12288), (far, far + 4096)]
        );
        assert_eq!(
            merge_hints(vec![(0, 4096), (HINT_GAP, HINT_GAP + 4096)]),
            [(0, HINT_GAP + 4096)]
        );
        assert!(merge_hints(Vec::new()).is_empty());
    }

    /// An image file has no block queue, so it keeps metadata hints.
    #[test]
    #[cfg(target_os = "linux")]
    fn image_files_are_not_rotational() {
        let path = std::env::temp_dir().join(format!("carryfs-rotational-{}", std::process::id()));
        let disk = disk_over(&path);
        assert!(!rotational(&disk.file));
        drop(disk);
        std::fs::remove_file(path).unwrap();
    }

    fn disk_over(path: &Path) -> Disk {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        file.set_len(16 << 20).unwrap();
        Disk::with_file(file, MountMode::ReadWrite, geometry(16 << 20)).unwrap()
    }

    fn geometry(byte_len: u64) -> Geometry {
        Geometry {
            byte_len,
            logical_sector_bytes: 512,
            physical_sector_bytes: 512,
        }
    }

    /// A background read-ahead window lists runs and copies held writes when it
    /// starts, and a write into its range after that drops it.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_prefetched_window_sees_writes_from_before_and_after_it_began() {
        const UNIT: usize = 1 << 16;
        let path =
            std::env::temp_dir().join(format!("carryfs-disk-prefetch-{}", std::process::id()));
        let mut disk = disk_over(&path);
        let next = READAHEAD as u64;
        let second = next + READAHEAD as u64;
        disk.write_all_at(second + 4096, &[3; 4096]).unwrap();
        disk.write_pending().unwrap();
        disk.queue.drain().unwrap();
        // Held before the prefetch starts, then written out while it runs.
        disk.write_all_at(next, &[1; 4096]).unwrap();
        let mut out = vec![0; UNIT];
        disk.read_data_exact_at(0, &mut out, u64::MAX, None)
            .unwrap();
        assert_eq!(disk.prefetch.last().map(|ahead| ahead.start), Some(next));
        disk.write_pending().unwrap();
        disk.queue.drain().unwrap();
        // Held after it starts: the prefetch is dropped.
        disk.write_all_at(next + 8192, &[2; 4096]).unwrap();
        assert!(disk.prefetch.is_empty());
        // Consume the next window, as a prefetch or a fresh read.
        disk.read_data_exact_at(0, &mut out, u64::MAX, None)
            .unwrap();
        disk.read_data_exact_at(next, &mut out, u64::MAX, None)
            .unwrap();
        assert!(out[..4096].iter().all(|&byte| byte == 1));
        assert!(out[8192..12288].iter().all(|&byte| byte == 2));

        // A prefetch consumed without writes in between.
        disk.read_data_exact_at(next + UNIT as u64, &mut out, u64::MAX, None)
            .unwrap();
        assert_eq!(disk.prefetch.last().map(|ahead| ahead.start), Some(second));
        disk.read_data_exact_at(second, &mut out, u64::MAX, None)
            .unwrap();
        assert!(
            disk.prefetch
                .last()
                .is_some_and(|ahead| ahead.start > second)
        );
        assert!(out[4096..8192].iter().all(|&byte| byte == 3));
        assert!(out[..4096].iter().all(|&byte| byte == 0));
        std::fs::remove_file(&path).unwrap();
    }

    /// A stream that has used a background window requests `AHEAD_WINDOWS`
    /// windows, none of them past the end of its run.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_stream_reads_further_ahead_only_inside_its_run() {
        const UNIT: usize = 1 << 16;
        let window = READAHEAD as u64;
        for (run_end, ahead) in [
            (4 * window, vec![2 * window, 3 * window]),
            (3 * window, vec![2 * window]),
            (2 * window, vec![]),
        ] {
            let path = std::env::temp_dir().join(format!(
                "carryfs-disk-ahead-{}-{run_end}",
                std::process::id()
            ));
            let mut disk = disk_over(&path);
            let mut out = vec![0; UNIT];
            disk.read_data_exact_at(0, &mut out, run_end, None).unwrap();
            let starts = |disk: &Disk| {
                disk.prefetch
                    .iter()
                    .map(|ahead| ahead.start)
                    .collect::<Vec<_>>()
            };
            assert_eq!(starts(&disk), [window]);
            disk.read_data_exact_at(window, &mut out, run_end, None)
                .unwrap();
            assert_eq!(starts(&disk), ahead);
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// Where the file continues is requested in place of the window past
    /// the run's end, and a hint outside the device is dropped.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_stream_reads_ahead_where_its_file_continues() {
        const UNIT: usize = 1 << 16;
        let window = READAHEAD as u64;
        let path =
            std::env::temp_dir().join(format!("carryfs-disk-next-run-{}", std::process::id()));
        let mut disk = disk_over(&path);
        let mut out = vec![0; UNIT];
        let starts = |disk: &Disk| {
            disk.prefetch
                .iter()
                .map(|ahead| ahead.start)
                .collect::<Vec<_>>()
        };
        disk.read_data_exact_at(0, &mut out, window, Some(3 * window))
            .unwrap();
        assert_eq!(starts(&disk), [3 * window]);
        // The stream arrives there and consumes it.
        disk.read_data_exact_at(3 * window, &mut out, 4 * window, None)
            .unwrap();
        assert!(starts(&disk).is_empty());
        disk.read_data_exact_at(window, &mut out, 2 * window, Some(1 << 40))
            .unwrap();
        assert!(starts(&disk).is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    /// A file shorter than a window is read alone: its window ends where its
    /// extent does, and nothing after it is read ahead.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_window_ends_with_the_extent() {
        const UNIT: usize = 1 << 16;
        let path =
            std::env::temp_dir().join(format!("carryfs-disk-run-end-{}", std::process::id()));
        let mut disk = disk_over(&path);
        let mut out = vec![0; UNIT];
        let run_end = 4096 + 3 * UNIT as u64;
        disk.read_data_exact_at(4096, &mut out, run_end, None)
            .unwrap();
        assert_eq!(disk.reads.window(), (4096, 3 * UNIT));
        assert!(disk.prefetch.is_empty());
        // The rest of the file comes from that window.
        disk.read_data_exact_at(4096 + 2 * UNIT as u64, &mut out, run_end, None)
            .unwrap();
        assert!(disk.prefetch.is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    /// A data read that runs off the end of one window continues in the
    /// next, read in the background, instead of fetching a window of its own.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_data_read_across_two_windows_takes_from_both() {
        const UNIT: usize = 1 << 16;
        let window = READAHEAD as u64;
        let path = std::env::temp_dir().join(format!("carryfs-disk-stitch-{}", std::process::id()));
        let mut disk = disk_over(&path);
        disk.write_all_at(window - 4096, &[1; 4096]).unwrap();
        disk.write_all_at(window, &[2; 4096]).unwrap();
        disk.write_pending().unwrap();
        disk.queue.drain().unwrap();
        let mut out = vec![0; UNIT];
        disk.read_data_exact_at(0, &mut out, 4 * window, None)
            .unwrap();
        let mut across = vec![0; 8192];
        disk.read_data_exact_at(window - 4096, &mut across, 4 * window, None)
            .unwrap();
        assert!(across[..4096].iter().all(|&byte| byte == 1));
        assert!(across[4096..].iter().all(|&byte| byte == 2));
        // The background window became the stream's window, and the stream
        // reads on from it.
        assert_eq!(disk.reads.window(), (window, READAHEAD));
        let starts: Vec<_> = disk.prefetch.iter().map(|ahead| ahead.start).collect();
        assert_eq!(starts, [2 * window, 3 * window]);
        std::fs::remove_file(&path).unwrap();
    }

    /// Hinted metadata units are read in the background, at most
    /// `METADATA_READS` at once; a page read waits for its unit and sees held
    /// writes, and a write into a unit being read drops that read.
    #[test]
    #[cfg(target_os = "macos")]
    fn hinted_metadata_units_are_read_ahead_and_see_writes() {
        const UNIT: u64 = 1 << 16;
        let path = std::env::temp_dir().join(format!("carryfs-disk-hints-{}", std::process::id()));
        let mut disk = disk_over(&path);
        disk.write_all_at(2 * UNIT, &[4; 4096]).unwrap();
        disk.write_pending().unwrap();
        disk.queue.drain().unwrap();
        // Held before the hint: copied into the read.
        disk.write_all_at(UNIT + 4096, &[5; 4096]).unwrap();
        disk.prefetch_metadata(UNIT, 2 * UNIT as usize).unwrap();
        let started: Vec<_> = disk.metadata.iter().map(|ahead| ahead.start).collect();
        assert_eq!(started, [UNIT, 2 * UNIT]);
        // Written pages stay cached; drop the held one so its read takes
        // the unit read ahead.
        disk.reads.discarded(UNIT + 4096);
        let mut page = vec![0; 4096];
        disk.read_exact_at(UNIT + 4096, &mut page).unwrap();
        assert!(page.iter().all(|&byte| byte == 5));
        disk.read_exact_at(2 * UNIT + 8192, &mut page).unwrap();
        assert!(page.iter().all(|&byte| byte == 0));
        assert!(disk.metadata.is_empty());
        disk.read_exact_at(2 * UNIT, &mut page).unwrap();
        assert!(page.iter().all(|&byte| byte == 4));
        // Held after the hint: the read is dropped, and the page read sees it.
        disk.prefetch_metadata(4 * UNIT, 4096).unwrap();
        disk.write_all_at(4 * UNIT + 8192, &[6; 4096]).unwrap();
        assert!(disk.metadata.is_empty());
        disk.read_exact_at(4 * UNIT + 8192, &mut page).unwrap();
        assert!(page.iter().all(|&byte| byte == 6));
        // More units than the bound: the oldest are finished into the cache.
        disk.prefetch_metadata(8 * UNIT, 12 * UNIT as usize)
            .unwrap();
        assert_eq!(disk.metadata.len(), METADATA_READS);
        assert_eq!(
            disk.metadata.front().map(|ahead| ahead.start),
            Some(12 * UNIT)
        );
        disk.prefetch_metadata(8 * UNIT, 4 * UNIT as usize).unwrap();
        assert_eq!(disk.metadata.len(), METADATA_READS);
        std::fs::remove_file(&path).unwrap();
    }

    /// A read over held bytes returns them without sending anything, and a
    /// held write longer than a page is overlaid from its middle.
    #[test]
    fn a_read_over_held_bytes_returns_them_unsent() {
        let path =
            std::env::temp_dir().join(format!("carryfs-disk-overlay-{}", std::process::id()));
        let mut disk = disk_over(&path);
        disk.write_all_at(4096, &[1u8; 4096]).unwrap();
        disk.write_all_at(16384, &[2u8; 12288]).unwrap();

        // Unaligned, and covering the first held page only in part.
        let mut out = [0u8; 4096];
        disk.read_exact_at(6144, &mut out).unwrap();
        assert!(out[..2048].iter().all(|&byte| byte == 1));
        assert!(out[2048..].iter().all(|&byte| byte == 0));
        // Inside the three-page write, neither end.
        disk.read_exact_at(20480, &mut out).unwrap();
        assert!(out.iter().all(|&byte| byte == 2));
        // Before anything held.
        disk.read_exact_at(0, &mut out).unwrap();
        assert!(out.iter().all(|&byte| byte == 0));

        // Nothing reached the file, so a later discard can still drop it.
        assert_eq!(disk.pending.len(), 2);
        let mut raw = [0u8; 4096];
        disk.file.read_exact_at(&mut raw, 4096).unwrap();
        assert!(raw.iter().all(|&byte| byte == 0));
        std::fs::remove_file(&path).unwrap();
    }

    /// Held writes past `MAX_HELD` are sent without waiting for a flush, so
    /// memory stays bounded however long a write runs between checkpoints.
    #[test]
    fn held_writes_past_the_bound_are_sent() {
        let path = std::env::temp_dir().join(format!("carryfs-disk-bound-{}", std::process::id()));
        let mut disk = disk_over(&path);
        let pages = MAX_HELD / 4096;
        for index in 0..pages - 1 {
            disk.write_all_at(index as u64 * 4096, &[7u8; 4096])
                .unwrap();
        }
        assert_eq!(disk.pending.len(), pages - 1, "sent before the bound");
        disk.write_all_at((pages - 1) as u64 * 4096, &[7u8; 4096])
            .unwrap();
        assert!(disk.pending.is_empty(), "held past the bound");
        assert_eq!(disk.held, 0);
        disk.queue.drain().unwrap();
        let mut raw = [0u8; 4096];
        disk.file.read_exact_at(&mut raw, 0).unwrap();
        assert!(raw.iter().all(|&byte| byte == 7));
        std::fs::remove_file(&path).unwrap();
    }

    /// A page the core discards is never sent, and one already sent is ignored.
    #[test]
    fn a_discarded_page_is_never_sent() {
        let mut disk = Disk::with_file(
            File::open("/dev/null").unwrap(),
            MountMode::ReadWrite,
            geometry(1 << 20),
        )
        .unwrap();
        for index in 0..3u64 {
            disk.write_all_at(index * 4096, &vec![index as u8; 4096])
                .unwrap();
        }
        disk.discard(4096);
        disk.discard(1 << 19); // never held
        let shape: Vec<_> = runs(std::mem::take(&mut disk.pending))
            .iter()
            .map(|(offset, pieces)| (*offset, run_len(pieces)))
            .collect();
        assert_eq!(shape, [(0, 4096), (8192, 4096)]);
    }

    /// A queue whose writer copies into `bytes` once `hold` clears, or fails
    /// while `fail` is set.
    struct Fake {
        queue: Arc<Queue>,
        bytes: Arc<Mutex<Vec<u8>>>,
        hold: Arc<(Mutex<bool>, Condvar)>,
        fail: Arc<std::sync::atomic::AtomicBool>,
        writer: Option<JoinHandle<()>>,
    }

    impl Fake {
        fn new(len: usize) -> Self {
            let queue = Arc::new(Queue::default());
            let bytes = Arc::new(Mutex::new(vec![0u8; len]));
            let hold = Arc::new((Mutex::new(false), Condvar::new()));
            let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let writer = {
                let (queue, bytes, hold, fail) = (
                    Arc::clone(&queue),
                    Arc::clone(&bytes),
                    Arc::clone(&hold),
                    Arc::clone(&fail),
                );
                std::thread::spawn(move || {
                    queue.write(|offset, pieces| {
                        let (held, released) = &*hold;
                        let mut held = held.lock().unwrap();
                        while *held {
                            held = released.wait(held).unwrap();
                        }
                        if fail.load(std::sync::atomic::Ordering::SeqCst) {
                            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
                        }
                        let mut at = offset as usize;
                        for piece in pieces {
                            bytes.lock().unwrap()[at..at + piece.len()].copy_from_slice(piece);
                            at += piece.len();
                        }
                        Ok(())
                    })
                })
            };
            Self {
                queue,
                bytes,
                hold,
                fail,
                writer: Some(writer),
            }
        }

        fn hold(&self, on: bool) {
            *self.hold.0.lock().unwrap() = on;
            self.hold.1.notify_all();
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.hold(false);
            self.queue.close();
            self.writer.take().unwrap().join().unwrap();
        }
    }

    /// A run still queued behind a busy writer is overlaid on a read, piece by
    /// piece, from the middle of the run.
    #[test]
    fn a_queued_run_is_read_back_before_the_writer_reaches_it() {
        let fake = Fake::new(1 << 20);
        fake.hold(true);
        fake.queue
            .send(4096, vec![vec![1; 4096], vec![2; 4096]])
            .unwrap();
        let queued = fake.queue.overlapping(6144, 4096).unwrap();
        assert_eq!(queued.len(), 1);
        let mut out = [0u8; 4096];
        overlay_run(&queued[0], 6144, &mut out);
        assert!(out[..2048].iter().all(|&byte| byte == 1));
        assert!(out[2048..].iter().all(|&byte| byte == 2));
        assert!(fake.queue.overlapping(12288, 4096).unwrap().is_empty());
        assert!(
            fake.bytes.lock().unwrap()[4096..12288]
                .iter()
                .all(|&byte| byte == 0)
        );
        fake.hold(false);
        fake.queue.drain().unwrap();
        assert!(
            fake.bytes.lock().unwrap()[8192..12288]
                .iter()
                .all(|&byte| byte == 2)
        );
        assert!(fake.queue.overlapping(4096, 8192).unwrap().is_empty());
    }

    /// With several writers, a run that overlaps an older unwritten run waits
    /// for it, and one that overlaps nothing older goes out beside it.
    #[test]
    fn writers_take_a_run_only_after_older_runs_it_overlaps() {
        let run = |offset, byte| Arc::new((offset, vec![vec![byte; 4096]]));
        let (old, apart, newer) = (run(0, 1), run(8192, 2), run(0, 3));
        let mut state = Queued::default();
        state.runs.extend([&old, &apart, &newer].map(Arc::clone));
        let next = |state: &Queued| next_run(state).map(|run| run.1[0][0]);
        assert_eq!(next(&state), Some(1));
        state.writing.push(Arc::clone(&old));
        assert_eq!(next(&state), Some(2));
        state.writing.push(Arc::clone(&apart));
        assert_eq!(next(&state), None, "the newer write overtook the older");
        state.runs.pop_front();
        state.writing.remove(0);
        assert_eq!(next(&state), Some(3));
    }

    /// The first failed write fails the next drain and every send and read
    /// after it: the device does not hold the run the queue dropped.
    #[test]
    fn a_failed_queued_write_fails_the_next_flush_and_every_send_and_read_after() {
        let fake = Fake::new(1 << 20);
        fake.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        fake.queue.send(0, vec![vec![1; 4096]]).unwrap();
        assert!(matches!(fake.queue.drain(), Err(Error::Io { .. })));
        assert!(matches!(
            fake.queue.overlapping(0, 4096),
            Err(Error::Io { .. })
        ));
        fake.fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(fake.queue.send(0, vec![vec![2; 4096]]).is_err());
        assert!(fake.queue.drain().is_err());
    }

    /// A panic in the write counts as a failed write. Otherwise the run it
    /// was writing stays queued and every `drain` waits for a thread that is
    /// gone.
    #[test]
    fn a_panicking_write_fails_drain_instead_of_hanging() {
        let queue = Arc::new(Queue::default());
        let writer = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || queue.write(|_, _| panic!("injected")))
        };
        queue.send(0, vec![vec![1; 4096]]).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let drainer = Arc::clone(&queue);
        std::thread::spawn(move || done.send(drainer.drain().is_err()).unwrap());
        assert_eq!(
            finished.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(true)
        );
        assert!(queue.send(0, vec![vec![2; 4096]]).is_err());
        queue.close();
        writer.join().unwrap();
    }

    /// Past `MAX_QUEUED`, a send waits until the writer drains the queue.
    #[test]
    fn a_full_queue_makes_the_sender_wait_for_the_writer() {
        let fake = Fake::new(MAX_QUEUED + MAX_RUN);
        fake.hold(true);
        for index in 0..MAX_QUEUED / MAX_RUN {
            fake.queue
                .send((index * MAX_RUN) as u64, vec![vec![3; MAX_RUN]])
                .unwrap();
        }
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let releaser = {
            let (released, hold) = (Arc::clone(&released), Arc::clone(&fake.hold));
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                released.store(true, std::sync::atomic::Ordering::SeqCst);
                *hold.0.lock().unwrap() = false;
                hold.1.notify_all();
            })
        };
        fake.queue
            .send(MAX_QUEUED as u64, vec![vec![3; MAX_RUN]])
            .unwrap();
        assert!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            "a send exceeded the queue bound"
        );
        fake.queue.drain().unwrap();
        releaser.join().unwrap();
        assert!(fake.bytes.lock().unwrap().iter().all(|&byte| byte == 3));
    }

    /// `pwritev` writes every piece of a run at its offset.
    #[test]
    fn a_run_is_written_piece_by_piece() {
        let path = std::env::temp_dir().join(format!("carryfs-disk-run-{}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let pieces: Vec<Vec<u8>> = (0..2000u32).map(|index| vec![index as u8; 512]).collect();
        write_run(&file, 4096, &pieces).unwrap();
        let mut raw = vec![0u8; 2000 * 512];
        file.read_exact_at(&mut raw, 4096).unwrap();
        assert_eq!(raw, pieces.concat());
        std::fs::remove_file(&path).unwrap();
    }
}
