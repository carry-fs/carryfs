//! Windows partition backend. The partition is opened with no sharing at all,
//! so no other handle can open it while CarryFS owns it. Partitions are
//! hidden (GPT attributes 62 and 63), so Windows assigns them no drive letter
//! and ordinary programs have no path to them.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cache::{Cache, MAX_DIRTY};
use crate::waits::{Shared, Waits, Window};
use crate::writer::Writer;
use carryfs_core::{BlockDevice, Error, Geometry, MountMode, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_IO_PENDING, GetLastError, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY_EX, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
    PropertyStandardQuery, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
    STORAGE_PROPERTY_QUERY, StorageAccessAlignmentProperty,
};
use windows_sys::Win32::System::Threading::CreateEventW;

pub struct Disk {
    /// The partition path, which scopes injected failures to one volume.
    #[cfg(feature = "fault-injection")]
    name: String,
    /// Shared with `Release`, which closes the handle when Windows asks to
    /// remove the device. I/O clones the `Arc` so it never holds the lock.
    file: Arc<Mutex<Option<Arc<File>>>>,
    mode: MountMode,
    geometry: Geometry,
    cache: Cache,
    /// Where the last data-sized device read ended, to detect a sequential stream.
    stream_end: Option<u64>,
    /// Writes held blocks in the background; `None` on a read-only device.
    writer: Option<Writer>,
    /// The next windows of a sequential stream, in order, each being read
    /// on its own thread: at most `IN_FLIGHT`.
    read_ahead: VecDeque<ReadAhead>,
    /// What request threads see of those windows outside the volume lock.
    shared: Arc<Shared>,
    /// The current window of a sequential stream, as read from the device.
    stream: Option<(u64, Vec<u8>)>,
    /// The window before it. Windows' cache manager keeps several requests of
    /// a file in flight and they arrive in any order. Without this, a request
    /// for the last window that came after one for this was read from the
    /// device again and ended the stream.
    previous: Option<(u64, Vec<u8>)>,
    /// How much the stream reads ahead next: `FIRST_WINDOW` when it starts,
    /// doubled each time it reaches the window read ahead, up to `READ_AHEAD`.
    window: usize,
    /// Bytes of data-sized device reads that each began where the last ended.
    run: u64,
    /// Windows read ahead and used: calls and bytes.
    background_reads: (u64, u64),
    /// Device call counts and time, printed on drop when `CARRYFS_TRACE` is set.
    stats: [(u64, u64, Duration); 3],
    /// Set while the volume is being opened and checked, which reads metadata
    /// in runs of adjacent pages: a metadata miss then reads its whole chunk.
    scan: Arc<AtomicBool>,
}

/// Turns the chunked metadata reads of `Disk` on and off from outside the
/// volume that owns the disk.
#[derive(Clone)]
pub struct Scan(Arc<AtomicBool>);

impl Scan {
    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::Relaxed);
    }
}

struct ReadAhead {
    offset: u64,
    len: usize,
    /// Cleared when a write overlaps the window after it was started.
    valid: bool,
    landed: Window,
    thread: JoinHandle<io::Result<Vec<u8>>>,
}

/// Whether a read that ended at `end` used up the stream's current window,
/// `(start, len)`, and the first window in flight, at `first`, follows it.
fn continues_in((start, len): (u64, usize), first: u64, end: u64) -> bool {
    end == start + len as u64 && first == end
}

/// The core reads file data in 64 KiB units; smaller reads are metadata pages.
const STREAM_UNIT: usize = 64 << 10;
/// The most a sequential stream reads ahead in one device call. On a USB SSD
/// a raw read runs at 307 MiB/s in 1 MiB requests and 360 in 4 MiB, with 368
/// at 8 MiB; macOS keeps 4 MiB too.
const READ_AHEAD: usize = 4 << 20;
/// Windows a stream keeps being read at once. On blocks of a USB SSD
/// written in one sequential pass, 4 MiB requests read at 311-347 MiB/s one
/// at a time, 383-386 with two in flight, 431-432 with three or four and
/// 374-382 with eight.
const IN_FLIGHT: usize = 3;
/// How long a stream runs before it keeps a second window in flight, and four
/// times as long before it keeps all of them. From the start, a second window
/// read files of 0.2 to 3 MiB in shuffled order 17% slower and files of 4 to
/// 12 MiB 8% slower: each stream left its windows behind for the next to wait
/// for. A third from 16 MiB cost files of 20 to 48 MiB 3%.
const DEEP: u64 = 16 << 20;
/// What a stream reads ahead when it starts. A window of 4 MiB from the start
/// read 256 files of 0.2 to 3 MiB in shuffled order at 63 MiB/s against 177:
/// each file paid for a window it never used and waited for the next one.
const FIRST_WINDOW: usize = 1 << 20;
/// How long a stream of requests no smaller than `FIRST_WINDOW` runs before
/// it reads ahead at all. WinFsp asks for 1 MiB at a time, which a 1 MiB
/// window never exceeds, so without this such a stream is read one
/// synchronous request after another, 290 MiB/s on a USB SSD.
const ENGAGE: u64 = 4 << 20;
/// The aligned chunk a metadata miss reads while the volume is scanned. The
/// raw reader of the CLI measured the same size best (src/device.rs), and a
/// 1,000,000-file mount on a hard disk agreed: 64 KiB 156 s, 256 KiB 133 s,
/// 1 MiB 181 s, 4 MiB 585 s, none 353 s.
const SCAN_CHUNK: usize = 256 << 10;
/// Written runs this long are file data and leave the block cache.
const LONG_RUN: usize = 256 << 10;

const READ: usize = 0;
const WRITE: usize = 1;
const FLUSH: usize = 2;

impl Disk {
    fn timed<T>(
        &mut self,
        kind: usize,
        bytes: usize,
        call: impl FnOnce(&File) -> io::Result<T>,
    ) -> io::Result<T> {
        let file = self.file.lock().ok().and_then(|file| file.clone());
        let Some(file) = file else {
            return Err(io::Error::other("the device was released for removal"));
        };
        let start = Instant::now();
        let result = call(&file);
        let elapsed = start.elapsed();
        if elapsed > Duration::from_secs(5) {
            eprintln!(
                "carryfs: device {} took {elapsed:?}",
                ["read", "write", "flush"][kind]
            );
        }
        let entry = &mut self.stats[kind];
        entry.0 += 1;
        entry.1 += bytes as u64;
        entry.2 += elapsed;
        result
    }
}

impl Drop for Disk {
    /// Finish queued writes, then report the counters.
    fn drop(&mut self) {
        for ahead in self.read_ahead.drain(..) {
            let _ = ahead.thread.join();
        }
        let background = self.writer.take().map(|writer| {
            let _ = writer.drain();
            writer.stats()
        });
        if std::env::var_os("CARRYFS_TRACE").is_some()
            || std::env::var_os("CARRYFS_STATS").is_some()
        {
            for (name, (calls, bytes, time)) in ["read", "write", "flush"].iter().zip(self.stats) {
                eprintln!("carryfs: device {name}: {calls} calls, {bytes} bytes, {time:?}");
            }
            let (calls, bytes) = self.background_reads;
            eprintln!("carryfs: device read ahead: {calls} windows used, {bytes} bytes");
            if let Some((calls, bytes, time)) = background {
                eprintln!(
                    "carryfs: device background write: {calls} calls, {bytes} bytes, {time:?}"
                );
            }
        }
    }
}

impl Disk {
    /// Open a partition. `lock` takes exclusive write ownership; the unmounted
    /// `probe` only reads and shares.
    pub fn open(path: &Path, mode: MountMode, lock: bool) -> Result<Self> {
        // Owning a partition means denying every other handle through the share
        // mode. FSCTL_LOCK_VOLUME is not used: closing or unlocking a locked
        // handle blocks while Windows waits on a removal query, which then
        // vetoes the eject. A hidden partition has no other opener to lose to.
        let share = if lock {
            0
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        };
        // Overlapped, because Windows runs the calls on a synchronous handle
        // one at a time: windows read ahead, the background writer and the
        // request thread would only queue behind each other. With no sharing
        // there can be no second handle to give them.
        let file = OpenOptions::new()
            .read(true)
            .write(mode == MountMode::ReadWrite)
            .share_mode(share)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(path)
            .map_err(|source| Error::Io {
                operation: "open volume",
                source,
            })?;
        let mut length = GET_LENGTH_INFORMATION { Length: 0 };
        ioctl(
            &file,
            IOCTL_DISK_GET_LENGTH_INFO,
            &[],
            bytes_of_mut(&mut length),
            "read volume length",
        )?;
        // SAFETY: DISK_GEOMETRY_EX is plain old data; all-zero is a valid value.
        let mut disk: DISK_GEOMETRY_EX = unsafe { std::mem::zeroed() };
        ioctl(
            &file,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            &[],
            bytes_of_mut(&mut disk),
            "read sector size",
        )?;
        let logical = disk.Geometry.BytesPerSector;
        // Some USB bridges do not answer the alignment query; assume no 512e.
        let physical = physical_sector_bytes(&file).unwrap_or(logical);
        let geometry = Geometry {
            byte_len: u64::try_from(length.Length).map_err(|_| Error::InvalidGeometry)?,
            logical_sector_bytes: logical,
            physical_sector_bytes: physical,
        };
        let file = Arc::new(Mutex::new(Some(Arc::new(file))));
        let writer = if mode == MountMode::ReadWrite {
            let shared = Arc::clone(&file);
            #[cfg(feature = "fault-injection")]
            let name = path.display().to_string();
            let writer = Writer::spawn(move |offset, data| {
                #[cfg(feature = "fault-injection")]
                injected_failure("CARRYFS_FAIL_WRITES", &name)?;
                write_run(&shared, offset, data)
            })
            .map_err(|source| Error::Io {
                operation: "start the device writer",
                source,
            })?;
            Some(writer)
        } else {
            None
        };
        Ok(Self {
            file,
            #[cfg(feature = "fault-injection")]
            name: path.display().to_string(),
            mode,
            geometry,
            cache: Cache::default(),
            stream_end: None,
            writer,
            read_ahead: VecDeque::new(),
            shared: Arc::default(),
            stream: None,
            previous: None,
            window: FIRST_WINDOW,
            run: 0,
            background_reads: (0, 0),
            stats: [(0, 0, Duration::ZERO); 3],
            scan: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The file system Windows has mounted on the volume, such as `NTFS`.
    /// A blank or CarryFS partition has none; Windows reports it as `RAW`.
    pub fn file_system(&self) -> Option<String> {
        use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
        let handle = self.handle()?;
        let mut name = [0u16; 64];
        // SAFETY: the handle is open, the name buffer is live for the length
        // passed, and the other outputs are optional.
        let answered = unsafe {
            GetVolumeInformationByHandleW(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        if answered == 0 {
            return None;
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Some(String::from_utf16_lossy(&name[..len])).filter(|name| name != "RAW")
    }

    /// The raw volume handle, for device notifications; valid until released or dropped.
    pub fn handle(&self) -> Option<windows_sys::Win32::Foundation::HANDLE> {
        let file = self.file.lock().ok()?;
        file.as_ref().map(|file| file.as_raw_handle())
    }

    pub fn release(&self) -> Release {
        Release(Arc::clone(&self.file))
    }

    /// The switch for chunked metadata reads, off until set.
    pub fn scan(&self) -> Scan {
        Scan(Arc::clone(&self.scan))
    }

    /// What requests wait for before they take the volume lock.
    pub fn waits(&self) -> Waits {
        Waits {
            backlog: self.writer.as_ref().map(Writer::backlog),
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Closes a disk's handle from outside the volume that owns it.
pub struct Release(Arc<Mutex<Option<Arc<File>>>>);

impl Release {
    /// Close the handle; later device I/O fails. I/O in flight is cancelled,
    /// because I/O on a disk with a pending removal query waits for that query.
    /// Returns whether the handle closed within `timeout`.
    pub fn release(&self, timeout: Duration) -> bool {
        let Some(file) = self.0.lock().ok().and_then(|mut file| file.take()) else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while Arc::strong_count(&file) > 1 {
            // SAFETY: the handle stays open while `file` is held; a null
            // OVERLAPPED cancels every pending I/O on it, from any thread.
            unsafe { CancelIoEx(file.as_raw_handle(), std::ptr::null()) };
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }
}

impl Disk {
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

impl BlockDevice for Disk {
    fn mode(&self) -> MountMode {
        self.mode
    }

    fn geometry(&mut self) -> Result<Geometry> {
        Ok(self.geometry)
    }

    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.read(offset, out)?;
        // Smaller reads are metadata pages and say nothing about the stream.
        if out.len() >= STREAM_UNIT {
            let next = self.next_window(offset + out.len() as u64);
            *self.shared.next.lock().unwrap_or_else(|e| e.into_inner()) = next;
        }
        Ok(())
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.mode == MountMode::ReadOnly {
            return Err(Error::ReadOnly);
        }
        self.check_range(offset, data.len())?;
        // Bytes read ahead or kept for the stream before this write may be
        // older than it.
        let end = offset + data.len() as u64;
        for ahead in &mut self.read_ahead {
            if offset < ahead.offset + ahead.len as u64 && ahead.offset < end {
                ahead.valid = false;
            }
        }
        for kept in [&mut self.stream, &mut self.previous] {
            if let Some((start, window)) = kept
                && offset < *start + window.len() as u64
                && *start < end
            {
                *kept = None;
            }
        }
        if self.cache.write(offset, data) {
            // Any order is allowed before the flush, so send a full buffer early.
            if self.cache.dirty_bytes() >= MAX_DIRTY {
                self.write_dirty()?;
            }
            return Ok(());
        }
        // Unaligned writes go straight to the device, after every block held
        // or queued before them, and their blocks leave the cache.
        self.write_dirty()?;
        self.drain()?;
        self.cache.forget(offset, data.len());
        self.write_device(offset, data)
    }

    fn flush_durable(&mut self) -> Result<()> {
        // The barrier covers every earlier write, queued ones included.
        self.write_dirty()?;
        self.drain()?;
        // FlushFileBuffers on a volume handle sends a flush-cache command to the disk.
        #[cfg(feature = "fault-injection")]
        let name = self.name.clone();
        self.timed(FLUSH, 0, |file| {
            #[cfg(feature = "fault-injection")]
            injected_failure("CARRYFS_FAIL_FLUSHES", &name)?;
            file.sync_all()
        })
        .map_err(|source| Error::Io {
            operation: "flush_durable",
            source,
        })
    }
}

impl Disk {
    fn read(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.check_range(offset, out.len())?;
        if self.cache.read(offset, out) {
            return Ok(());
        }
        self.collect_written();
        // File data is served from the stream window rather than the block
        // cache, where it would evict the metadata every operation reads.
        if self.read_stream(offset, out) {
            // A read below a data unit is cached wherever it came from, as
            // one read from the device is below. Small files read after a
            // large one ride its stream, and left out of the cache they made
            // the removals that followed 8% slower.
            if out.len() < STREAM_UNIT {
                self.cache.merge(offset, out);
            }
            return Ok(());
        }
        // A stream that reaches the window read ahead reads the next one
        // ahead. The read may start anywhere in that window: of two requests
        // a caller keeps in flight, the later one can get here first while
        // the earlier waits for the window outside the volume lock, and
        // counting only a read at the window's start then ended the stream
        // (robocopy out of a tree 10% slower).
        if let Some((start, end)) = self.finish_read_ahead()
            && self.read_stream(offset, out)
        {
            self.stream_end = Some(end);
            self.run = self.run.saturating_add(end - start);
            self.window = grown(self.window);
            self.start_read_ahead(end);
            return Ok(());
        }
        let window = self.read_window(offset, out.len());
        if window > out.len() {
            let mut buffer = vec![0; window];
            self.read_device(offset, &mut buffer)?;
            out.copy_from_slice(&buffer[..out.len()]);
            self.cache.overlay(offset, out);
            // Kept only if the device already had every block's latest bytes;
            // a pending block could leave the cache while the window lives.
            self.stream = (!self.cache.pending(offset, window)).then_some((offset, buffer));
            self.start_read_ahead(offset + window as u64);
        } else if let Some((start, len)) = self
            .scan
            .load(Ordering::Relaxed)
            .then(|| scan_chunk(offset, out.len(), self.geometry.byte_len, SCAN_CHUNK))
            .flatten()
        {
            let mut buffer = vec![0; len];
            self.read_device(start, &mut buffer)?;
            // Held and queued blocks stay authoritative over what was read.
            self.cache.merge(start, &mut buffer);
            let from = (offset - start) as usize;
            out.copy_from_slice(&buffer[from..from + out.len()]);
        } else {
            // A long enough stream of whole-window requests has the next
            // window read while this request is.
            if out.len() >= STREAM_UNIT && self.run >= ENGAGE {
                self.start_read_ahead(offset + out.len() as u64);
            }
            self.read_device(offset, out)?;
            if out.len() < STREAM_UNIT {
                self.cache.merge(offset, out);
            } else {
                self.cache.overlay(offset, out);
            }
        }
        Ok(())
    }

    /// The window a read continuing from `end` would wait for: the first one
    /// in flight, when the read just served ended where the stream's current
    /// window does.
    fn next_window(&self, end: u64) -> Option<Window> {
        let (start, window) = self.stream.as_ref()?;
        let first = self.read_ahead.front()?;
        continues_in((*start, window.len()), first.offset, end).then(|| first.landed.clone())
    }

    /// How many bytes to read for a miss of `len` at `offset`. A data unit that
    /// starts where the last one ended continues a sequential stream and is
    /// read `FIRST_WINDOW` at a time, because each synchronous device call
    /// costs far more than its bytes. Metadata pages neither start nor break
    /// a stream.
    fn read_window(&mut self, offset: u64, len: usize) -> usize {
        if len < STREAM_UNIT {
            return len;
        }
        let mut window = len;
        if self.stream_end == Some(offset) {
            self.run = self.run.saturating_add(len as u64);
            let room = self.geometry.byte_len.saturating_sub(offset);
            let ahead = usize::try_from(room).map_or(FIRST_WINDOW, |room| room.min(FIRST_WINDOW));
            // Whole cache blocks, so `merge` can keep held writes authoritative.
            window = window.max(ahead - ahead % crate::cache::BLOCK);
        } else {
            self.run = len as u64;
            self.window = FIRST_WINDOW;
        }
        self.stream_end = offset.checked_add(window as u64);
        window
    }

    /// Queue held blocks for the background writer; they stay cached until
    /// written. A failure, now or reported for an earlier run, faults the
    /// core's publication, which requires reopening the volume.
    fn write_dirty(&mut self) -> Result<()> {
        let runs = self.cache.take_dirty();
        let Some(writer) = &self.writer else {
            // Only a writable device holds blocks.
            return if runs.is_empty() {
                Ok(())
            } else {
                Err(Error::ReadOnly)
            };
        };
        for (offset, run) in runs {
            let len = run.len();
            self.cache.pin(offset, len);
            if let Err(source) = writer.send(offset, run) {
                self.cache.unpin(offset, len);
                return Err(Error::Io {
                    operation: "write_all_at",
                    source,
                });
            }
        }
        self.collect_written();
        Ok(())
    }

    /// Wait until every queued run is on the device.
    fn drain(&mut self) -> Result<()> {
        let result = self.writer.as_ref().map_or(Ok(()), Writer::drain);
        self.collect_written();
        result.map_err(|source| Error::Io {
            operation: "write_all_at",
            source,
        })
    }

    /// Have the stream's windows from `offset` on read on other threads, up
    /// to `IN_FLIGHT` at once. A window is not started while any block in it
    /// is held or queued: the device may not have those bytes yet, and the
    /// blocks could leave the cache before the window lands. Windows still in
    /// flight for another stream are left for `finish_read_ahead` to take.
    fn start_read_ahead(&mut self, offset: u64) {
        let mut next = match (self.read_ahead.front(), self.read_ahead.back()) {
            (Some(first), Some(last)) if first.offset == offset => last.offset + last.len as u64,
            (None, _) => offset,
            _ => return,
        };
        let in_flight = in_flight(self.run);
        while self.read_ahead.len() < in_flight {
            let offset = next;
            let room = self.geometry.byte_len.saturating_sub(offset);
            let len = usize::try_from(room).map_or(self.window, |room| room.min(self.window));
            let len = len - len % crate::cache::BLOCK;
            if len == 0 || self.cache.pending(offset, len) {
                return;
            }
            let Some(file) = self.file.lock().ok().and_then(|file| file.clone()) else {
                return;
            };
            let landed = Window(Arc::new(AtomicBool::new(false)));
            let (shared, arrived) = (Arc::clone(&self.shared), landed.clone());
            let thread = std::thread::Builder::new()
                .name("carryfs-read-ahead".into())
                .spawn(move || {
                    let read = || {
                        let mut buffer = vec![0; len];
                        let mut done = 0;
                        while done < len {
                            match read_at(&file, &mut buffer[done..], offset + done as u64)? {
                                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                                read => done += read,
                            }
                        }
                        Ok(buffer)
                    };
                    let result = read();
                    let _held = shared.arriving.lock().unwrap_or_else(|e| e.into_inner());
                    arrived.0.store(true, Ordering::Release);
                    shared.arrived.notify_all();
                    result
                });
            let Ok(thread) = thread else {
                return;
            };
            self.read_ahead.push_back(ReadAhead {
                offset,
                len,
                valid: true,
                landed,
                thread,
            });
            next = offset + len as u64;
        }
    }

    /// Wait for the first window in flight and cache it, unless a write
    /// overlapped it. Returns its range. A failed read is dropped: the
    /// synchronous read that follows reports the error.
    fn finish_read_ahead(&mut self) -> Option<(u64, u64)> {
        let ahead = self.read_ahead.pop_front()?;
        let data = ahead.thread.join().ok()?.ok()?;
        if !ahead.valid {
            return None;
        }
        self.background_reads.0 += 1;
        self.background_reads.1 += ahead.len as u64;
        self.previous = self.stream.replace((ahead.offset, data));
        Some((ahead.offset, ahead.offset + ahead.len as u64))
    }

    /// Serve `out` from the stream's current or previous window, with held
    /// and queued blocks laid over.
    fn read_stream(&self, offset: u64, out: &mut [u8]) -> bool {
        for (start, window) in [&self.stream, &self.previous].into_iter().flatten() {
            let Some(at) = offset
                .checked_sub(*start)
                .and_then(|at| usize::try_from(at).ok())
            else {
                continue;
            };
            if let Some(bytes) = window.get(at..at + out.len()) {
                out.copy_from_slice(bytes);
                self.cache.overlay(offset, out);
                return true;
            }
        }
        false
    }

    /// Release the cache blocks of runs the writer has finished. A long run is
    /// file data (the macOS adapter's rule) and leaves the cache.
    fn collect_written(&mut self) {
        if let Some(writer) = &self.writer {
            for (offset, len) in writer.take_done() {
                self.cache.unpin(offset, len);
                if len >= LONG_RUN {
                    self.cache.drop_clean(offset, len);
                }
            }
        }
    }

    fn read_device(&mut self, mut offset: u64, mut out: &mut [u8]) -> Result<()> {
        while !out.is_empty() {
            let length = out.len();
            let read = self
                .timed(READ, length, |file| read_at(file, out, offset))
                .map_err(|source| Error::Io {
                    operation: "read_exact_at",
                    source,
                })?;
            if read == 0 {
                return Err(Error::Io {
                    operation: "read_exact_at",
                    source: io::ErrorKind::UnexpectedEof.into(),
                });
            }
            out = &mut out[read..];
            offset += read as u64;
        }
        Ok(())
    }

    fn write_device(&mut self, mut offset: u64, mut data: &[u8]) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        let name = self.name.clone();
        while !data.is_empty() {
            let written = self
                .timed(WRITE, data.len(), |file| {
                    #[cfg(feature = "fault-injection")]
                    injected_failure("CARRYFS_FAIL_WRITES", &name)?;
                    write_at(file, data, offset)
                })
                .map_err(|source| Error::Io {
                    operation: "write_all_at",
                    source,
                })?;
            if written == 0 {
                return Err(Error::Io {
                    operation: "write_all_at",
                    source: io::ErrorKind::WriteZero.into(),
                });
            }
            data = &data[written..];
            offset += written as u64;
        }
        Ok(())
    }
}

/// How many windows a stream that has run `run` bytes keeps in flight.
fn in_flight(run: u64) -> usize {
    match run {
        run if run >= 4 * DEEP => IN_FLIGHT,
        run if run >= DEEP => 2,
        _ => 1,
    }
}

/// The window after one of `window` bytes that the stream went on to use.
fn grown(window: usize) -> usize {
    window.saturating_mul(2).min(READ_AHEAD)
}

/// The aligned chunk to read for a metadata request, as its start and length,
/// or `None` when the request is not whole blocks inside one whole-block chunk
/// of the device, and is read as asked.
fn scan_chunk(offset: u64, len: usize, byte_len: u64, chunk: usize) -> Option<(u64, usize)> {
    let block = crate::cache::BLOCK as u64;
    if len == 0
        || len >= STREAM_UNIT
        || !offset.is_multiple_of(block)
        || !(len as u64).is_multiple_of(block)
    {
        return None;
    }
    let start = offset - offset % chunk as u64;
    let end = (start + chunk as u64).min(byte_len);
    (end.is_multiple_of(block) && offset + len as u64 <= end)
        .then_some((start, (end - start) as usize))
}

/// Write one run for the background writer, through the handle `Release` may close.
fn write_run(file: &Mutex<Option<Arc<File>>>, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
    let file = file
        .lock()
        .ok()
        .and_then(|file| file.clone())
        .ok_or_else(|| io::Error::other("the device was released for removal"))?;
    while !data.is_empty() {
        let written = write_at(&file, data, offset)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        data = &data[written..];
        offset += written as u64;
    }
    Ok(())
}

/// Fail with ERROR_IO_DEVICE for `device` while the file that `variable`
/// names exists, as a device that reports errors would. The file's text, when
/// not empty, is the partition path to fail; the service mounts other volumes
/// that a test must not touch. A VHD cannot stand in for a failing device: it
/// acknowledges writes and flushes while its backing volume has room.
#[cfg(feature = "fault-injection")]
fn injected_failure(variable: &str, device: &str) -> io::Result<()> {
    const ERROR_IO_DEVICE: i32 = 1117;
    let Some(path) = std::env::var_os(variable) else {
        return Ok(());
    };
    match std::fs::read_to_string(path) {
        Ok(target) if target.trim().is_empty() || target.trim() == device => {
            Err(io::Error::from_raw_os_error(ERROR_IO_DEVICE))
        }
        _ => Ok(()),
    }
}

/// Partition paths (`\\.\HarddiskNPartitionM`) of the disk at `disk`, such as
/// a disk interface path or `\\.\PhysicalDriveN`. Hidden partitions have no
/// volume name, so this is how CarryFS partitions are found.
pub fn partitions(disk: &Path) -> Vec<String> {
    let number = OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(disk)
        .ok()
        .and_then(|file| {
            // SAFETY: STORAGE_DEVICE_NUMBER is plain old data; all-zero is valid.
            let mut number: STORAGE_DEVICE_NUMBER = unsafe { std::mem::zeroed() };
            ioctl(
                &file,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                &[],
                bytes_of_mut(&mut number),
                "read disk number",
            )
            .ok()
            .map(|()| number.DeviceNumber)
        });
    let Some(number) = number else {
        return Vec::new();
    };
    // Stop at the first partition that cannot be opened. This assumes contiguous
    // numbering from 1 and misses later partitions after a gap or an open failure.
    (1..)
        .map(|partition| format!("\\\\.\\Harddisk{number}Partition{partition}"))
        .take_while(|path| {
            OpenOptions::new()
                .access_mode(0)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(path)
                .is_ok()
        })
        .collect()
}

/// Whether the partition at `path` can still be opened: `None` if the answer
/// is unclear, such as a sharing violation.
pub fn partition_present(path: &str) -> Option<bool> {
    match OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
    {
        Ok(_) => Some(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn physical_sector_bytes(file: &File) -> Result<u32> {
    // STORAGE_PROPERTY_QUERY has trailing padding, so build its bytes explicitly.
    let mut query = [0_u8; size_of::<STORAGE_PROPERTY_QUERY>()];
    query[..4].copy_from_slice(&StorageAccessAlignmentProperty.to_le_bytes());
    query[4..8].copy_from_slice(&PropertyStandardQuery.to_le_bytes());
    // SAFETY: the descriptor is plain old data; all-zero is a valid value.
    let mut alignment: STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR = unsafe { std::mem::zeroed() };
    ioctl(
        file,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &query,
        bytes_of_mut(&mut alignment),
        "query sector alignment",
    )?;
    Ok(alignment.BytesPerPhysicalSector)
}

/// A manual-reset event for one thread's overlapped calls. An OVERLAPPED
/// without one is signalled through the file handle, which calls running at
/// once would share.
struct Event(HANDLE);

impl Event {
    fn new() -> Option<Self> {
        // SAFETY: no security attributes or name; manual reset, not signalled.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        (!event.is_null()).then_some(Self(event))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the event was created by `new` and is closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

thread_local! {
    static EVENT: Option<Event> = Event::new();
}

/// Start one overlapped call at `offset` with `start` and wait for it.
/// Returns the bytes transferred, zero at the end of the device.
///
/// `start` must issue exactly one call on the handle with the OVERLAPPED it is
/// given, returning that call's BOOL, and whatever buffers it passes must
/// stay valid until this function returns.
fn overlapped(
    file: &File,
    offset: u64,
    start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
) -> io::Result<usize> {
    EVENT.with(|event| {
        let event = event
            .as_ref()
            .ok_or_else(|| io::Error::other("no event for device I/O"))?;
        // SAFETY: OVERLAPPED is plain old data; all-zero is a valid value.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        overlapped.hEvent = event.0;
        let handle = file.as_raw_handle();
        if start(handle, &mut overlapped) == 0 {
            // SAFETY: reads the calling thread's last error.
            match unsafe { GetLastError() } {
                ERROR_IO_PENDING => {}
                ERROR_HANDLE_EOF => return Ok(0),
                code => return Err(io::Error::from_raw_os_error(code as i32)),
            }
        }
        let mut transferred = 0;
        // SAFETY: the handle is open while `file` is borrowed, and the
        // OVERLAPPED and its event live until this wait returns, which is
        // when the call has completed or been cancelled.
        if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) } == 0 {
            // SAFETY: reads the calling thread's last error.
            return match unsafe { GetLastError() } {
                ERROR_HANDLE_EOF => Ok(0),
                code => Err(io::Error::from_raw_os_error(code as i32)),
            };
        }
        Ok(transferred as usize)
    })
}

fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    let length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    overlapped(file, offset, |handle, overlapped| {
        // SAFETY: the buffer is writable for `length` bytes and outlives the
        // call, which `overlapped` waits for.
        unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                length,
                std::ptr::null_mut(),
                overlapped,
            )
        }
    })
}

fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<usize> {
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    overlapped(file, offset, |handle, overlapped| {
        // SAFETY: the data is readable for `length` bytes and outlives the
        // call, which `overlapped` waits for.
        unsafe {
            WriteFile(
                handle,
                data.as_ptr(),
                length,
                std::ptr::null_mut(),
                overlapped,
            )
        }
    })
}

fn ioctl(
    file: &File,
    code: u32,
    input: &[u8],
    output: &mut [u8],
    operation: &'static str,
) -> Result<()> {
    let length = |bytes: usize| {
        u32::try_from(bytes).map_err(|_| Error::InvalidInput("ioctl buffer is too large"))
    };
    let (input_length, output_length) = (length(input.len())?, length(output.len())?);
    // A handle opened without FILE_FLAG_OVERLAPPED ignores the OVERLAPPED.
    overlapped(file, 0, |handle, overlapped| {
        // SAFETY: the handle is open for the call, and both buffers are valid
        // for their stated lengths until `overlapped` has waited for it.
        unsafe {
            DeviceIoControl(
                handle,
                code,
                input.as_ptr().cast::<c_void>(),
                input_length,
                output.as_mut_ptr().cast::<c_void>(),
                output_length,
                std::ptr::null_mut(),
                overlapped,
            )
        }
    })
    .map(|_| ())
    .map_err(|source| Error::Io { operation, source })
}

fn bytes_of_mut<T>(value: &mut T) -> &mut [u8] {
    // SAFETY: only used for Win32 plain-old-data output structs, for which
    // any byte pattern the kernel writes is a valid value.
    unsafe { std::slice::from_raw_parts_mut((value as *mut T).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: usize = 256 << 10;
    const DEVICE: u64 = 1 << 30;

    #[test]
    fn a_stream_window_doubles_up_to_the_largest() {
        let mut window = FIRST_WINDOW;
        let mut sizes = Vec::new();
        for _ in 0..4 {
            window = grown(window);
            sizes.push(window >> 20);
        }
        assert_eq!(sizes, [2, 4, 4, 4]);
    }

    #[test]
    fn a_read_continues_in_the_window_that_follows_the_one_it_used_up() {
        const MIB: u64 = 1 << 20;
        // The stream's window is [8, 12) MiB and [12, 16) is being read.
        assert!(continues_in((8 * MIB, 4 << 20), 12 * MIB, 12 * MIB));
        // A read that stops inside the window has more of it to use.
        assert!(!continues_in((8 * MIB, 4 << 20), 12 * MIB, 11 * MIB));
        // A window in flight for another stream is not this read's.
        assert!(!continues_in((8 * MIB, 4 << 20), 64 * MIB, 12 * MIB));
    }

    #[test]
    fn a_stream_adds_windows_in_flight_as_it_runs() {
        let mib = |count: u64| in_flight(count << 20);
        assert_eq!([mib(0), mib(15), mib(16), mib(63)], [1, 1, 2, 2]);
        assert_eq!([mib(64), mib(4096)], [IN_FLIGHT, IN_FLIGHT]);
    }

    #[test]
    fn a_metadata_page_reads_its_whole_aligned_chunk() {
        assert_eq!(
            scan_chunk(0x41000, 4096, DEVICE, CHUNK),
            Some((0x40000, CHUNK))
        );
        assert_eq!(scan_chunk(0, 4096, DEVICE, CHUNK), Some((0, CHUNK)));
        // The last page of a chunk, and a two-page request inside one.
        assert_eq!(scan_chunk(0x3f000, 4096, DEVICE, CHUNK), Some((0, CHUNK)));
        assert_eq!(scan_chunk(0x3e000, 8192, DEVICE, CHUNK), Some((0, CHUNK)));
    }

    #[test]
    fn the_last_chunk_stops_at_the_end_of_the_device() {
        let end = 0x40000 + 0x10000;
        assert_eq!(
            scan_chunk(0x4f000, 4096, end, CHUNK),
            Some((0x40000, 0x10000))
        );
        // A device that is not whole blocks long is read as asked.
        assert_eq!(scan_chunk(0x40000, 4096, end + 512, CHUNK), None);
    }

    #[test]
    fn other_requests_are_read_as_asked() {
        // File data units, empty, unaligned, and partial-block requests.
        assert_eq!(scan_chunk(0x40000, STREAM_UNIT, DEVICE, CHUNK), None);
        assert_eq!(scan_chunk(0x40000, 0, DEVICE, CHUNK), None);
        assert_eq!(scan_chunk(0x40200, 4096, DEVICE, CHUNK), None);
        assert_eq!(scan_chunk(0x40000, 512, DEVICE, CHUNK), None);
        // A request that crosses a chunk boundary.
        assert_eq!(scan_chunk(0x3f000, 8192, DEVICE, CHUNK), None);
    }
}
