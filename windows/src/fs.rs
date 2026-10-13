//! WinFsp operations over one serialized `Volume`.
//!
//! This module has no FFI, so its behavior is tested on any host. `ffi.rs`
//! translates WinFsp callbacks into these methods. Writable volumes defer
//! checkpoints: changes are published by `FlushFileBuffers` on a file or the
//! volume, by writes through a `FILE_FLAG_WRITE_THROUGH` handle, by `tick`
//! after a pause in operations or once pending work reaches `Pacing::max_age`,
//! and at eject. A crash loses at most the unpublished changes and recovers
//! the last published state. Cleanup of deleted and replaced objects also runs
//! from `tick`. WinFsp caches directory information for one second, or an hour
//! on a read-only mount (`ffi.rs`). Windows caches the data of files opened
//! without write access and keeps such a file open after its last handle
//! closed, so a context gives up its core handle at Cleanup and the pages
//! Windows still reads or writes through it borrow one (`State::lend`). An
//! open with write access bypasses that cache. What Windows knows of an open
//! file or directory never expires, so the adapter names what a request made
//! stale there (`CarryFs::take_stale`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

use carryfs_core::{
    Access, Attributes, BlockDevice, DirectoryEntry, Error, Handle, Name, ObjectId, ObjectKind,
    ROOT, RemoveKind, Replace, Timestamp, Volume,
};

use crate::waits::{Waits, Window};

pub type NtStatus = i32;

const fn nt(code: u32) -> NtStatus {
    code as i32
}

pub const STATUS_SUCCESS: NtStatus = 0;
pub const STATUS_REPARSE: NtStatus = 0x0000_0104;
pub const STATUS_BUFFER_TOO_SMALL: NtStatus = nt(0xC000_0023);
pub const STATUS_NOT_A_REPARSE_POINT: NtStatus = nt(0xC000_0275);
pub const STATUS_IO_REPARSE_TAG_MISMATCH: NtStatus = nt(0xC000_0277);
pub const STATUS_IO_REPARSE_DATA_INVALID: NtStatus = nt(0xC000_0278);
pub const STATUS_BUFFER_OVERFLOW: NtStatus = nt(0x8000_0005);
pub const STATUS_INVALID_HANDLE: NtStatus = nt(0xC000_0008);
pub const STATUS_INVALID_PARAMETER: NtStatus = nt(0xC000_000D);
pub const STATUS_NO_SUCH_DEVICE: NtStatus = nt(0xC000_000E);
pub const STATUS_INVALID_DEVICE_REQUEST: NtStatus = nt(0xC000_0010);
pub const STATUS_END_OF_FILE: NtStatus = nt(0xC000_0011);
pub const STATUS_ACCESS_DENIED: NtStatus = nt(0xC000_0022);
pub const STATUS_OBJECT_NAME_INVALID: NtStatus = nt(0xC000_0033);
pub const STATUS_OBJECT_NAME_NOT_FOUND: NtStatus = nt(0xC000_0034);
pub const STATUS_OBJECT_NAME_COLLISION: NtStatus = nt(0xC000_0035);
pub const STATUS_OBJECT_PATH_NOT_FOUND: NtStatus = nt(0xC000_003A);
pub const STATUS_DISK_FULL: NtStatus = nt(0xC000_007F);
pub const STATUS_MEDIA_WRITE_PROTECTED: NtStatus = nt(0xC000_00A2);
pub const STATUS_FILE_IS_A_DIRECTORY: NtStatus = nt(0xC000_00BA);
pub const STATUS_UNEXPECTED_IO_ERROR: NtStatus = nt(0xC000_00E9);
pub const STATUS_DIRECTORY_NOT_EMPTY: NtStatus = nt(0xC000_0101);
pub const STATUS_FILE_CORRUPT_ERROR: NtStatus = nt(0xC000_0102);
pub const STATUS_NOT_A_DIRECTORY: NtStatus = nt(0xC000_0103);
pub const STATUS_TOO_MANY_OPENED_FILES: NtStatus = nt(0xC000_011F);
pub const STATUS_DEVICE_BUSY: NtStatus = nt(0x8000_0011);
pub const STATUS_FILE_TOO_LARGE: NtStatus = nt(0xC000_0904);

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// `SymbolicLinkReparseBuffer.Flags`: the target is relative to the link.
const SYMLINK_FLAG_RELATIVE: u32 = 1;
/// Bytes before `PathBuffer` in a symlink `REPARSE_DATA_BUFFER`.
const SYMLINK_HEADER: usize = 20;
/// `FspCleanupDelete`: the last cleanup of a delete-pending file.
pub const CLEANUP_DELETE: u32 = 0x01;

type Reply<T> = std::result::Result<T, NtStatus>;

/// `FSP_FSCTL_FILE_INFO`, 72 bytes; `ffi.rs` checks the layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileInfo {
    pub file_attributes: u32,
    pub reparse_tag: u32,
    pub allocation_size: u64,
    pub file_size: u64,
    pub creation_time: u64,
    pub last_access_time: u64,
    pub last_write_time: u64,
    pub change_time: u64,
    pub index_number: u64,
    pub hard_links: u32,
    pub ea_size: u32,
}

pub struct CarryFs<D: BlockDevice> {
    state: Mutex<State<D>>,
    read_only: bool,
    /// What a read or a write waits for before it takes the volume lock.
    waits: Option<Waits>,
}

struct State<D: BlockDevice> {
    volume: Volume<D>,
    open: HashMap<u64, Open>,
    /// The object of each context that had its Cleanup and awaits its Close.
    /// Windows sends the Close only when it drops the file's cached data and
    /// mapped views, so these outnumber the volume's 4,096 handles and hold
    /// none.
    cleaned: HashMap<u64, ObjectId>,
    next: u64,
    /// The generation at which a maintenance step last published nothing.
    /// Until something publishes again, or an orphan loses its last pin, a
    /// further step would only cost a flush.
    quiet: Option<u64>,
    /// When the last WinFsp request entered.
    last_operation: Instant,
    /// When `tick` first found work that nothing has published since.
    waiting_since: Option<Instant>,
    /// The device handle was released for removal; `tick` issues no more I/O.
    released: bool,
    known: Known,
    /// Counts the requests that changed a name or an attribute.
    changes: u64,
    /// Files whose pages Windows should write out and drop, and open
    /// directories whose entries changed, each once: paths whose state in
    /// Windows went stale. See `CarryFs::take_stale`.
    stale: Vec<Vec<u16>>,
    touched: Vec<Vec<u16>>,
    /// The contexts of files read through a handle that has since closed,
    /// oldest first, each with its path: the files Windows may still keep
    /// data of. Beyond `KEPT_FILES` the oldest becomes stale.
    kept: VecDeque<(u64, Vec<u16>)>,
    /// When a handle with write access last closed. Windows was then asked to
    /// write out the file's mapped pages, and does so a moment later.
    settling: Option<Instant>,
}

/// Files Windows may keep the data of after their handles closed. Each keeps
/// a WinFsp file node and a context here until Windows drops it, which with
/// memory to spare is never, and every volume flush visits each node: with
/// 10,000 kept, a flush took 25 ms longer and 10,000 removals with a flush
/// every 128 took 35% longer.
const KEPT_FILES: usize = 1024;

/// Decoded attributes kept between requests.
const KNOWN_ATTRIBUTES: usize = 16_384;

/// What the adapter remembers between requests, so that a `stat`, which
/// WinFsp turns into GetSecurityByName, Open, and Close, walks the tree once
/// and not ten times. Nothing else can change the
/// volume while this mount owns the device, and every request that changes
/// an attribute or removes a directory forgets what it could have made stale.
#[derive(Default)]
struct Known {
    attributes: HashMap<u64, Attributes>,
    /// The directory the last path was found in: its path as requested, its
    /// object, and its stored spelling.
    directory: Option<(Vec<u16>, ObjectId, Vec<u16>)>,
}

impl Known {
    /// An object's attributes, read from the core once until forgotten. A
    /// faulted volume fails every read, so it is not served from here either.
    fn attributes<D: BlockDevice>(
        &mut self,
        volume: &mut Volume<D>,
        object: ObjectId,
    ) -> Reply<Attributes> {
        if volume.is_faulted() {
            return Err(status(&Error::Faulted));
        }
        if let Some(attributes) = self.attributes.get(&object.get()) {
            return Ok(*attributes);
        }
        let attributes = volume.attributes(object).map_err(|e| status(&e))?;
        if self.attributes.len() >= KNOWN_ATTRIBUTES {
            self.attributes.clear();
        }
        self.attributes.insert(object.get(), attributes);
        Ok(attributes)
    }

    /// Before and after a request that changes sizes or times.
    fn forget_attributes(&mut self) {
        if !self.attributes.is_empty() {
            self.attributes.clear();
        }
    }

    /// Before and after a request that can rename or retype a directory,
    /// which changes where paths through it lead.
    fn forget(&mut self) {
        self.forget_attributes();
        self.directory = None;
    }
}

struct Open {
    object: ObjectId,
    kind: ObjectKind,
    handle: Handle,
    /// For a file, the path WinFsp knows it by.
    path: Option<Vec<u16>>,
    /// The core handle is open for writing.
    writable: bool,
    /// A read was served through this context, so Windows may keep the
    /// file's data, and the context with it, after the handle closes.
    read: bool,
    /// Opened with `FILE_FLAG_WRITE_THROUGH`: writes and truncations publish
    /// before they return.
    write_through: bool,
    listing: Option<Listing>,
    /// The offset at which this handle's last read stopped, when it used up
    /// a stream window and the one after it was still being read.
    next_window: Option<(u64, Window)>,
}

/// Where a directory handle's last ReadDirectory stopped. WinFsp takes about
/// 35 entries a call from a 4 KiB caller and a core page holds 63, so without
/// this every call seeks by name again and decodes entries it never returns.
struct Listing {
    /// `State::changes` when the page was read: any change since drops it.
    changes: u64,
    /// The last name WinFsp received, which it sends back to continue.
    marker: Vec<u16>,
    /// The entries after `marker` already read from the core.
    rest: VecDeque<DirectoryEntry>,
    /// Whether the directory has entries after `rest`.
    more: bool,
}

/// When `tick` publishes.
///
/// A checkpoint holds the volume for its writes and flushes, so publishing in
/// the middle of a burst stalls it. The timer waits for a pause of `idle`, and
/// `max_age` makes work due even without a pause. Lock acquisition and I/O
/// can delay the publication itself.
#[derive(Clone, Copy, Debug)]
pub struct Pacing {
    pub idle: Duration,
    pub max_age: Duration,
}

/// The pacing the Linux and macOS adapters use.
pub const PACING: Pacing = Pacing {
    idle: Duration::from_millis(100),
    max_age: Duration::from_secs(1),
};

impl Pacing {
    /// Whether work outstanding since `waiting_since` should be done now.
    fn due(&self, now: Instant, last_operation: Instant, waiting_since: Instant) -> bool {
        now.duration_since(waiting_since) >= self.max_age
            || now.duration_since(last_operation) >= self.idle
    }
}

/// How a query-remove was answered.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// The volume was idle and published; `release` ran and returned this.
    Released(bool),
    /// The volume is faulted and can publish nothing more, so waiting would
    /// hold the disk for nothing. `release` ran and returned this; changes
    /// since the last successful publication are not on the disk.
    Lost(bool),
    /// A request or checkpoint is in progress; veto and let the user retry.
    Busy,
    /// Changes await publication, which cannot be done while the query blocks
    /// device I/O; veto, publish, and let the user retry.
    Unsynced,
}

impl<D: BlockDevice> CarryFs<D> {
    /// Serve `volume`. A writable volume switches to deferred checkpoints.
    pub fn new(mut volume: Volume<D>, read_only: bool) -> Result<Self, Error> {
        if !read_only {
            volume.set_deferred_checkpoints(true)?;
        }
        Ok(Self {
            state: Mutex::new(State {
                volume,
                open: HashMap::new(),
                cleaned: HashMap::new(),
                next: 1,
                quiet: None,
                last_operation: Instant::now(),
                waiting_since: None,
                released: false,
                known: Known::default(),
                changes: 0,
                stale: Vec::new(),
                touched: Vec::new(),
                kept: VecDeque::new(),
                settling: None,
            }),
            read_only,
            waits: None,
        })
    }

    /// Have reads and writes wait for the device before they take the volume
    /// lock. Otherwise they wait inside the core with the lock held and every
    /// other request behind them.
    pub fn with_waits(mut self, waits: Waits) -> Self {
        self.waits = Some(waits);
        self
    }

    fn with<T>(&self, operation: impl FnOnce(&mut State<D>) -> Reply<T>) -> Reply<T> {
        let mut state = self.state.lock().map_err(|_| STATUS_UNEXPECTED_IO_ERROR)?;
        state.last_operation = Instant::now();
        operation(&mut state)
    }

    /// `with` for a request that changes attributes: nothing remembered
    /// about them outlives it.
    fn change<T>(&self, operation: impl FnOnce(&mut State<D>) -> Reply<T>) -> Reply<T> {
        self.with(|s| {
            s.changes = s.changes.wrapping_add(1);
            s.known.forget_attributes();
            let result = operation(s);
            s.known.forget_attributes();
            result
        })
    }

    /// `change` for a request that can also rename or retype a directory.
    fn change_names<T>(&self, operation: impl FnOnce(&mut State<D>) -> Reply<T>) -> Reply<T> {
        self.with(|s| {
            s.changes = s.changes.wrapping_add(1);
            s.known.forget();
            let result = operation(s);
            s.known.forget();
            result
        })
    }

    /// Publish pending work or run cleanup when `pacing` says it is due,
    /// unless the last step published nothing and nothing has changed since.
    /// The mount's timer calls this every 100 ms. A checkpoint can make
    /// retirement eligible, so only a step that had nothing to publish and
    /// changed nothing marks the volume quiet.
    pub fn tick(&self, now: Instant, pacing: Pacing) {
        if self.read_only {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.released {
            return;
        }
        match state.tick(now, pacing) {
            Ok(()) | Err(Error::Faulted | Error::ReadOnly | Error::Ejected) => {}
            Err(error) => eprintln!("carryfs: checkpoint failed: {error}"),
        }
    }

    /// Answer a query-remove without device I/O, which the pending query
    /// blocks. An idle, published volume runs `release`, which closes the
    /// device handle, while requests are held off; later requests fail. A
    /// faulted volume is released too, since it can publish nothing more.
    pub fn release_for_removal(&self, release: impl FnOnce() -> bool) -> Removal {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Removal::Busy,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        // A fault comes from a failed publication or detected corruption. Either
        // way nothing more can be published, so a veto would hold the disk for
        // nothing; whatever was pending is lost.
        let faulted = !self.read_only && state.volume.is_faulted();
        // Pages Windows was asked to write out when a file closed count as
        // unpublished until they had time to arrive.
        let settling = state
            .settling
            .is_some_and(|closed| closed.elapsed() < PACING.max_age);
        if !self.read_only && !faulted && (settling || state.volume.has_unsynced_changes()) {
            return Removal::Unsynced;
        }
        state.released = true;
        let closed = release();
        if faulted {
            Removal::Lost(closed)
        } else {
            Removal::Released(closed)
        }
    }

    /// The paths Windows must be told have changed, which WinFsp answers by
    /// writing out and dropping what it holds of each. The mount's timer
    /// collects them, so a request pays nothing for it and a directory that
    /// changes a thousand times is named once. Two kinds of request leave one:
    ///
    /// - The close of a file that was open with write access. Windows may hold
    ///   pages of it that a mapped view wrote, and sends them when it pleases:
    ///   60 s after such a close none had arrived, and an eject then lost
    ///   them.
    /// - The close of a file that was read, once `KEPT_FILES` read since are
    ///   closed too.
    /// - A create, delete or rename in a directory that is open, as a
    ///   program's current directory is. Its times changed, and Windows reads
    ///   an open object's information only once.
    pub fn take_stale(&self) -> Vec<Vec<u16>> {
        match self.state.lock() {
            Ok(mut state) => {
                let mut stale = std::mem::take(&mut state.stale);
                stale.append(&mut state.touched);
                stale
            }
            Err(_) => Vec::new(),
        }
    }

    /// Take the volume back for eject once WinFsp has stopped dispatching.
    pub fn into_volume(self) -> Reply<Volume<D>> {
        let state = self
            .state
            .into_inner()
            .map_err(|_| STATUS_UNEXPECTED_IO_ERROR)?;
        Ok(state.volume)
    }

    /// Close every handle WinFsp left open, as happens when a device
    /// disappears before its files are closed.
    pub fn close_all(&self) -> Reply<()> {
        self.with(|s| {
            s.cleaned.clear();
            s.kept.clear();
            // Close every handle and report the first failure: returning at
            // it would drop the rest of the table with their handles open.
            let mut result = Ok(());
            for (_, open) in s.open.drain() {
                let closed = s.volume.close_handle(open.handle).map_err(|e| status(&e));
                result = result.and(closed);
            }
            result
        })
    }

    /// Attributes for `GetSecurityByName`; also the existence check before create.
    pub fn attributes_by_name(&self, path: &[u16]) -> Reply<u32> {
        self.with(|s| {
            let (object, _, _) = s.locate(path)?;
            let attributes = s.known.attributes(&mut s.volume, object)?;
            Ok(file_info(&mut s.volume, object, &attributes).file_attributes)
        })
    }

    /// Create a file or directory. Also returns the path in its stored
    /// spelling, which WinFsp reports as the file's normalized name.
    pub fn create(
        &self,
        path: &[u16],
        directory: bool,
        write_through: bool,
    ) -> Reply<(u64, FileInfo, Vec<u16>)> {
        self.change(|s| {
            let (parent, name, mut normalized) = s.parent(path)?;
            let (object, handle) = if directory {
                let object =
                    with_space(&mut s.volume, |v| v.create_directory(parent, name.clone()))
                        .map_err(|e| status(&e))?;
                let handle = s
                    .volume
                    .open_object(object, Access::Read)
                    .map_err(|e| open_status(&e))?;
                (object, handle)
            } else {
                let handle = with_space(&mut s.volume, |v| v.create_file(parent, name.clone()))
                    .map_err(|e| open_status(&e))?;
                match s.volume.lookup(parent, &name) {
                    Ok(object) => (object, handle),
                    // The file stays created, as after any failed publication-side step.
                    Err(e) => {
                        s.volume.close_handle(handle).map_err(|e| status(&e))?;
                        return Err(status(&e));
                    }
                }
            };
            let kind = if directory {
                ObjectKind::Directory
            } else {
                ObjectKind::File
            };
            // A new entry is stored with the requested spelling.
            push_component(&mut normalized, &name)?;
            s.touched(parent, path);
            if write_through {
                s.publish()?;
            }
            let known = (!directory).then(|| path.to_vec());
            let (context, info) =
                s.insert(object, kind, handle, known, !directory, write_through)?;
            Ok((context, info, normalized))
        })
    }

    pub fn open(
        &self,
        path: &[u16],
        write: bool,
        write_through: bool,
    ) -> Reply<(u64, FileInfo, Vec<u16>)> {
        self.with(|s| {
            let (object, kind, normalized) = s.locate(path)?;
            // WinFsp has already checked the granted access and sharing.
            let access = if kind == ObjectKind::File && write && !self.read_only {
                Access::ReadWrite
            } else {
                Access::Read
            };
            let handle = s
                .volume
                .open_object(object, access)
                .map_err(|e| open_status(&e))?;
            let known = (kind == ObjectKind::File).then(|| path.to_vec());
            let writable = access == Access::ReadWrite;
            let (context, info) = s.insert(object, kind, handle, known, writable, write_through)?;
            Ok((context, info, normalized))
        })
    }

    /// Truncate for `CREATE_ALWAYS`, `TRUNCATE_EXISTING`, and supersede.
    /// Attribute bits are not stored.
    pub fn overwrite(&self, context: u64) -> Reply<FileInfo> {
        self.change(|s| {
            let open = get(&s.open, context)?;
            with_space(&mut s.volume, |v| v.truncate(&open.handle, 0)).map_err(|e| status(&e))?;
            s.publish_if_write_through(context)?;
            s.info(context)
        })
    }

    /// A handle was closed: delete a delete-pending object at its last
    /// cleanup, then give up the context's core handle. Other opens keep an
    /// unlinked file readable until their own cleanup. Cleanup cannot fail.
    pub fn cleanup(&self, context: u64, path: Option<&[u16]>, flags: u32) {
        let delete = flags & CLEANUP_DELETE != 0;
        if delete {
            let result = self.change(|s| {
                let kind = match get(&s.open, context)?.kind {
                    ObjectKind::File | ObjectKind::Symlink => RemoveKind::File,
                    ObjectKind::Directory => RemoveKind::Directory,
                };
                let path = path.ok_or(STATUS_INVALID_PARAMETER)?;
                let (parent, name, _) = s.parent(path)?;
                s.volume
                    .remove(parent, &name, kind)
                    .map_err(|e| status(&e))?;
                s.touched(parent, path);
                // A path through a removed directory leads nowhere now.
                if kind == RemoveKind::Directory {
                    s.known.directory = None;
                }
                // The removed object awaits cleanup, which `tick` runs.
                s.quiet = None;
                Ok(())
            });
            if let Err(code) = result {
                eprintln!("carryfs: delete on cleanup failed: NTSTATUS {code:#010x}");
            }
        }
        if let Err(code) = self.with(|s| s.release(context, !delete)) {
            eprintln!("carryfs: cleanup failed: NTSTATUS {code:#010x}");
        }
    }

    pub fn close(&self, context: u64) {
        if let Err(code) = self.with(|s| s.close(context)) {
            eprintln!("carryfs: close failed: NTSTATUS {code:#010x}");
        }
    }

    pub fn read(&self, context: u64, offset: u64, buffer: &mut [u8]) -> Reply<usize> {
        loop {
            // A read that continues where the last one used up its window
            // needs the window being read ahead. While that is still being
            // read, wait for it without the volume lock and come back.
            match self.read_now(context, offset, buffer)? {
                Ok(done) => return Ok(done),
                Err(window) => match &self.waits {
                    Some(waits) => waits.arrival(&window),
                    None => return Err(STATUS_UNEXPECTED_IO_ERROR),
                },
            }
        }
    }

    /// Read, unless this read would first wait for the window it names.
    fn read_now(
        &self,
        context: u64,
        offset: u64,
        buffer: &mut [u8],
    ) -> Reply<Result<usize, Window>> {
        self.with(|s| {
            if let Some(open) = s.open.get_mut(&context)
                && let Some((_, window)) = open.next_window.take().filter(|(at, _)| *at == offset)
                && !window.arrived()
            {
                return Ok(Err(window));
            }
            // What an earlier request left says nothing about this read.
            self.waits.as_ref().and_then(Waits::take_next);
            let lent = s.lend(context, Access::Read)?;
            let done = s.read_at(&lent, offset, buffer);
            let window = self.waits.as_ref().and_then(Waits::take_next);
            s.take_back(lent)?;
            let done = done?;
            if let Some(open) = s.open.get_mut(&context) {
                open.read = true;
                if let Some(window) = window {
                    open.next_window = Some((offset + done as u64, window));
                }
            }
            Ok(Ok(done))
        })
    }

    /// `to_end` appends; `constrained` (paging I/O) must not extend the file.
    pub fn write(
        &self,
        context: u64,
        offset: u64,
        data: &[u8],
        to_end: bool,
        constrained: bool,
    ) -> Reply<(usize, FileInfo)> {
        // A stream of writes outruns the device; this is where it waits.
        if let Some(waits) = &self.waits {
            waits.before_write();
        }
        self.change(|s| {
            let lent = s.lend(context, Access::ReadWrite)?;
            let done = s.write_at(&lent, offset, data, to_end, constrained);
            s.take_back(lent)?;
            let done = done?;
            s.publish_if_write_through(context)?;
            Ok((done, s.info(context)?))
        })
    }

    /// `FlushFileBuffers`: publish pending changes and wait for durability.
    /// A file flush leaves cleanup to `tick`; a volume flush also runs it.
    pub fn flush(&self, context: Option<u64>) -> Reply<FileInfo> {
        self.change(|s| {
            s.quiet = None;
            match context {
                Some(context) => {
                    s.publish()?;
                    s.info(context)
                }
                None => {
                    s.volume.sync().map_err(|e| status(&e))?;
                    s.waiting_since = None;
                    Ok(FileInfo::default())
                }
            }
        })
    }

    pub fn file_info(&self, context: u64) -> Reply<FileInfo> {
        self.with(|s| s.info(context))
    }

    /// Only creation and last-write times are stored. Zero means "unchanged";
    /// attribute bits, access time, and change time are accepted and ignored.
    pub fn set_basic_info(&self, context: u64, creation: u64, last_write: u64) -> Reply<FileInfo> {
        self.change(|s| {
            let creation = (creation != 0).then(|| timestamp(creation)).transpose()?;
            let modification = (last_write != 0)
                .then(|| timestamp(last_write))
                .transpose()?;
            if creation.is_some() || modification.is_some() {
                let handle = &get(&s.open, context)?.handle;
                s.volume
                    .set_times(handle, creation, modification)
                    .map_err(|e| status(&e))?;
            }
            s.info(context)
        })
    }

    /// Allocation size is derived from extents, so setting it only truncates
    /// when it drops below the file size.
    pub fn set_file_size(&self, context: u64, size: u64, allocation: bool) -> Reply<FileInfo> {
        self.change(|s| {
            let handle = &get(&s.open, context)?.handle;
            let current = s
                .volume
                .handle_attributes(handle)
                .map_err(|e| status(&e))?
                .size;
            if !allocation || size < current {
                with_space(&mut s.volume, |v| v.truncate(handle, size)).map_err(|e| status(&e))?;
                s.publish_if_write_through(context)?;
            }
            s.info(context)
        })
    }

    /// WinFsp asks before it sets a delete disposition. A read-only mount must
    /// refuse here: `Cleanup` cannot report an error, so a delete accepted
    /// here would look successful and do nothing.
    pub fn can_delete(&self, context: u64) -> Reply<()> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        self.with(|s| {
            let open = get(&s.open, context)?;
            if open.kind == ObjectKind::Directory {
                let page = s
                    .volume
                    .read_directory(&open.handle, None)
                    .map_err(|e| status(&e))?;
                if !page.entries.is_empty() {
                    return Err(STATUS_DIRECTORY_NOT_EMPTY);
                }
            }
            Ok(())
        })
    }

    pub fn rename(&self, path: &[u16], new_path: &[u16], replace: bool) -> Reply<()> {
        self.change_names(|s| {
            let (source_parent, source, _) = s.parent(path)?;
            let (destination_parent, destination, _) = s.parent(new_path)?;
            let moved = s
                .volume
                .lookup(source_parent, &source)
                .map_err(|e| status(&e))?;
            let replace = if replace {
                // Windows never replaces a directory by rename.
                if let Ok(existing) = s.volume.lookup(destination_parent, &destination) {
                    let kind = s.volume.attributes(existing).map_err(|e| status(&e))?.kind;
                    if kind == ObjectKind::Directory && existing != moved {
                        return Err(STATUS_ACCESS_DENIED);
                    }
                }
                Replace::Allowed
            } else {
                Replace::Forbidden
            };
            with_space(&mut s.volume, |v| {
                v.rename(
                    source_parent,
                    &source,
                    destination_parent,
                    destination.clone(),
                    replace,
                )
            })
            .map_err(|e| status(&e))?;
            // A replaced destination becomes an orphan awaiting cleanup by `tick`.
            s.quiet = None;
            // WinFsp knows the file by its new name from here on. A renamed
            // directory has no open file below it: Windows refuses that.
            for open in s.open.values_mut().filter(|open| open.object == moved) {
                if let Some(known) = &mut open.path {
                    *known = new_path.to_vec();
                }
            }
            for (context, known) in &mut s.kept {
                if s.cleaned.get(context) == Some(&moved) {
                    *known = new_path.to_vec();
                }
            }
            s.touched(source_parent, path);
            if destination_parent != source_parent {
                s.touched(destination_parent, new_path);
            }
            Ok(())
        })
    }

    /// List entries after `marker`, the last name WinFsp received, until `add`
    /// reports a full buffer. Returns true when the listing ended, so the
    /// caller can terminate the buffer. A call that continues the previous one
    /// with nothing changed in between takes up the page that one stopped in;
    /// any other resumes by name, which keeps deletes during enumeration from
    /// skipping or repeating unchanged entries.
    pub fn read_directory(
        &self,
        context: u64,
        marker: Option<&[u16]>,
        mut add: impl FnMut(&[u16], &FileInfo) -> bool,
    ) -> Reply<bool> {
        self.with(|s| {
            let changes = s.changes;
            let open = s.open.get_mut(&context).ok_or(STATUS_INVALID_HANDLE)?;
            let object = open.object;
            let resumed = open.listing.take().filter(|listing| {
                listing.changes == changes && marker == Some(listing.marker.as_slice())
            });
            // `pending` holds entries read from the core and not yet returned;
            // `after` is the name the next page follows, none for the first.
            let (mut pending, mut more, mut after) = (VecDeque::new(), true, None);
            if let Some(listing) = resumed {
                (pending, more) = (listing.rest, listing.more);
            } else {
                let attributes = s.known.attributes(&mut s.volume, object)?;
                let text = marker
                    .map(|marker| String::from_utf16(marker).map_err(|_| STATUS_INVALID_PARAMETER))
                    .transpose()?;
                after = match text.as_deref() {
                    // The root has no "." or ".." entries, as on NTFS.
                    None | Some(".") | Some("..") if object == ROOT => None,
                    None | Some(".") => {
                        if text.is_none()
                            && !add(&utf16("."), &file_info(&mut s.volume, object, &attributes))
                        {
                            return Ok(false);
                        }
                        let parent = s.known.attributes(&mut s.volume, attributes.parent)?;
                        if !add(
                            &utf16(".."),
                            &file_info(&mut s.volume, attributes.parent, &parent),
                        ) {
                            return Ok(false);
                        }
                        None
                    }
                    Some("..") => None,
                    Some(name) => Some(decode_alias(name)?),
                };
            }
            let handle = &get(&s.open, context)?.handle;
            let mut returned = None;
            let ended = loop {
                let Some(entry) = pending.front() else {
                    if !more {
                        break true;
                    }
                    let page = match &after {
                        Some(name) => s.volume.read_directory_after(handle, name),
                        None => s.volume.read_directory(handle, None),
                    }
                    .map_err(|e| status(&e))?;
                    if page.entries.is_empty() {
                        break true;
                    }
                    more = page.cursor.is_some();
                    pending = page.entries.into();
                    continue;
                };
                let child = s.known.attributes(&mut s.volume, entry.object)?;
                let mut name = Vec::new();
                push_alias(&mut name, &entry.name)?;
                if !add(&name, &file_info(&mut s.volume, entry.object, &child)) {
                    break false;
                }
                returned = Some(name);
                after = pending.pop_front().map(|entry| entry.name);
            };
            // WinFsp's buffer filled inside a page: keep its tail for the call
            // that continues from the last name returned.
            if !ended
                && !pending.is_empty()
                && let Some(marker) = returned.or_else(|| marker.map(<[u16]>::to_vec))
            {
                let open = s.open.get_mut(&context).ok_or(STATUS_INVALID_HANDLE)?;
                open.listing = Some(Listing {
                    changes,
                    marker,
                    rest: pending,
                    more,
                });
            }
            Ok(ended)
        })
    }

    /// A symlink's `REPARSE_DATA_BUFFER`, for `GetReparsePoint`.
    pub fn reparse(&self, context: u64) -> Reply<Vec<u8>> {
        self.with(|s| {
            let object = get(&s.open, context)?.object;
            reparse_data(&mut s.volume, object)
        })
    }

    /// A symlink's `REPARSE_DATA_BUFFER` by path, for WinFsp's reparse point
    /// helpers. They walk a path one prefix at a time, so every prefix they
    /// ask about is a directory or a link.
    pub fn reparse_by_name(&self, path: &[u16]) -> Reply<Vec<u8>> {
        self.with(|s| {
            let (object, _, _) = s.locate(path)?;
            reparse_data(&mut s.volume, object)
        })
    }

    /// `FSCTL_SET_REPARSE_POINT`: Windows creates a symlink as an empty file
    /// or directory, then sets this, so it converts that object in place.
    /// `on_volume` is WinFsp's `TargetOnFileSystem`: for an absolute target on
    /// this volume, the byte offset where its volume-relative path starts.
    pub fn set_reparse(
        &self,
        context: u64,
        path: &[u16],
        data: &[u8],
        on_volume: u16,
    ) -> Reply<()> {
        let target = symlink_target(data, on_volume)?;
        self.change_names(|s| {
            let (parent, name, _) = s.parent(path)?;
            with_space(&mut s.volume, |v| {
                v.make_symlink(parent, &name, target.as_bytes())
            })
            .map_err(|e| match e {
                // Only the target can be too long here; it is not a name.
                Error::TooLong(_) => STATUS_INVALID_PARAMETER,
                e => status(&e),
            })?;
            let object = get(&s.open, context)?.object;
            for open in s.open.values_mut().filter(|open| open.object == object) {
                open.kind = ObjectKind::Symlink;
            }
            Ok(())
        })
    }

    /// Total and available bytes; available excludes the maintenance reserve.
    pub fn volume_size(&self) -> Reply<(u64, u64)> {
        self.with(|s| {
            let space = s.volume.space().map_err(|e| status(&e))?;
            let block = u64::from(space.block_bytes);
            Ok((
                space.total_blocks.saturating_mul(block),
                space.available_blocks.saturating_mul(block),
            ))
        })
    }
}

impl<D: BlockDevice> State<D> {
    fn insert(
        &mut self,
        object: ObjectId,
        kind: ObjectKind,
        handle: Handle,
        path: Option<Vec<u16>>,
        writable: bool,
        write_through: bool,
    ) -> Reply<(u64, FileInfo)> {
        let context = self.next;
        self.next += 1;
        self.open.insert(
            context,
            Open {
                object,
                kind,
                handle,
                path,
                writable,
                read: false,
                write_through,
                listing: None,
                next_window: None,
            },
        );
        match self.info(context) {
            Ok(info) => Ok((context, info)),
            // WinFsp gets no context for a failed open or create, so it never
            // closes this one.
            Err(error) => {
                self.close(context)?;
                Err(error)
            }
        }
    }

    fn info(&mut self, context: u64) -> Reply<FileInfo> {
        let object = self.object(context)?;
        let attributes = self.known.attributes(&mut self.volume, object)?;
        Ok(file_info(&mut self.volume, object, &attributes))
    }

    /// Resolve a path as `resolve_entry` does, but through `directory`, so
    /// the entries of one directory share the walk to it.
    fn locate(&mut self, path: &[u16]) -> Reply<(ObjectId, ObjectKind, Vec<u16>)> {
        let Some((prefix, last)) = split(path) else {
            return Ok((ROOT, ObjectKind::Directory, Vec::new()));
        };
        let name = component(last)?;
        let (parent, mut normalized) = self.directory(prefix)?;
        let entry = match self.volume.lookup_entry(parent, &name) {
            Ok(entry) => entry,
            Err(Error::NotFound) => return Err(STATUS_OBJECT_NAME_NOT_FOUND),
            Err(e) => return Err(status(&e)),
        };
        push_component(&mut normalized, &entry.name)?;
        Ok((entry.object, entry.kind, normalized))
    }

    /// The directory at `prefix` and its stored spelling, remembered for the
    /// next path in it: listing or copying a directory asks for its entries
    /// one after another. A missing or non-directory prefix is "path not found".
    fn directory(&mut self, prefix: &[u16]) -> Reply<(ObjectId, Vec<u16>)> {
        if let Some((remembered, object, normalized)) = &self.known.directory
            && remembered == prefix
        {
            return Ok((*object, normalized.clone()));
        }
        let (object, kind, normalized) = match self.resolve_entry(&components(prefix)?) {
            Err(STATUS_OBJECT_NAME_NOT_FOUND) => return Err(STATUS_OBJECT_PATH_NOT_FOUND),
            result => result?,
        };
        if kind != ObjectKind::Directory {
            return Err(STATUS_OBJECT_PATH_NOT_FOUND);
        }
        self.known.directory = Some((prefix.to_vec(), object, normalized.clone()));
        Ok((object, normalized))
    }

    /// Walk from the root, collecting each component's stored spelling. As on
    /// NTFS, a missing final name is "name not found", while a missing or
    /// non-directory intermediate is "path not found".
    fn resolve_entry(&mut self, names: &[Name]) -> Reply<(ObjectId, ObjectKind, Vec<u16>)> {
        let mut object = ROOT;
        let mut kind = ObjectKind::Directory;
        let mut normalized = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let entry = match self.volume.lookup_entry(object, name) {
                Ok(entry) => entry,
                Err(Error::NotFound)
                    if index + 1 < names.len() || kind != ObjectKind::Directory =>
                {
                    return Err(STATUS_OBJECT_PATH_NOT_FOUND);
                }
                Err(Error::NotFound) => return Err(STATUS_OBJECT_NAME_NOT_FOUND),
                Err(e) => return Err(status(&e)),
            };
            push_component(&mut normalized, &entry.name)?;
            (object, kind) = (entry.object, entry.kind);
        }
        Ok((object, kind, normalized))
    }

    /// The parent directory, the final name, and the parent's stored spelling,
    /// for a path that is not the root.
    fn parent(&mut self, path: &[u16]) -> Reply<(ObjectId, Name, Vec<u16>)> {
        let (prefix, last) = split(path).ok_or(STATUS_ACCESS_DENIED)?;
        let name = component(last)?;
        let (parent, normalized) = self.directory(prefix)?;
        Ok((parent, name, normalized))
    }

    /// The object behind a context, open or awaiting its Close.
    fn object(&self, context: u64) -> Reply<ObjectId> {
        match self.open.get(&context) {
            Some(open) => Ok(open.object),
            None => self
                .cleaned
                .get(&context)
                .copied()
                .ok_or(STATUS_INVALID_HANDLE),
        }
    }

    /// The core handle for a read or a write on `context`: its own when that
    /// allows `access`. Windows also reads and writes a file's cached and
    /// mapped pages through whichever context first cached or mapped it,
    /// though that one may have had its Cleanup, or be open for reading while
    /// another handle's mapped view is written. Such a request gets a handle
    /// of its own, which `take_back` closes.
    fn lend(&mut self, context: u64, access: Access) -> Reply<Lent> {
        let allows = |open: &Open| open.writable || access == Access::Read;
        let object = match self.open.get(&context) {
            Some(open) if allows(open) => return Ok(Lent::Of(context)),
            Some(open) => open.object,
            None => self.object(context)?,
        };
        match self.volume.open_object(object, access) {
            Ok(handle) => Ok(Lent::Opened(handle)),
            // With every handle taken, the handle whose read or mapped view
            // caused this request can serve it.
            Err(Error::LimitExceeded) => self
                .open
                .iter()
                .find(|(_, open)| open.object == object && allows(open))
                .map(|(&other, _)| Lent::Of(other))
                .ok_or(STATUS_TOO_MANY_OPENED_FILES),
            Err(e) => Err(open_status(&e)),
        }
    }

    fn take_back(&mut self, lent: Lent) -> Reply<()> {
        match lent {
            Lent::Of(_) => Ok(()),
            Lent::Opened(handle) => self.volume.close_handle(handle).map_err(|e| status(&e)),
        }
    }

    fn read_at(&mut self, lent: &Lent, offset: u64, buffer: &mut [u8]) -> Reply<usize> {
        let handle = lent.handle(&self.open)?;
        let size = self
            .volume
            .handle_attributes(handle)
            .map_err(|e| status(&e))?
            .size;
        if offset >= size {
            return Err(STATUS_END_OF_FILE);
        }
        let mut done = 0;
        while done < buffer.len() {
            let position = offset
                .checked_add(done as u64)
                .ok_or(STATUS_INVALID_PARAMETER)?;
            let read = self
                .volume
                .read(handle, position, &mut buffer[done..])
                .map_err(|e| status(&e))?;
            if read == 0 {
                break;
            }
            done += read;
        }
        Ok(done)
    }

    fn write_at(
        &mut self,
        lent: &Lent,
        offset: u64,
        data: &[u8],
        to_end: bool,
        constrained: bool,
    ) -> Reply<usize> {
        let handle = lent.handle(&self.open)?;
        let size = self
            .volume
            .handle_attributes(handle)
            .map_err(|e| status(&e))?
            .size;
        let offset = if to_end { size } else { offset };
        let data = if constrained {
            let room = usize::try_from(size.saturating_sub(offset)).unwrap_or(usize::MAX);
            &data[..data.len().min(room)]
        } else {
            data
        };
        let mut done = 0;
        while done < data.len() {
            let position = offset
                .checked_add(done as u64)
                .ok_or(STATUS_FILE_TOO_LARGE)?;
            match with_space(&mut self.volume, |v| {
                v.write(handle, position, &data[done..])
            }) {
                Ok(written) => done += written,
                // Report the accepted prefix and let the caller retry, which
                // surfaces the error. Log it here in case none does.
                Err(error) if done > 0 => {
                    eprintln!("carryfs: write stopped after {done} bytes: {error}");
                    break;
                }
                Err(e) => return Err(status(&e)),
            }
        }
        Ok(done)
    }

    /// An entry was added to, removed from or renamed in `directory`, the
    /// parent of `child`. Its times moved, which Windows learns of an open
    /// directory only when told.
    fn touched(&mut self, directory: ObjectId, child: &[u16]) {
        if self.open.values().any(|open| open.object == directory) {
            let path = match split(child) {
                Some((parent, _)) if !parent.is_empty() => parent,
                _ => &[u16::from(b'\\')],
            };
            if !self.touched.iter().any(|known| known == path) {
                self.touched.push(path.to_vec());
            }
        }
    }

    /// Cleanup: close the context's core handle and keep its object for the
    /// requests Windows may still send before the Close. A file that still
    /// `exists` may leave pages with Windows: ones a mapped view wrote, if it
    /// was open with write access, or the data it was read for.
    fn release(&mut self, context: u64, exists: bool) -> Reply<()> {
        let mut open = self.open.remove(&context).ok_or(STATUS_INVALID_HANDLE)?;
        self.cleaned.insert(context, open.object);
        match open.path.take().filter(|_| exists) {
            Some(path) if open.writable => {
                self.stale.push(path);
                self.settling = Some(Instant::now());
            }
            Some(path) if open.read => {
                self.kept.push_back((context, path));
                // Windows may have dropped the oldest already, and closed it.
                if self.kept.len() > KEPT_FILES
                    && let Some((oldest, path)) = self.kept.pop_front()
                    && self.cleaned.contains_key(&oldest)
                {
                    self.stale.push(path);
                }
            }
            _ => {}
        }
        self.close_handle(open)
    }

    fn close(&mut self, context: u64) -> Reply<()> {
        if self.cleaned.remove(&context).is_some() {
            return Ok(());
        }
        // WinFsp closes without a Cleanup an open it could not complete.
        let open = self.open.remove(&context).ok_or(STATUS_INVALID_HANDLE)?;
        self.close_handle(open)
    }

    fn close_handle(&mut self, open: Open) -> Reply<()> {
        self.volume
            .close_handle(open.handle)
            .map_err(|e| status(&e))?;
        let pinned = self.open.values().any(|other| other.object == open.object);
        // Only an unpinned orphan is new cleanup work for `tick`. Closing
        // anything else cannot have created any.
        if !pinned
            && self
                .known
                .attributes(&mut self.volume, open.object)
                .map_or(true, |attributes| attributes.orphan)
        {
            self.quiet = None;
        }
        Ok(())
    }

    /// Publish before replying to a request on a write-through handle.
    fn publish_if_write_through(&mut self, context: u64) -> Reply<()> {
        if self
            .open
            .get(&context)
            .is_some_and(|open| open.write_through)
        {
            self.publish()?;
        }
        Ok(())
    }

    /// Publish pending changes for a request that asked for it. The timer's
    /// age of pending work starts again: left running, it made the timer
    /// checkpoint in the middle of the burst that followed a flush, 1,000
    /// small files 5 to 8% slower.
    fn publish(&mut self) -> Reply<()> {
        self.volume.sync_publication().map_err(|e| status(&e))?;
        self.waiting_since = None;
        Ok(())
    }

    fn tick(&mut self, now: Instant, pacing: Pacing) -> Result<(), Error> {
        let before = self.volume.info()?.generation;
        let pending = self.volume.has_unsynced_changes();
        if self.quiet == Some(before) && !pending {
            self.waiting_since = None;
            return Ok(());
        }
        let since = *self.waiting_since.get_or_insert(now);
        if !pacing.due(now, self.last_operation, since) {
            return Ok(());
        }
        self.waiting_since = None;
        // Cleanup rewrites no inode of a reachable object, so what `known`
        // holds stays true across it.
        self.volume.sync()?;
        if !pending && self.volume.info()?.generation == before {
            self.quiet = Some(before);
        }
        Ok(())
    }
}

/// Whose core handle serves a read or a write; see `State::lend`.
enum Lent {
    /// The handle of this open context.
    Of(u64),
    /// A handle opened for the one request.
    Opened(Handle),
}

impl Lent {
    fn handle<'a>(&'a self, open: &'a HashMap<u64, Open>) -> Reply<&'a Handle> {
        match self {
            Lent::Of(context) => Ok(&get(open, *context)?.handle),
            Lent::Opened(handle) => Ok(handle),
        }
    }
}

/// Borrow only the open table, so the volume stays mutably borrowable.
fn get(open: &HashMap<u64, Open>, context: u64) -> Reply<&Open> {
    open.get(&context).ok_or(STATUS_INVALID_HANDLE)
}

/// Retry an allocating operation once after draining cleanup. Removal
/// reclaims space in bounded steps, so `NoSpace` right after deleting a large
/// file is usually transient. Every other request waits for the drain.
fn with_space<D: BlockDevice, T>(
    volume: &mut Volume<D>,
    mut operation: impl FnMut(&mut Volume<D>) -> Result<T, Error>,
) -> Result<T, Error> {
    match operation(volume) {
        Err(Error::NoSpace) => {
            drain(volume)?;
            operation(volume)
        }
        result => result,
    }
}

/// Run maintenance until a step publishes nothing.
fn drain<D: BlockDevice>(volume: &mut Volume<D>) -> Result<(), Error> {
    // Stop when a step leaves the generation unchanged.
    // Cap the drain so maintenance that never quiesces cannot loop forever.
    for _ in 0..1_000_000 {
        let before = volume.info()?.generation;
        volume.sync()?;
        if volume.info()?.generation == before {
            break;
        }
    }
    Ok(())
}

/// Split a WinFsp path such as `\dir\file` into validated names.
fn components(path: &[u16]) -> Reply<Vec<Name>> {
    path.split(|&unit| unit == u16::from(b'\\'))
        .filter(|part| !part.is_empty())
        .map(component)
        .collect()
}

/// Validate one path component.
fn component(part: &[u16]) -> Reply<Name> {
    // Unpaired surrogates have no UTF-8 spelling.
    let text = String::from_utf16(part).map_err(|_| STATUS_OBJECT_NAME_INVALID)?;
    decode_alias(&text)
}

/// Split a WinFsp path into the path of its directory and its last component,
/// ignoring trailing separators. The root has neither.
fn split(path: &[u16]) -> Option<(&[u16], &[u16])> {
    let separator = u16::from(b'\\');
    let end = path.iter().rposition(|&unit| unit != separator)? + 1;
    let path = &path[..end];
    Some(match path.iter().rposition(|&unit| unit == separator) {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (&path[..0], path),
    })
}

/// Append `\name` in its Windows spelling to a normalized path.
fn push_component(path: &mut Vec<u16>, name: &Name) -> Reply<()> {
    path.push(u16::from(b'\\'));
    push_alias(path, name)
}

/// Append a stored name in its Windows spelling. Most names are ASCII that
/// Windows accepts, which `alias` would copy unchanged through two buffers.
fn push_alias(units: &mut Vec<u16>, name: &Name) -> Reply<()> {
    let stored = name.stored();
    let plain = stored.is_ascii()
        && !matches!(stored.last(), Some(b' ' | b'.'))
        && !stored.iter().any(|byte| b"\"*:<>?\\|".contains(byte))
        && !name.is_windows_device_name();
    if plain {
        units.extend(stored.iter().map(|&byte| u16::from(byte)));
    } else {
        units.extend(alias(name)?.encode_utf16());
    }
    Ok(())
}

const ESCAPE: char = '\u{E000}';
const RESERVED_PREFIX: char = '\u{E001}';

/// Characters Windows forbids, in Services for Macintosh order. Each shows as
/// U+F020 plus its index, the stand-ins Samba and the macOS SMB client also
/// use, so copies made on Windows can regain the original name there. Space
/// and period are forbidden only at the end of a name.
const SFM_ORIGINALS: &str = "\"*:<>?\\| .";
const SFM_FIRST: u32 = 0xF020;

fn sfm(character: char) -> Option<char> {
    let index = u32::try_from(SFM_ORIGINALS.find(character)?).ok()?;
    char::from_u32(SFM_FIRST + index)
}

fn from_sfm(character: char) -> Option<char> {
    let index = u32::from(character).checked_sub(SFM_FIRST)?;
    SFM_ORIGINALS.chars().nth(usize::try_from(index).ok()?)
}

fn fullwidth(character: char) -> Reply<char> {
    char::from_u32(u32::from(character) + 0xFEE0).ok_or(STATUS_FILE_CORRUPT_ERROR)
}

/// The reserved device name that `text` spells with a fullwidth first character.
fn device_name(text: &str) -> Option<Name> {
    let mut characters = text.chars();
    let first = characters
        .next()
        .filter(|character| ('\u{FF01}'..='\u{FF5E}').contains(character))?;
    let ascii = char::from_u32(u32::from(first) - 0xFEE0)?;
    Name::parse(&format!("{ascii}{}", characters.as_str()))
        .ok()
        .filter(Name::is_windows_device_name)
}

/// Encode a stored name into a Windows-safe, directly reversible component.
fn alias(name: &Name) -> Reply<String> {
    let text = std::str::from_utf8(name.stored()).map_err(|_| STATUS_FILE_CORRUPT_ERROR)?;
    encode(text, name.is_windows_device_name())
}

/// `alias` over text that may not be NFC yet, so decoding can compare a
/// caller's decomposed spelling with its own encoding.
fn encode(text: &str, reserved: bool) -> Reply<String> {
    let mut output = String::with_capacity(text.len());
    if reserved
        && !text
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii())
    {
        output.push(RESERVED_PREFIX);
    }
    for (index, character) in text.char_indices() {
        let last = index + character.len_utf8() == text.len();
        if index == 0 && reserved && character.is_ascii() {
            output.push(fullwidth(character)?);
        } else if let Some(substitute) =
            sfm(character).filter(|_| last || !matches!(character, ' ' | '.'))
        {
            output.push(substitute);
        } else {
            if matches!(character, ESCAPE | RESERVED_PREFIX)
                || from_sfm(character).is_some()
                || (index == 0 && device_name(text).is_some())
            {
                output.push(ESCAPE);
            }
            output.push(character);
        }
    }
    debug_assert!(output.encode_utf16().count() <= 255);
    Ok(output)
}

/// Decode a Windows-visible component before the core's indexed lookup. Only
/// the spelling `alias` produces is accepted, so raw forbidden spellings such
/// as `what?` or `CON` are invalid.
fn decode_alias(text: &str) -> Reply<Name> {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars();
    if text.starts_with(RESERVED_PREFIX) {
        characters.next();
    }
    while let Some(character) = characters.next() {
        output.push(match character {
            ESCAPE => characters.next().ok_or(STATUS_OBJECT_NAME_INVALID)?,
            _ => from_sfm(character).unwrap_or(character),
        });
    }
    // Checked first: the fullwidth spelling of a 255-byte device name is too
    // long to parse as a literal name.
    if let Some(device) = device_name(&output)
        && alias(&device)? == text
    {
        return Ok(device);
    }
    // Compare before NFC normalization: a decomposed spelling is canonical
    // input for the name it normalizes to.
    let name = Name::parse(&output).map_err(|error| status(&error))?;
    if encode(&output, name.is_windows_device_name())? != text {
        return Err(STATUS_OBJECT_NAME_INVALID);
    }
    Ok(name)
}

fn alias_target(target: &str) -> Reply<String> {
    target
        .split('/')
        .map(|part| match part {
            "" | "." | ".." => Ok(part.to_owned()),
            _ => alias(&Name::parse(part).map_err(|error| status(&error))?),
        })
        .collect::<Reply<Vec<_>>>()
        .map(|parts| parts.join("\\"))
}

fn decode_target(target: &str) -> Reply<String> {
    target
        .split('\\')
        .map(|part| match part {
            "" | "." | ".." => Ok(part.to_owned()),
            _ => {
                let name = decode_alias(part)?;
                Ok(std::str::from_utf8(name.stored())
                    .map_err(|_| STATUS_OBJECT_NAME_INVALID)?
                    .to_owned())
            }
        })
        .collect::<Reply<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn file_info<D: BlockDevice>(
    volume: &mut Volume<D>,
    object: ObjectId,
    attributes: &Attributes,
) -> FileInfo {
    let (file_attributes, reparse_tag) = match attributes.kind {
        ObjectKind::File => (FILE_ATTRIBUTE_NORMAL, 0),
        ObjectKind::Directory => (FILE_ATTRIBUTE_DIRECTORY, 0),
        ObjectKind::Symlink
            if attributes
                .directory_link
                .unwrap_or_else(|| targets_directory(volume, object, attributes.parent)) =>
        {
            (
                FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
                IO_REPARSE_TAG_SYMLINK,
            )
        }
        ObjectKind::Symlink => (FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_SYMLINK),
    };
    let modification = filetime(attributes.modification);
    FileInfo {
        file_attributes,
        reparse_tag,
        allocation_size: attributes.allocated_bytes,
        file_size: attributes.size,
        creation_time: filetime(attributes.creation),
        // Access times are not stored.
        last_access_time: modification,
        last_write_time: modification,
        change_time: filetime(attributes.metadata_change),
        index_number: object.get(),
        ..FileInfo::default()
    }
}

/// Whether a link made on another OS names a directory in this volume.
/// Windows keeps file and directory links apart; a link Windows made records
/// which in `directory_link`, whatever its target, and for the rest the target
/// decides, as in WinFsp's FUSE layer. A target that leaves the volume, passes through
/// another link, or does not exist is a file link.
fn targets_directory<D: BlockDevice>(
    volume: &mut Volume<D>,
    link: ObjectId,
    parent: ObjectId,
) -> bool {
    let Ok(target) = volume.read_symlink(link) else {
        return false;
    };
    let Ok(target) = std::str::from_utf8(&target) else {
        return false;
    };
    let mut current = if target.starts_with('/') {
        ROOT
    } else {
        parent
    };
    for part in target.split('/') {
        current = match part {
            "" | "." => continue,
            ".." => match volume.attributes(current) {
                Ok(attributes) => attributes.parent,
                Err(_) => return false,
            },
            name => match Name::parse(name).and_then(|name| volume.lookup_entry(current, &name)) {
                Ok(entry) if entry.kind == ObjectKind::Directory => entry.object,
                _ => return false,
            },
        };
    }
    true
}

/// Encode a symlink as a relative `REPARSE_DATA_BUFFER`, one path serving as
/// both substitute and print name, as WinFsp's FUSE layer does. A target
/// starting with `/` reads as relative to the volume root.
fn reparse_data<D: BlockDevice>(volume: &mut Volume<D>, object: ObjectId) -> Reply<Vec<u8>> {
    let target = match volume.read_symlink(object) {
        Ok(target) => target,
        Err(Error::WrongType) => return Err(STATUS_NOT_A_REPARSE_POINT),
        Err(e) => return Err(status(&e)),
    };
    let target = std::str::from_utf8(&target).map_err(|_| STATUS_FILE_CORRUPT_ERROR)?;
    let path: Vec<u16> = alias_target(target)?.encode_utf16().collect();
    let bytes = u16::try_from(path.len() * 2).map_err(|_| STATUS_FILE_CORRUPT_ERROR)?;
    let mut data = Vec::with_capacity(SYMLINK_HEADER + usize::from(bytes));
    data.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    data.extend_from_slice(&(12 + bytes).to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    // Substitute and print names share the one path at offset 0.
    for field in [0, bytes, 0, bytes] {
        data.extend_from_slice(&field.to_le_bytes());
    }
    data.extend_from_slice(&SYMLINK_FLAG_RELATIVE.to_le_bytes());
    data.extend(path.iter().flat_map(|unit| unit.to_le_bytes()));
    Ok(data)
}

/// Decode a symlink `REPARSE_DATA_BUFFER` into a stored target, `\` becoming
/// `/`. An absolute target on this volume is stored from its volume root, as
/// `/dir/file`, which reads back as root-relative `\dir\file`.
fn symlink_target(data: &[u8], on_volume: u16) -> Reply<String> {
    let field = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
    if data.len() < SYMLINK_HEADER {
        return Err(STATUS_IO_REPARSE_DATA_INVALID);
    }
    if u32::from_le_bytes([data[0], data[1], data[2], data[3]]) != IO_REPARSE_TAG_SYMLINK {
        return Err(STATUS_IO_REPARSE_TAG_MISMATCH);
    }
    if usize::from(field(4)) + 8 != data.len() {
        return Err(STATUS_IO_REPARSE_DATA_INVALID);
    }
    let relative =
        u32::from_le_bytes([data[16], data[17], data[18], data[19]]) & SYMLINK_FLAG_RELATIVE != 0;
    let mut start = SYMLINK_HEADER + usize::from(field(8));
    let end = start + usize::from(field(10));
    if !relative {
        // An absolute target elsewhere has no portable meaning; WinFsp's FUSE
        // layer refuses it the same way.
        if on_volume == 0 {
            return Err(STATUS_ACCESS_DENIED);
        }
        start += usize::from(on_volume);
    }
    let (units, odd) = data
        .get(start..end)
        .ok_or(STATUS_IO_REPARSE_DATA_INVALID)?
        .as_chunks::<2>();
    if !odd.is_empty() {
        return Err(STATUS_IO_REPARSE_DATA_INVALID);
    }
    let units: Vec<u16> = units.iter().map(|unit| u16::from_le_bytes(*unit)).collect();
    let target = String::from_utf16(&units).map_err(|_| STATUS_IO_REPARSE_DATA_INVALID)?;
    decode_target(&target)
}

/// Map a core error to NTSTATUS. Damage and device failures are logged here, once.
pub fn status(error: &Error) -> NtStatus {
    match error {
        Error::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        Error::NameCollision => STATUS_OBJECT_NAME_COLLISION,
        Error::WrongType => STATUS_NOT_A_DIRECTORY,
        Error::DirectoryNotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        Error::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
        Error::NoSpace => STATUS_DISK_FULL,
        Error::LimitExceeded => STATUS_FILE_TOO_LARGE,
        // Windows reports over-long components as invalid names too.
        Error::InvalidName(_) | Error::TooLong(_) => STATUS_OBJECT_NAME_INVALID,
        Error::InvalidInput(_) | Error::InvalidGeometry => STATUS_INVALID_PARAMETER,
        Error::InvalidHandle | Error::DirectoryChanged => STATUS_INVALID_HANDLE,
        Error::Busy => STATUS_DEVICE_BUSY,
        Error::Ejected => STATUS_NO_SUCH_DEVICE,
        Error::Faulted => STATUS_UNEXPECTED_IO_ERROR,
        Error::Io { .. } => {
            eprintln!("carryfs: {error}");
            STATUS_UNEXPECTED_IO_ERROR
        }
        Error::Corrupt(_) | Error::Corruption { .. } | Error::UnsupportedFormat => {
            eprintln!("carryfs: {error}");
            STATUS_FILE_CORRUPT_ERROR
        }
    }
}

/// Opening only exceeds a limit when the 4,096-handle registry is full, and
/// only a directory refuses write access.
fn open_status(error: &Error) -> NtStatus {
    match error {
        Error::LimitExceeded => STATUS_TOO_MANY_OPENED_FILES,
        Error::WrongType => STATUS_FILE_IS_A_DIRECTORY,
        error => status(error),
    }
}

/// Seconds from 1601-01-01, the FILETIME epoch, to 1970-01-01.
const FILETIME_UNIX_SECONDS: i64 = 11_644_473_600;
const TICKS_PER_SECOND: u64 = 10_000_000;

/// FILETIME ticks of 100 ns; sub-tick nanoseconds are truncated and instants
/// before 1601 clamp to zero.
fn filetime(timestamp: Timestamp) -> u64 {
    let Some(seconds) = timestamp.seconds().checked_add(FILETIME_UNIX_SECONDS) else {
        return u64::MAX;
    };
    let Ok(seconds) = u64::try_from(seconds) else {
        return 0;
    };
    seconds
        .saturating_mul(TICKS_PER_SECOND)
        .saturating_add(u64::from(timestamp.nanos() / 100))
}

fn timestamp(filetime: u64) -> Reply<Timestamp> {
    let seconds =
        i64::try_from(filetime / TICKS_PER_SECOND).map_err(|_| STATUS_INVALID_PARAMETER)?;
    let nanos =
        u32::try_from(filetime % TICKS_PER_SECOND).map_err(|_| STATUS_INVALID_PARAMETER)? * 100;
    Timestamp::new(seconds - FILETIME_UNIX_SECONDS, nanos).map_err(|_| STATUS_INVALID_PARAMETER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use carryfs_core::ImageDevice;
    use std::time::Instant;

    struct Image(std::path::PathBuf);

    impl Drop for Image {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn mount(tag: &str) -> (Image, CarryFs<ImageDevice>) {
        let path =
            std::env::temp_dir().join(format!("carryfs-windows-{tag}-{}.img", std::process::id()));
        let image = Image(path);
        let device = ImageDevice::create_new(&image.0, 256 << 20).unwrap();
        (
            image,
            CarryFs::new(Volume::format(device).unwrap(), false).unwrap(),
        )
    }

    fn w(path: &str) -> Vec<u16> {
        utf16(path)
    }

    #[test]
    fn windows_aliases_round_trip_original_names() {
        let names = [
            "what?",
            "what？",
            "a:b",
            "a\\b",
            "name.",
            "name ",
            "CON",
            "con.txt",
            "LPT³",
            "\u{E000}",
            "\u{E001}",
            "\u{F025}",
            "a\u{F029}b",
            "ＣON",
            "ＣON.txt",
            "Ｃat",
            "file*name",
            "é.",
            "a. b",
        ];
        // Names `push_alias` copies as they are must be ones `alias` leaves alone.
        for original in names.into_iter().chain([
            "plain",
            "a. b",
            "nul.d",
            "x y.txt",
            "-_~!#$%&'()+,;=@[]^`{}",
        ]) {
            let name = Name::parse(original).unwrap();
            let visible = alias(&name).unwrap();
            let mut units = Vec::new();
            push_alias(&mut units, &name).unwrap();
            assert_eq!(units, utf16(&visible), "{original:?}");
            assert_eq!(decode_alias(&visible).unwrap(), name, "{original:?}");
            assert!(!visible.contains(['<', '>', ':', '"', '\\', '|', '?', '*']));
            assert!(!matches!(visible.as_bytes().last(), Some(b' ' | b'.')));
            assert!(visible.encode_utf16().count() <= 255);
        }
        // The Services for Macintosh stand-ins.
        for (original, visible) in [
            ("what?", "what\u{F025}"),
            ("a:b", "a\u{F022}b"),
            (
                "\"*<>\\|",
                "\u{F020}\u{F021}\u{F023}\u{F024}\u{F026}\u{F027}",
            ),
            ("name.", "name\u{F029}"),
            ("name ", "name\u{F028}"),
            ("a. b", "a. b"),
            ("what？", "what？"),
            ("CON", "ＣON"),
            ("Ｃat", "Ｃat"),
            ("ＣON", "\u{E000}ＣON"),
        ] {
            assert_eq!(alias(&Name::parse(original).unwrap()).unwrap(), visible);
        }
        for raw in [
            "CON",
            "what?",
            "name.",
            "name ",
            "a\u{F029}b",
            "\u{E001}abc",
            "\u{E000}",
        ] {
            assert_eq!(
                decode_alias(raw),
                Err(STATUS_OBJECT_NAME_INVALID),
                "{raw:?}"
            );
        }
        // A decomposed spelling opens its NFC name, stand-ins included.
        for (decomposed, stored) in [("cafe\u{301}", "café"), ("e\u{301}\u{F025}", "é?")] {
            assert_eq!(
                decode_alias(decomposed).unwrap(),
                Name::parse(stored).unwrap()
            );
        }
        let long = Name::parse(&"a".repeat(255)).unwrap();
        assert_eq!(alias(&long).unwrap().encode_utf16().count(), 255);
        let long = Name::parse(&"?".repeat(255)).unwrap();
        assert_eq!(decode_alias(&alias(&long).unwrap()).unwrap(), long);
        let long = Name::parse(&format!("CON.{}", "a".repeat(251))).unwrap();
        assert_eq!(decode_alias(&alias(&long).unwrap()).unwrap(), long);
    }

    #[test]
    fn aliases_work_across_listing_lookup_rename_and_symlinks() {
        let (_image, fs) = mount("aliases");
        fs.with(|s| {
            for original in ["what?", "what？", "CON", "name."] {
                let handle = s
                    .volume
                    .create_file(ROOT, Name::parse(original).unwrap())
                    .unwrap();
                s.volume.close_handle(handle).unwrap();
            }
            s.volume
                .create_symlink(ROOT, Name::parse("link").unwrap(), b"what?/../CON")
                .unwrap();
            Ok(())
        })
        .unwrap();
        let names = list(&fs, "\\", 2, false);
        for original in ["what?", "what？", "CON", "name."] {
            let visible = alias(&Name::parse(original).unwrap()).unwrap();
            assert!(names.contains(&visible), "{original:?}");
            let (context, _, normalized) =
                fs.open(&w(&format!("\\{visible}")), false, false).unwrap();
            assert_eq!(
                String::from_utf16(&normalized).unwrap(),
                format!("\\{visible}")
            );
            fs.close(context);
        }
        let old = alias(&Name::parse("name.").unwrap()).unwrap();
        fs.rename(&w(&format!("\\{old}")), &w("\\renamed"), false)
            .unwrap();
        let (context, _, _) = fs.open(&w("\\renamed"), false, false).unwrap();
        fs.cleanup(context, Some(&w("\\renamed")), CLEANUP_DELETE);
        fs.close(context);
        assert_eq!(
            fs.open(&w("\\renamed"), false, false).map(|_| ()),
            Err(STATUS_OBJECT_NAME_NOT_FOUND)
        );

        let target = alias_target("what?/../CON").unwrap();
        assert_eq!(decode_target(&target).unwrap(), "what?/../CON");
        let data = fs.reparse_by_name(&w("\\link")).unwrap();
        assert_eq!(symlink_target(&data, 0).unwrap(), "what?/../CON");
    }

    #[test]
    #[ignore = "manual host adapter timing"]
    fn name_adapter_timing() {
        let (_image, fs) = mount("name-timing");
        for index in 0..100 {
            let (context, _, _) = fs
                .create(&w(&format!("\\file{index:03}")), false, false)
                .unwrap();
            fs.close(context);
        }
        for _ in 0..3 {
            let started = Instant::now();
            for _ in 0..1_000 {
                let (context, _, _) = fs.open(&w("\\file050"), false, false).unwrap();
                fs.close(context);
            }
            println!("lookup_1000_us={}", started.elapsed().as_micros());

            let (directory, _, _) = fs.open(&w("\\"), false, false).unwrap();
            let started = Instant::now();
            for _ in 0..100 {
                fs.read_directory(directory, None, |_, _| true).unwrap();
            }
            println!("listing_100_us={}", started.elapsed().as_micros());
            fs.close(directory);
        }
    }

    /// What one `os.stat` costs in callbacks (GetSecurityByName, Open, Close)
    /// and one listing in ReadDirectory calls of about 20 entries, the batch a
    /// 4 KiB `FindNextFile` buffer holds, over 10,000 files in one directory.
    #[test]
    #[ignore = "manual host adapter timing"]
    fn stat_and_listing_timing() {
        const FILES: usize = 10_000;
        let (_image, fs) = mount("stat-timing");
        let (directory, _, _) = fs.create(&w("\\d"), true, false).unwrap();
        fs.close(directory);
        let paths: Vec<Vec<u16>> = (0..FILES).map(|i| w(&format!("\\d\\f{i:05}"))).collect();
        for path in &paths {
            let (context, _, _) = fs.create(path, false, false).unwrap();
            fs.write(context, 0, &[7; 4096], false, false).unwrap();
            fs.close(context);
        }
        fs.flush(None).unwrap();
        for _ in 0..3 {
            let started = Instant::now();
            for path in &paths {
                fs.attributes_by_name(path).unwrap();
            }
            let by_name = started.elapsed();
            // In batches: a volume holds at most 4,096 handles.
            let (mut open, mut close) = (Duration::ZERO, Duration::ZERO);
            for batch in paths.chunks(1_000) {
                let started = Instant::now();
                let contexts: Vec<u64> = batch
                    .iter()
                    .map(|path| fs.open(path, false, false).unwrap().0)
                    .collect();
                open += started.elapsed();
                let started = Instant::now();
                for context in contexts {
                    fs.close(context);
                }
                close += started.elapsed();
            }
            let (directory, _, _) = fs.open(&w("\\d"), false, false).unwrap();
            let started = Instant::now();
            let (mut marker, mut calls): (Option<Vec<u16>>, usize) = (None, 0);
            loop {
                let (mut taken, mut last) = (0, None);
                let ended = fs
                    .read_directory(directory, marker.as_deref(), |name, _| {
                        if taken == 20 {
                            return false;
                        }
                        taken += 1;
                        last = Some(name.to_vec());
                        true
                    })
                    .unwrap();
                marker = last.or(marker);
                calls += 1;
                if ended {
                    break;
                }
            }
            let listing = started.elapsed();
            fs.close(directory);
            println!(
                "per call: by_name {} ns, open {} ns, close {} ns; listing {} us in {calls} calls",
                by_name.as_nanos() / FILES as u128,
                open.as_nanos() / FILES as u128,
                close.as_nanos() / FILES as u128,
                listing.as_micros(),
            );
        }
    }

    /// Enumerate like WinFsp: fixed-size batches, resuming after the last
    /// name returned, optionally deleting each listed entry before the next batch.
    fn list(fs: &CarryFs<ImageDevice>, path: &str, batch: usize, delete: bool) -> Vec<String> {
        let (directory, _, _) = fs.open(&w(path), false, false).unwrap();
        let mut names = Vec::new();
        let mut marker: Option<Vec<u16>> = None;
        loop {
            let mut batch_names = Vec::new();
            let ended = fs
                .read_directory(directory, marker.as_deref(), |name, _| {
                    if batch_names.len() == batch {
                        return false;
                    }
                    batch_names.push(String::from_utf16(name).unwrap());
                    true
                })
                .unwrap();
            marker = batch_names.last().map(|name| w(name));
            for name in &batch_names {
                if delete && !name.starts_with('.') {
                    let child = format!("{path}\\{name}");
                    let (context, _, _) = fs.open(&w(&child), false, false).unwrap();
                    fs.can_delete(context).unwrap();
                    fs.cleanup(context, Some(&w(&child)), CLEANUP_DELETE);
                    fs.close(context);
                }
            }
            names.extend(batch_names);
            if ended {
                break;
            }
        }
        fs.close(directory);
        names
    }

    #[test]
    fn paths_resolve_case_insensitively_and_report_the_missing_component() {
        let (_image, fs) = mount("paths");
        let (dir, info, _) = fs.create(&w("\\Docs"), true, false).unwrap();
        assert_eq!(info.file_attributes, FILE_ATTRIBUTE_DIRECTORY);
        let (file, _, _) = fs.create(&w("\\Docs\\Notes.txt"), false, false).unwrap();
        fs.close(file);
        fs.close(dir);
        let (again, info, _) = fs.open(&w("\\DOCS\\notes.TXT"), false, false).unwrap();
        assert_eq!(info.file_attributes, FILE_ATTRIBUTE_NORMAL);
        fs.close(again);

        let status = |path: &str| fs.open(&w(path), false, false).map(|_| ()).unwrap_err();
        assert_eq!(status("\\docs\\missing"), STATUS_OBJECT_NAME_NOT_FOUND);
        assert_eq!(status("\\missing\\file"), STATUS_OBJECT_PATH_NOT_FOUND);
        assert_eq!(
            status("\\docs\\notes.txt\\child"),
            STATUS_OBJECT_PATH_NOT_FOUND
        );
        assert_eq!(status("\\CON"), STATUS_OBJECT_NAME_INVALID);
        assert_eq!(
            fs.open(&[u16::from(b'\\'), 0xD800], false, false)
                .map(|_| ()),
            Err(STATUS_OBJECT_NAME_INVALID)
        );
        assert_eq!(
            fs.create(&w("\\docs\\NOTES.txt"), false, false).map(|_| ()),
            Err(STATUS_OBJECT_NAME_COLLISION)
        );
        assert_eq!(
            fs.create(&w("\\missing\\new"), false, false).map(|_| ()),
            Err(STATUS_OBJECT_PATH_NOT_FOUND)
        );
        assert_eq!(
            fs.create(&w("\\docs\\notes.txt\\new"), true, false)
                .map(|_| ()),
            Err(STATUS_OBJECT_PATH_NOT_FOUND)
        );
    }

    #[test]
    fn opening_through_an_alias_reports_the_stored_spelling() {
        let (_image, fs) = mount("normalized");
        let (dir, _, created) = fs.create(&w("\\Docs"), true, false).unwrap();
        assert_eq!(String::from_utf16(&created).unwrap(), "\\Docs");
        let (file, _, created) = fs.create(&w("\\docs\\Notes.TXT"), false, false).unwrap();
        // The parent keeps its stored spelling; a new entry keeps the requested one.
        assert_eq!(String::from_utf16(&created).unwrap(), "\\Docs\\Notes.TXT");
        fs.close(file);
        fs.close(dir);

        let (opened, _, normalized) = fs.open(&w("\\DOCS\\notes.txt"), false, false).unwrap();
        assert_eq!(
            String::from_utf16(&normalized).unwrap(),
            "\\Docs\\Notes.TXT"
        );
        fs.close(opened);
    }

    /// What `CreateSymbolicLinkW` sends: a relative link to `target`.
    fn symlink_buffer(target: &str, flags: u32) -> Vec<u8> {
        let path: Vec<u8> = target.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let bytes = path.len() as u16;
        let mut data = IO_REPARSE_TAG_SYMLINK.to_le_bytes().to_vec();
        data.extend_from_slice(&(12 + 2 * bytes).to_le_bytes());
        data.extend_from_slice(&[0, 0]);
        // Substitute name first, print name after it, as Windows lays them out.
        for field in [0, bytes, bytes, bytes] {
            data.extend_from_slice(&field.to_le_bytes());
        }
        data.extend_from_slice(&flags.to_le_bytes());
        data.extend_from_slice(&path);
        data.extend_from_slice(&path);
        data
    }

    #[test]
    fn set_reparse_point_turns_new_objects_into_symlinks() {
        let (_image, fs) = mount("reparse");
        let (dir, _, _) = fs.create(&w("\\dir"), true, false).unwrap();
        fs.close(dir);
        // mklink creates an empty file, then sets the reparse point on it.
        let (link, _, _) = fs.create(&w("\\file-link"), false, false).unwrap();
        fs.set_reparse(
            link,
            &w("\\file-link"),
            &symlink_buffer("dir\\..\\x.txt", 1),
            0,
        )
        .unwrap();
        let info = fs.file_info(link).unwrap();
        assert_eq!(info.file_attributes, FILE_ATTRIBUTE_REPARSE_POINT);
        assert_eq!(info.reparse_tag, IO_REPARSE_TAG_SYMLINK);
        let data = fs.reparse(link).unwrap();
        assert_eq!(symlink_target(&data, 0).unwrap(), "dir/../x.txt");
        assert_eq!(fs.reparse_by_name(&w("\\FILE-LINK")).unwrap(), data);
        fs.close(link);

        // mklink /D creates an empty directory; a target that is one reports it.
        let (link, _, _) = fs.create(&w("\\dir-link"), true, false).unwrap();
        fs.set_reparse(link, &w("\\dir-link"), &symlink_buffer("dir", 1), 0)
            .unwrap();
        assert_eq!(
            fs.file_info(link).unwrap().file_attributes,
            FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY
        );
        fs.close(link);
        assert_eq!(
            fs.reparse_by_name(&w("\\dir")),
            Err(STATUS_NOT_A_REPARSE_POINT)
        );

        let (other, _, _) = fs.create(&w("\\absolute"), false, false).unwrap();
        assert_eq!(
            fs.set_reparse(
                other,
                &w("\\absolute"),
                &symlink_buffer("\\??\\C:\\x", 0),
                0
            ),
            Err(STATUS_ACCESS_DENIED)
        );
        // An absolute target on this volume: WinFsp reports where its path starts.
        let absolute = symlink_buffer("\\??\\D:\\dir", 0);
        let on_volume = ("\\??\\D:".len() * 2) as u16;
        fs.set_reparse(other, &w("\\absolute"), &absolute, on_volume)
            .unwrap();
        assert_eq!(
            symlink_target(&fs.reparse(other).unwrap(), 0).unwrap(),
            "/dir"
        );
        // Made as a file link, it stays one although its target is a directory.
        assert_eq!(
            fs.file_info(other).unwrap().file_attributes,
            FILE_ATTRIBUTE_REPARSE_POINT
        );
        let mut junction = symlink_buffer("x", 1);
        junction[..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        assert_eq!(
            fs.set_reparse(other, &w("\\absolute"), &junction, 0),
            Err(STATUS_IO_REPARSE_TAG_MISMATCH)
        );
        fs.cleanup(other, Some(&w("\\absolute")), CLEANUP_DELETE);
        fs.close(other);

        // Delete-on-close removes a link, never its target.
        let (link, _, _) = fs.open(&w("\\dir-link"), false, false).unwrap();
        fs.cleanup(link, Some(&w("\\dir-link")), CLEANUP_DELETE);
        fs.close(link);
        assert!(fs.open(&w("\\dir-link"), false, false).is_err());
        assert!(fs.open(&w("\\dir"), false, false).is_ok());
    }

    #[test]
    fn links_made_elsewhere_take_their_kind_from_the_target() {
        let (_image, fs) = mount("inferred");
        let (dir, _, _) = fs.create(&w("\\dir"), true, false).unwrap();
        fs.close(dir);
        fs.with(|s| {
            for (link, target) in [("up", "dir/../dir"), ("root", "/dir"), ("gone", "missing")] {
                let name = Name::parse(link).unwrap();
                s.volume
                    .create_symlink(ROOT, name, target.as_bytes())
                    .unwrap();
            }
            Ok(())
        })
        .unwrap();
        let directory = FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY;
        for (link, attributes) in [
            ("\\up", directory),
            ("\\root", directory),
            ("\\gone", FILE_ATTRIBUTE_REPARSE_POINT),
        ] {
            assert_eq!(fs.attributes_by_name(&w(link)), Ok(attributes), "{link}");
        }
    }

    #[test]
    fn listing_resumes_by_marker_while_entries_are_deleted() {
        let (_image, fs) = mount("listing");
        let (dir, _, _) = fs.create(&w("\\d"), true, false).unwrap();
        fs.close(dir);
        let mut expected = vec![".".to_string(), "..".to_string()];
        for index in 0..150 {
            let name = format!("f{index:03}");
            let (file, _, _) = fs
                .create(&w(&format!("\\d\\{name}")), false, false)
                .unwrap();
            fs.close(file);
            expected.push(name);
        }
        assert_eq!(list(&fs, "\\d", 7, false), expected);
        assert_eq!(list(&fs, "\\d", 100, true), expected);
        assert_eq!(list(&fs, "\\d", 1, false), [".", ".."]);
        assert_eq!(list(&fs, "\\", 5, false), ["d"]);
    }

    fn make(fs: &CarryFs<ImageDevice>, path: &str, directory: bool) {
        let (context, _, _) = fs.create(&w(path), directory, false).unwrap();
        fs.close(context);
    }

    fn delete(fs: &CarryFs<ImageDevice>, path: &str) {
        let (context, _, _) = fs.open(&w(path), false, false).unwrap();
        fs.cleanup(context, Some(&w(path)), CLEANUP_DELETE);
        fs.close(context);
    }

    #[test]
    fn a_continued_listing_is_dropped_when_anything_changes() {
        let (_image, fs) = mount("listing-continued");
        make(&fs, "\\d", true);
        for index in 0..150 {
            make(&fs, &format!("\\d\\f{index:03}"), false);
        }
        let (directory, _, _) = fs.open(&w("\\d"), false, false).unwrap();
        let take = |marker: Option<&str>, count: usize| {
            let mut names = Vec::new();
            fs.read_directory(directory, marker.map(w).as_deref(), |name, _| {
                if names.len() == count {
                    return false;
                }
                names.push(String::from_utf16(name).unwrap());
                true
            })
            .unwrap();
            names
        };
        assert_eq!(take(None, 10).last().unwrap(), "f007");
        // The first call stopped inside a page. A name added and one removed
        // in the part it had read but not returned both show in the next call.
        make(&fs, "\\d\\f008a", false);
        delete(&fs, "\\d\\f009");
        assert_eq!(
            take(Some("f007"), 5),
            ["f008", "f008a", "f010", "f011", "f012"]
        );
        // With nothing changed, the continued page and a seek by an older
        // marker return the same entries.
        let continued = take(Some("f012"), 3);
        assert_eq!(continued, ["f013", "f014", "f015"]);
        assert_eq!(take(Some("f012"), 3), continued);
        assert_eq!(take(Some("f015"), 200).len(), 134);
        fs.close(directory);
    }

    #[test]
    fn remembered_attributes_follow_every_change() {
        let (_image, fs) = mount("known-attributes");
        make(&fs, "\\d", true);
        let path = w("\\d\\file");
        let (writer, _, _) = fs.create(&path, false, false).unwrap();
        let (reader, _, _) = fs.open(&path, false, false).unwrap();
        // The size by name, through another handle, and in a listing.
        let sizes = || {
            let mut listed = None;
            let (directory, _, _) = fs.open(&w("\\d"), false, false).unwrap();
            fs.read_directory(directory, None, |name, info| {
                if name == w("file").as_slice() {
                    listed = Some(info.file_size);
                }
                true
            })
            .unwrap();
            fs.close(directory);
            fs.attributes_by_name(&path).unwrap();
            (fs.file_info(reader).unwrap().file_size, listed.unwrap())
        };
        assert_eq!(sizes(), (0, 0));
        fs.write(writer, 0, &[1; 5000], false, false).unwrap();
        assert_eq!(sizes(), (5000, 5000));
        fs.set_file_size(writer, 100, false).unwrap();
        assert_eq!(sizes(), (100, 100));
        fs.overwrite(writer).unwrap();
        assert_eq!(sizes(), (0, 0));
        let stamp = 133_000_000_000_000_000;
        assert_ne!(fs.file_info(reader).unwrap().last_write_time, stamp);
        fs.set_basic_info(writer, 0, stamp).unwrap();
        assert_eq!(fs.file_info(reader).unwrap().last_write_time, stamp);
        // Publication changes no attribute.
        fs.flush(None).unwrap();
        assert_eq!(sizes(), (0, 0));
        assert_eq!(fs.file_info(reader).unwrap().last_write_time, stamp);
        fs.close(writer);
        fs.close(reader);
    }

    #[test]
    fn cleanup_by_the_timer_changes_no_remembered_attribute() {
        let (_image, fs) = mount("known-cleanup");
        make(&fs, "\\d", true);
        for name in ["keep", "shrink", "gone"] {
            let path = w(&format!("\\d\\{name}"));
            let (file, _, _) = fs.create(&path, false, false).unwrap();
            fs.write(file, 0, &vec![7; 1 << 20], false, false).unwrap();
            fs.close(file);
        }
        let (shrink, _, _) = fs.open(&w("\\d\\shrink"), true, false).unwrap();
        fs.set_file_size(shrink, 4096, false).unwrap();
        delete(&fs, "\\d\\gone");
        // Remember what a listing and a handle report, then let the timer
        // publish, reclaim the truncated blocks, and remove the orphan.
        assert_eq!(list(&fs, "\\d", 100, false).len(), 4);
        fs.file_info(shrink).unwrap();
        let mut now = Instant::now();
        for _ in 0..100 {
            now += PACING.max_age;
            fs.tick(now, PACING);
            if fs.state.lock().unwrap().quiet.is_some() {
                break;
            }
        }
        assert!(!unsynced(&fs));
        let mut state = fs.state.lock().unwrap();
        let State { known, volume, .. } = &mut *state;
        let mut compared = 0;
        for (object, remembered) in &known.attributes {
            // The removed file's object is gone and can no longer be asked for.
            if let Ok(current) = volume.attributes(ObjectId::new(*object).unwrap()) {
                assert_eq!(current, *remembered);
                compared += 1;
            }
        }
        assert!(compared >= 4, "{compared} remembered objects compared");
        drop(state);
        fs.close(shrink);
    }

    #[test]
    fn a_remembered_directory_is_forgotten_when_its_path_changes() {
        let (_image, fs) = mount("known-directory");
        for path in ["\\a", "\\a\\b"] {
            make(&fs, path, true);
        }
        make(&fs, "\\a\\b\\one", false);
        assert!(fs.attributes_by_name(&w("\\a\\b\\one")).is_ok());
        // Renaming a directory above it moves every path through it.
        fs.rename(&w("\\a"), &w("\\z"), false).unwrap();
        assert_eq!(
            fs.attributes_by_name(&w("\\a\\b\\one")),
            Err(STATUS_OBJECT_PATH_NOT_FOUND)
        );
        assert!(fs.attributes_by_name(&w("\\z\\b\\one")).is_ok());
        // A directory removed and made again under its path is another object.
        delete(&fs, "\\z\\b\\one");
        assert!(fs.attributes_by_name(&w("\\z\\b\\one")).is_err());
        delete(&fs, "\\z\\b");
        assert_eq!(
            fs.attributes_by_name(&w("\\z\\b\\one")),
            Err(STATUS_OBJECT_PATH_NOT_FOUND)
        );
        make(&fs, "\\z\\b", true);
        make(&fs, "\\z\\b\\two", false);
        assert_eq!(
            fs.attributes_by_name(&w("\\z\\b\\one")),
            Err(STATUS_OBJECT_NAME_NOT_FOUND)
        );
        let (two, _, normalized) = fs.open(&w("\\Z\\B\\TWO"), false, false).unwrap();
        assert_eq!(normalized, w("\\z\\b\\two"));
        fs.close(two);
        // An empty directory that becomes a link holds no paths any more.
        let (link, _, _) = fs.create(&w("\\z\\link"), true, false).unwrap();
        assert_eq!(
            fs.attributes_by_name(&w("\\z\\link\\x")),
            Err(STATUS_OBJECT_NAME_NOT_FOUND)
        );
        fs.set_reparse(link, &w("\\z\\link"), &symlink_buffer("b", 1), 0)
            .unwrap();
        fs.close(link);
        assert_eq!(
            fs.attributes_by_name(&w("\\z\\link\\x")),
            Err(STATUS_OBJECT_PATH_NOT_FOUND)
        );
    }

    #[test]
    fn delete_pending_file_stays_readable_until_its_last_close() {
        let (_image, fs) = mount("delete");
        let path = w("\\victim");
        let (writer, _, _) = fs.create(&path, false, false).unwrap();
        let (written, info) = fs.write(writer, 0, &[5; 100_000], false, false).unwrap();
        assert_eq!((written, info.file_size), (100_000, 100_000));
        let (reader, _, _) = fs.open(&path, false, false).unwrap();
        fs.can_delete(writer).unwrap();
        fs.cleanup(writer, Some(&path), CLEANUP_DELETE);
        fs.close(writer);
        assert_eq!(
            fs.open(&path, false, false).map(|_| ()),
            Err(STATUS_OBJECT_NAME_NOT_FOUND)
        );

        let mut buffer = vec![0; 100_000];
        assert_eq!(fs.read(reader, 0, &mut buffer), Ok(100_000));
        assert!(buffer.iter().all(|&byte| byte == 5));
        assert_eq!(
            fs.read(reader, 100_000, &mut buffer),
            Err(STATUS_END_OF_FILE)
        );
        let before = fs.volume_size().unwrap().1;
        fs.close(reader);
        for _ in 0..10 {
            fs.flush(None).unwrap();
        }
        assert!(fs.volume_size().unwrap().1 > before);
    }

    fn handles<D: BlockDevice>(fs: &CarryFs<D>) -> (usize, usize) {
        let state = fs.state.lock().unwrap();
        (state.open.len(), state.cleaned.len())
    }

    /// Windows keeps a cached file open long after its handle closed, and
    /// reads the pages it lacks through that context.
    #[test]
    fn a_context_gives_up_its_handle_at_cleanup_and_still_reads() {
        let (_image, fs) = mount("cleaned");
        let path = w("\\cached");
        let (writer, _, _) = fs.create(&path, false, false).unwrap();
        fs.write(writer, 0, &[9; 70_000], false, false).unwrap();
        fs.cleanup(writer, None, 0);
        fs.close(writer);
        assert_eq!(handles(&fs), (0, 0));

        // More contexts than the volume has handles.
        let contexts: Vec<u64> = (0..5_000)
            .map(|_| {
                let (context, _, _) = fs.open(&path, false, false).unwrap();
                fs.cleanup(context, None, 0);
                context
            })
            .collect();
        assert_eq!(handles(&fs), (0, 5_000));
        let mut buffer = vec![0; 70_000];
        for &context in [&contexts[0], &contexts[4_999]] {
            buffer.fill(0);
            assert_eq!(fs.read(context, 0, &mut buffer), Ok(70_000));
            assert!(buffer.iter().all(|&byte| byte == 9));
            assert_eq!(
                fs.read(context, 70_000, &mut buffer),
                Err(STATUS_END_OF_FILE)
            );
        }
        for context in contexts {
            fs.close(context);
        }
        assert_eq!(handles(&fs), (0, 0));
        assert_eq!(fs.read(writer, 0, &mut buffer), Err(STATUS_INVALID_HANDLE));
        fs.into_volume().unwrap().eject().unwrap();
    }

    /// A mapped view's pages are written through the context that first
    /// cached or mapped the file, whatever access that one was opened with.
    #[test]
    fn paging_writes_arrive_through_read_and_cleaned_up_contexts() {
        let (image, fs) = mount("paging");
        let path = w("\\mapped");
        let (writer, _, _) = fs.create(&path, false, false).unwrap();
        fs.write(writer, 0, &[0; 8192], false, false).unwrap();
        let (reader, _, _) = fs.open(&path, false, false).unwrap();
        assert_eq!(fs.write(reader, 0, b"one", false, true).unwrap().0, 3);
        fs.cleanup(reader, None, 0);
        let (written, info) = fs.write(reader, 4096, b"two", false, true).unwrap();
        assert_eq!((written, info.file_size), (3, 8192));
        // Paging I/O never extends a file.
        assert_eq!(fs.write(reader, 8190, b"tail", false, true).unwrap().0, 2);
        fs.close(reader);

        let mut buffer = [0; 8192];
        assert_eq!(fs.read(writer, 0, &mut buffer), Ok(8192));
        assert_eq!(&buffer[..3], b"one");
        assert_eq!(&buffer[4096..4099], b"two");
        assert_eq!(&buffer[8190..], b"ta");
        fs.cleanup(writer, None, 0);
        fs.close(writer);
        assert_eq!(handles(&fs), (0, 0));

        // A read-only mount has nothing to write them to.
        fs.into_volume().unwrap().eject().unwrap();
        let device = ImageDevice::open(&image.0, carryfs_core::MountMode::ReadOnly).unwrap();
        let fs = CarryFs::new(Volume::open(device).unwrap(), true).unwrap();
        let (reader, _, _) = fs.open(&path, false, false).unwrap();
        fs.cleanup(reader, None, 0);
        assert_eq!(
            fs.write(reader, 0, b"x", false, true).map(|_| ()),
            Err(STATUS_MEDIA_WRITE_PROTECTED)
        );
        fs.close(reader);
    }

    /// Closing a file that was open with write access names it, so the caller
    /// can have Windows write out pages a mapped view left; until those had
    /// time to arrive the volume is not ready for removal.
    #[test]
    fn closing_a_written_file_names_it_and_holds_removal() {
        let (_image, fs) = mount("writeout");
        let (created, _, _) = fs.create(&w("\\New"), false, false).unwrap();
        assert!(fs.take_stale().is_empty());
        fs.cleanup(created, None, 0);
        assert_eq!(fs.take_stale(), [w("\\New")]);
        fs.close(created);
        fs.flush(None).unwrap();
        assert_eq!(fs.release_for_removal(|| true), Removal::Unsynced);

        // Reading it names nothing; neither does deleting it.
        let (reader, _, _) = fs.open(&w("\\new"), false, false).unwrap();
        fs.cleanup(reader, None, 0);
        fs.close(reader);
        // An open file follows a rename made through another handle.
        let (writer, _, _) = fs.open(&w("\\new"), true, false).unwrap();
        let (second, _, _) = fs.open(&w("\\NEW"), true, false).unwrap();
        fs.rename(&w("\\New"), &w("\\Moved"), false).unwrap();
        assert!(fs.take_stale().is_empty());
        fs.cleanup(second, None, 0);
        assert_eq!(fs.take_stale(), [w("\\Moved")]);
        fs.close(second);
        fs.can_delete(writer).unwrap();
        fs.cleanup(writer, Some(&w("\\Moved")), CLEANUP_DELETE);
        fs.close(writer);
        let (directory, _, _) = fs.create(&w("\\dir"), true, false).unwrap();
        fs.cleanup(directory, None, 0);
        fs.close(directory);
        assert!(fs.take_stale().is_empty());

        fs.flush(None).unwrap();
        // A slow host can spend more than `max_age` in the steps above.
        fs.state.lock().unwrap().settling = Some(Instant::now());
        assert_eq!(fs.release_for_removal(|| true), Removal::Unsynced);
        let earlier = Instant::now().checked_sub(PACING.max_age).unwrap();
        fs.state.lock().unwrap().settling = Some(earlier);
        assert_eq!(fs.release_for_removal(|| true), Removal::Released(true));
    }

    /// Windows keeps the data of a file that was read, and its context,
    /// after the handle closes. Past `KEPT_FILES` the one closed longest ago
    /// is named for Windows to drop, unless Windows already has.
    #[test]
    fn only_the_files_last_read_stay_with_windows() {
        let (_image, fs) = mount("kept");
        let read = |name: &str| {
            let path = w(name);
            if let Ok((created, _, _)) = fs.create(&path, false, false) {
                fs.write(created, 0, b"data", false, false).unwrap();
                fs.close(created);
            }
            let (context, _, _) = fs.open(&path, false, false).unwrap();
            assert_eq!(fs.read(context, 0, &mut [0; 4]), Ok(4));
            fs.cleanup(context, None, 0);
            context
        };
        let first = read("\\first");
        let second = read("\\second");
        // Opened and closed without a read: Windows has nothing of it.
        let (unread, _, _) = fs.open(&w("\\first"), false, false).unwrap();
        fs.cleanup(unread, None, 0);
        fs.close(unread);
        // Windows dropped the first on its own.
        fs.close(first);
        fs.rename(&w("\\second"), &w("\\renamed"), false).unwrap();
        for _ in 2..KEPT_FILES {
            read("\\filler");
        }
        assert!(fs.take_stale().is_empty());
        read("\\filler");
        assert!(fs.take_stale().is_empty(), "the first was closed already");
        read("\\filler");
        assert_eq!(fs.take_stale(), [w("\\renamed")]);
        fs.close(second);
        assert_eq!(handles(&fs), (0, KEPT_FILES));
    }

    /// Windows reads an open directory's times once, so each change to its
    /// entries names it. A directory nobody has open is read afresh anyway.
    #[test]
    fn a_change_in_an_open_directory_names_the_directory() {
        let (_image, fs) = mount("touched");
        let (held, _, _) = fs.create(&w("\\Held"), true, false).unwrap();
        let (other, _, _) = fs.create(&w("\\other"), true, false).unwrap();
        fs.cleanup(other, None, 0);
        fs.close(other);
        assert!(fs.take_stale().is_empty());

        let (file, _, _) = fs.create(&w("\\held\\a"), false, false).unwrap();
        assert_eq!(fs.take_stale(), [w("\\held")]);
        fs.rename(&w("\\held\\a"), &w("\\held\\b"), false).unwrap();
        assert_eq!(fs.take_stale(), [w("\\held")]);
        fs.rename(&w("\\held\\b"), &w("\\other\\b"), false).unwrap();
        assert_eq!(fs.take_stale(), [w("\\held")]);
        fs.rename(&w("\\other\\b"), &w("\\HELD\\c"), false).unwrap();
        assert_eq!(fs.take_stale(), [w("\\HELD")]);
        fs.can_delete(file).unwrap();
        fs.cleanup(file, Some(&w("\\Held\\c")), CLEANUP_DELETE);
        assert_eq!(fs.take_stale(), [w("\\Held")]);
        fs.close(file);

        // The root is a directory like any other.
        let (root, _, _) = fs.open(&w("\\"), false, false).unwrap();
        let (sub, _, _) = fs.create(&w("\\sub"), true, false).unwrap();
        assert_eq!(fs.take_stale(), [w("\\")]);
        fs.rename(&w("\\sub"), &w("\\held\\sub"), false).unwrap();
        assert_eq!(fs.take_stale(), [w("\\"), w("\\held")]);
        for context in [sub, root, held] {
            fs.cleanup(context, None, 0);
            fs.close(context);
        }
        let (unheld, _, _) = fs.create(&w("\\held\\d"), false, false).unwrap();
        assert!(fs.take_stale().is_empty());
        fs.close(unheld);
    }

    /// With every handle of the volume taken, a cleaned-up context is served
    /// by a handle that is open on the same file.
    #[test]
    fn a_cleaned_up_context_reads_while_every_handle_is_taken() {
        let (_image, fs) = mount("saturated");
        let path = w("\\busy");
        let (writer, _, _) = fs.create(&path, false, false).unwrap();
        fs.write(writer, 0, b"data", false, false).unwrap();
        let (first, _, _) = fs.open(&path, false, false).unwrap();
        fs.cleanup(first, None, 0);
        let (other, _, _) = fs.create(&w("\\other"), false, false).unwrap();
        fs.cleanup(other, None, 0);
        let mut readers = Vec::new();
        loop {
            match fs.open(&path, false, false) {
                Ok((reader, _, _)) => readers.push(reader),
                Err(code) => break assert_eq!(code, STATUS_TOO_MANY_OPENED_FILES),
            }
        }
        assert_eq!(readers.len(), 4_095);

        let mut buffer = [0; 4];
        assert_eq!(fs.read(first, 0, &mut buffer), Ok(4));
        assert_eq!(&buffer, b"data");
        assert_eq!(fs.write(first, 0, b"D", false, true).unwrap().0, 1);
        // No handle is open on the other file.
        assert_eq!(
            fs.read(other, 0, &mut buffer),
            Err(STATUS_TOO_MANY_OPENED_FILES)
        );
        // One close admits the request again.
        fs.close(readers.pop().unwrap());
        assert_eq!(fs.read(other, 0, &mut buffer), Err(STATUS_END_OF_FILE));
    }

    #[test]
    fn writes_append_and_constrained_writes_do_not_extend() {
        let (_image, fs) = mount("write");
        let (file, _, _) = fs.create(&w("\\f"), false, false).unwrap();
        fs.write(file, 0, b"hello", false, false).unwrap();
        let (_, info) = fs.write(file, 0, b" world", true, false).unwrap();
        assert_eq!(info.file_size, 11);
        let (written, info) = fs.write(file, 8, b"XXXXXX", false, true).unwrap();
        assert_eq!((written, info.file_size), (3, 11));
        let mut buffer = [0; 11];
        fs.read(file, 0, &mut buffer).unwrap();
        assert_eq!(&buffer, b"hello woXXX");
        assert_eq!(fs.set_file_size(file, 4096, true).unwrap().file_size, 11);
        assert_eq!(fs.set_file_size(file, 2, true).unwrap().file_size, 2);
        assert_eq!(fs.overwrite(file).unwrap().file_size, 0);
        fs.close(file);
    }

    #[test]
    fn rename_never_replaces_a_directory_and_reports_collisions() {
        let (_image, fs) = mount("rename");
        for (path, directory) in [("\\a", false), ("\\b", false), ("\\dir", true)] {
            let (context, _, _) = fs.create(&w(path), directory, false).unwrap();
            fs.close(context);
        }
        assert_eq!(
            fs.rename(&w("\\a"), &w("\\B"), false),
            Err(STATUS_OBJECT_NAME_COLLISION)
        );
        assert_eq!(
            fs.rename(&w("\\a"), &w("\\dir"), true),
            Err(STATUS_ACCESS_DENIED)
        );
        fs.rename(&w("\\a"), &w("\\b"), true).unwrap();
        fs.rename(&w("\\dir"), &w("\\DIR"), false).unwrap();
        assert_eq!(list(&fs, "\\", 10, false), ["b", "DIR"]);
    }

    #[test]
    fn a_read_only_mount_refuses_deletion() {
        let (image, volume) = {
            let (image, fs) = mount("readonly-setup");
            let (file, _, _) = fs.create(&w("\\keep.txt"), false, false).unwrap();
            fs.close(file);
            (image, fs.into_volume().unwrap())
        };
        let fs = CarryFs::new(volume, true).unwrap();
        let (context, _, _) = fs.open(&w("\\keep.txt"), false, false).unwrap();
        assert_eq!(fs.can_delete(context), Err(STATUS_MEDIA_WRITE_PROTECTED));
        fs.close(context);
        drop(image);
    }

    #[test]
    fn non_empty_directories_cannot_be_deleted() {
        let (_image, fs) = mount("candelete");
        let (dir, _, _) = fs.create(&w("\\dir"), true, false).unwrap();
        let (file, _, _) = fs.create(&w("\\dir\\f"), false, false).unwrap();
        fs.close(file);
        assert_eq!(fs.can_delete(dir), Err(STATUS_DIRECTORY_NOT_EMPTY));
        fs.close(dir);
    }

    fn unsynced<D: BlockDevice>(fs: &CarryFs<D>) -> bool {
        fs.state.lock().unwrap().volume.has_unsynced_changes()
    }

    fn last_operation<D: BlockDevice>(fs: &CarryFs<D>) -> Instant {
        fs.state.lock().unwrap().last_operation
    }

    #[test]
    fn changes_wait_for_a_pause_the_age_cap_or_a_flush() {
        let (_image, fs) = mount("pacing");
        let (file, _, _) = fs.create(&w("\\f"), false, false).unwrap();
        fs.write(file, 0, b"hello", false, false).unwrap();
        assert!(unsynced(&fs));

        // Not yet idle for `PACING.idle`, and the change is new.
        let start = last_operation(&fs);
        fs.tick(start, PACING);
        assert!(unsynced(&fs));
        // A pause publishes.
        fs.tick(start + PACING.idle, PACING);
        assert!(!unsynced(&fs));

        // Under steady load only the age cap publishes: requests keep arriving,
        // so the timer never sees a pause.
        fs.write(file, 5, b" again", false, false).unwrap();
        let first = Instant::now();
        fs.tick(first, PACING);
        let busy = Pacing {
            idle: Duration::from_secs(3600),
            max_age: PACING.max_age,
        };
        fs.tick(first + PACING.max_age / 2, busy);
        assert!(unsynced(&fs));
        fs.tick(first + PACING.max_age, busy);
        assert!(!unsynced(&fs));

        // FlushFileBuffers on the file publishes at once.
        fs.write(file, 0, b"H", false, false).unwrap();
        assert!(unsynced(&fs));
        fs.flush(Some(file)).unwrap();
        assert!(!unsynced(&fs));
        fs.close(file);
    }

    /// A flush publishes what the timer was counting the age of. Its age cap
    /// must start again with the next change, or it checkpoints in the
    /// middle of the burst that follows.
    #[test]
    fn a_flush_restarts_the_age_of_pending_changes() {
        let (_image, fs) = mount("age");
        let busy = Pacing {
            idle: Duration::from_secs(3600),
            max_age: PACING.max_age,
        };
        let (file, _, _) = fs.create(&w("\\f"), false, false).unwrap();
        fs.write(file, 0, b"first", false, false).unwrap();
        let first = Instant::now();
        fs.tick(first, busy);
        assert!(unsynced(&fs));
        fs.flush(Some(file)).unwrap();
        assert!(!unsynced(&fs));

        // Past the first change's cap, the new change is still young.
        fs.write(file, 5, b" second", false, false).unwrap();
        fs.tick(first + PACING.max_age, busy);
        assert!(unsynced(&fs));
        fs.tick(first + PACING.max_age * 2, busy);
        assert!(!unsynced(&fs));
        fs.close(file);
    }

    #[test]
    fn write_through_handles_publish_before_replying() {
        let (_image, fs) = mount("writethrough");
        let (file, _, _) = fs.create(&w("\\f"), false, true).unwrap();
        assert!(!unsynced(&fs));
        fs.write(file, 0, b"durable", false, false).unwrap();
        assert!(!unsynced(&fs));
        fs.set_file_size(file, 3, false).unwrap();
        assert!(!unsynced(&fs));
        fs.close(file);

        // An ordinary handle to the same file defers.
        let (plain, _, _) = fs.open(&w("\\f"), true, false).unwrap();
        fs.write(plain, 0, b"x", false, false).unwrap();
        assert!(unsynced(&fs));
        fs.close(plain);
    }

    #[test]
    fn removal_is_vetoed_until_changes_are_published_and_requests_finish() {
        let (_image, fs) = mount("removal");
        let (file, _, _) = fs.create(&w("\\f"), false, false).unwrap();
        fs.write(file, 0, b"pending", false, false).unwrap();
        fs.close(file);

        let mut released = false;
        assert_eq!(
            fs.release_for_removal(|| {
                released = true;
                true
            }),
            Removal::Unsynced
        );
        assert!(!released, "the handle must stay open to publish");

        fs.flush(None).unwrap();
        {
            let _request = fs.state.lock().unwrap();
            assert_eq!(fs.release_for_removal(|| true), Removal::Busy);
        }
        assert_eq!(fs.release_for_removal(|| true), Removal::Released(true));
        // After release the timer leaves the device alone, even with work due.
        fs.state.lock().unwrap().quiet = None;
        let generation = fs.state.lock().unwrap().volume.info().unwrap().generation;
        fs.tick(Instant::now() + PACING.max_age, PACING);
        let after = fs.state.lock().unwrap().volume.info().unwrap().generation;
        assert_eq!(generation, after);
    }

    /// An image whose writes or flushes start failing when `broken` is set.
    struct Breakable {
        inner: ImageDevice,
        broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
        writes: bool,
    }

    impl Breakable {
        fn check(&self, writes: bool) -> carryfs_core::Result<()> {
            if self.writes == writes && self.broken.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::Io {
                    operation: "test device",
                    source: std::io::Error::other("injected"),
                });
            }
            Ok(())
        }
    }

    impl BlockDevice for Breakable {
        fn mode(&self) -> carryfs_core::MountMode {
            self.inner.mode()
        }

        fn geometry(&mut self) -> carryfs_core::Result<carryfs_core::Geometry> {
            self.inner.geometry()
        }

        fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> carryfs_core::Result<()> {
            self.inner.read_exact_at(offset, out)
        }

        fn write_all_at(&mut self, offset: u64, data: &[u8]) -> carryfs_core::Result<()> {
            self.check(true)?;
            self.inner.write_all_at(offset, data)
        }

        fn flush_durable(&mut self) -> carryfs_core::Result<()> {
            self.check(false)?;
            self.inner.flush_durable()
        }
    }

    #[test]
    fn a_faulted_volume_is_released_instead_of_vetoed_forever() {
        for writes in [true, false] {
            let path = std::env::temp_dir().join(format!(
                "carryfs-windows-lost-{writes}-{}.img",
                std::process::id()
            ));
            let _image = Image(path.clone());
            let broken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let device = Breakable {
                inner: ImageDevice::create_new(&path, 256 << 20).unwrap(),
                broken: broken.clone(),
                writes,
            };
            let fs = CarryFs::new(Volume::format(device).unwrap(), false).unwrap();
            let (file, _, _) = fs.create(&w("\\f"), false, false).unwrap();
            fs.write(file, 0, b"pending", false, false).unwrap();
            fs.close(file);
            assert!(unsynced(&fs));

            // Healthy and unpublished: vetoed, so the changes can be published.
            assert_eq!(fs.release_for_removal(|| true), Removal::Unsynced);
            broken.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(fs.flush(None).is_err(), "writes failing: {writes}");

            // Retrying cannot help, so the next removal goes through and reports the loss.
            let mut released = false;
            let removal = fs.release_for_removal(|| {
                released = true;
                true
            });
            assert_eq!(removal, Removal::Lost(true), "writes failing: {writes}");
            assert!(released);
        }
    }

    #[test]
    fn deleted_data_is_reclaimed_by_the_timer() {
        let (_image, fs) = mount("reclaim");
        let free = |fs: &CarryFs<ImageDevice>| {
            fs.state.lock().unwrap().volume.space().unwrap().free_blocks
        };
        fs.flush(None).unwrap();
        let empty = free(&fs);

        let (file, _, _) = fs.create(&w("\\big"), false, false).unwrap();
        fs.write(file, 0, &vec![7; 1 << 20], false, false).unwrap();
        fs.close(file);
        fs.flush(None).unwrap();
        assert!(free(&fs) < empty);

        let (file, _, _) = fs.open(&w("\\big"), false, false).unwrap();
        fs.can_delete(file).unwrap();
        fs.cleanup(file, Some(&w("\\big")), CLEANUP_DELETE);
        fs.close(file);
        // The delete itself does no cleanup, so it replies without waiting.
        assert!(unsynced(&fs));

        let mut now = Instant::now();
        for _ in 0..100 {
            now += PACING.max_age;
            fs.tick(now, PACING);
            if fs.state.lock().unwrap().quiet.is_some() {
                break;
            }
        }
        assert!(!unsynced(&fs));
        assert_eq!(free(&fs), empty);
    }

    #[test]
    fn filetimes_round_trip_across_both_epochs() {
        for (seconds, nanos) in [
            (0, 0),
            (1_789_000_000, 500),
            (-1, 0),
            (-FILETIME_UNIX_SECONDS, 100),
        ] {
            let stored = Timestamp::new(seconds, nanos).unwrap();
            assert_eq!(timestamp(filetime(stored)).unwrap(), stored);
        }
        // 1970-01-01 is 116444736000000000 ticks after 1601-01-01.
        assert_eq!(
            filetime(Timestamp::new(0, 0).unwrap()),
            116_444_736_000_000_000
        );
        assert_eq!(
            filetime(Timestamp::new(0, 199).unwrap()),
            116_444_736_000_000_001
        );
    }

    #[test]
    fn status_mapping_distinguishes_request_and_damage_errors() {
        assert_eq!(status(&Error::NotFound), STATUS_OBJECT_NAME_NOT_FOUND);
        assert_eq!(status(&Error::NoSpace), STATUS_DISK_FULL);
        assert_eq!(status(&Error::ReadOnly), STATUS_MEDIA_WRITE_PROTECTED);
        assert_eq!(
            status(&Name::parse(&"x".repeat(256)).unwrap_err()),
            STATUS_OBJECT_NAME_INVALID
        );
        assert_eq!(status(&Error::Corrupt("x")), STATUS_FILE_CORRUPT_ERROR);
        assert_eq!(
            open_status(&Error::LimitExceeded),
            STATUS_TOO_MANY_OPENED_FILES
        );
        assert_eq!(open_status(&Error::WrongType), STATUS_FILE_IS_A_DIRECTORY);
    }

    /// One handle that will not close does not keep the others open. A handle
    /// of another volume stands in for it; `eject` then requires that every
    /// handle of this volume was closed.
    #[test]
    fn close_all_closes_every_handle_after_one_fails() {
        let (_image, fs) = mount("close-all");
        let (_other_image, other) = mount("close-all-other");
        for index in 0..32 {
            fs.create(&w(&format!("\\f{index}")), false, false).unwrap();
        }
        let (context, _, _) = other.create(&w("\\foreign"), false, false).unwrap();
        let foreign = other.state.lock().unwrap().open.remove(&context).unwrap();
        fs.state.lock().unwrap().open.insert(u64::MAX, foreign);
        assert_eq!(fs.close_all(), Err(STATUS_INVALID_HANDLE));
        fs.into_volume().unwrap().eject().unwrap();
    }

    /// An image whose reads fail once `limit` of them have been made.
    struct Flaky {
        inner: ImageDevice,
        reads: std::sync::Arc<std::sync::atomic::AtomicU64>,
        limit: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl BlockDevice for Flaky {
        fn mode(&self) -> carryfs_core::MountMode {
            self.inner.mode()
        }

        fn geometry(&mut self) -> carryfs_core::Result<carryfs_core::Geometry> {
            self.inner.geometry()
        }

        fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> carryfs_core::Result<()> {
            use std::sync::atomic::Ordering::SeqCst;
            if self.reads.fetch_add(1, SeqCst) >= self.limit.load(SeqCst) {
                return Err(Error::Io {
                    operation: "test device",
                    source: std::io::Error::other("injected"),
                });
            }
            self.inner.read_exact_at(offset, out)
        }

        fn write_all_at(&mut self, offset: u64, data: &[u8]) -> carryfs_core::Result<()> {
            self.inner.write_all_at(offset, data)
        }

        fn flush_durable(&mut self) -> carryfs_core::Result<()> {
            self.inner.flush_durable()
        }
    }

    /// An open that fails at any of its device reads leaves no context
    /// behind: WinFsp is told the open failed, so it will never close one.
    #[test]
    fn a_failed_open_leaves_no_handle_registered() {
        use std::sync::atomic::Ordering::SeqCst;
        let (image, fs) = mount("failed-open");
        let (file, _, _) = fs.create(&w("\\dir"), true, false).unwrap();
        fs.close(file);
        let (file, _, _) = fs.create(&w("\\dir\\f"), false, false).unwrap();
        fs.write(file, 0, b"data", false, false).unwrap();
        fs.close(file);
        fs.into_volume().unwrap().eject().unwrap();

        // A read-only volume keeps no decoded pages between calls, so every
        // step of an open reads the device again, once the adapter has also
        // forgotten the directory and attributes an earlier open left it.
        let (reads, limit) = (
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
        );
        let device = Flaky {
            inner: ImageDevice::open(&image.0, carryfs_core::MountMode::ReadOnly).unwrap(),
            reads: reads.clone(),
            limit: limit.clone(),
        };
        let fs = CarryFs::new(Volume::open(device).unwrap(), true).unwrap();
        let path = w("\\dir\\f");
        let forget = || fs.state.lock().unwrap().known.forget();
        forget();
        reads.store(0, SeqCst);
        let (context, _, _) = fs.open(&path, false, false).unwrap();
        let needed = reads.load(SeqCst);
        fs.close(context);
        assert!(needed > 1, "an open made {needed} device reads");
        for allowed in 0..needed {
            forget();
            reads.store(0, SeqCst);
            limit.store(allowed, SeqCst);
            assert!(fs.open(&path, false, false).is_err(), "{allowed} reads");
            assert!(
                fs.state.lock().unwrap().open.is_empty(),
                "an open that failed after {allowed} reads stayed registered"
            );
        }
        limit.store(u64::MAX, SeqCst);
        let (context, _, _) = fs.open(&path, false, false).unwrap();
        fs.close(context);
    }
}
