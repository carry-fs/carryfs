//! FUSE callbacks over one serialized `Volume`.
//!
//! A writable mount defers checkpoints: each operation writes its pages, and
//! the roots publish them at `fsync`, on a timer, and before unmount. The timer
//! publishes after a 100 ms pause in operations, and after one second whatever
//! the load, so a crash loses at most the changes of the last second that no
//! `fsync` covered. Closing a file is not a publication point, as it is not on
//! ext4, btrfs, XFS, or ZFS; publishing on every close cost 3.1x on file
//! creation. So `flush` is left to fuser's default `ENOSYS`, after which Linux
//! and macFUSE stop sending FLUSH for the mount, one round trip fewer per close.
//!
//! Linux publishes in `before_destroy`, while the mount still owns the
//! device, and fuser waits for it before replying to DESTROY. `destroy`
//! runs after that reply and issues no Linux writes. The Linux syncfs
//! callback also publishes and returns publication errors to the caller.
//! On macOS, DiskArbitration's unmount approval also publishes, and tells the
//! user when that fails (`eject_macos`); forget and destroy are backstops.
//! Cleanup runs in the timer's full `sync`, outside explicit publication
//! barriers. The kernel writeback cache stays off.

#[cfg(target_os = "macos")]
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::sync::mpsc::Sender;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use carryfs_core::ROOT;
use carryfs_core::{
    Access, Attributes, BlockDevice, DirectoryEntry, Error, Handle, Name, ObjectId, ObjectKind,
    RemoveKind, Replace, Timestamp, Volume,
};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, Notifier, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};

#[cfg(target_os = "macos")]
use crate::eject_macos::Failure;

// The kernel caches a name under the spelling it asked for, and cannot know
// that CarryFS matches names case-insensitively: after a change made through
// one spelling, another cached spelling of the same name would be stale. So
// only an entry's stored spelling is ever given a lifetime, and every change
// made through any other spelling invalidates cached names. Linux increments
// the connection epoch before mutation; macOS queues per-name invalidation.
// Older Linux kernels use zero entry TTL.
// Other spellings and missing names are never cached. Caching exact names let
// `stat` skip the lookup round trip. Attributes
// belong to the inode, and this mount is the only writer; the kernel drops
// them after its own write, truncate, create, unlink, and rename requests, so
// a short TTL serves repeated stat calls.
//
// A stored-spelling name lives one hour, writable mounts included: this
// mount holds the device, and the kernel updates its own entries on every
// create, unlink, and rename, as it does for ext4, which never expires names.
// With 1 s, an unlink more than a second after the listing paid an extra
// LOOKUP: on Linux 10,000 of them were 16% of a batched delete, and macOS
// measured the same round trip. The hour is not tuned; it only bounds how long
// a missed invalidation of ours, or a raw writer under the mount, could leave
// a stale name.
const ENTRY_TTL: Duration = Duration::from_secs(3600);
/// How long macOS keeps a stored spelling uncached after a lookup through
/// another spelling. A change through that spelling follows its lookup in the
/// same system call, since other spellings are never cached, so the guard
/// only has to outlast that call and the invalidation queued with it.
#[cfg(target_os = "macos")]
const ALIAS_GUARD: Duration = Duration::from_secs(1);
const NO_TTL: Duration = Duration::ZERO;
const ATTR_TTL: Duration = Duration::from_secs(1);
/// Attributes on a cached read-only mount: nothing changes them while this
/// mount holds the device, and the core already trusts its cached pages.
const CACHED_TTL: Duration = Duration::from_secs(3600);
/// Linux keeps a file's cached pages across opens, as ext4 does. This mount
/// is the only writer, its kernel updates or drops those pages on every
/// write and truncate it sends, and object IDs are never reused, so a page
/// can only be stale if the medium changes under the mount. Dropping them on
/// every open read a repeated 512 MiB file through the daemon at 2.1 GB/s.
#[cfg(target_os = "linux")]
const FILE_OPEN: FopenFlags = FopenFlags::FOPEN_KEEP_CACHE;
#[cfg(not(target_os = "linux"))]
const FILE_OPEN: FopenFlags = FopenFlags::empty();
const MAX_WRITE: u32 = 1 << 20;
/// The block size each object reports. macFUSE sends reads and writes of
/// this size, rounded up to the 16 KiB page: 4 KiB made 16 KiB writes, which
/// ran at 31 MiB/s on a USB SSD against 165 MiB/s at 1 MiB. Neither the
/// `iosize` mount option nor the statfs block size changed that.
#[cfg(target_os = "macos")]
const IO_BYTES: u32 = MAX_WRITE;
#[cfg(target_os = "linux")]
const IO_BYTES: u32 = 4096;
#[cfg(target_os = "macos")]
const MACFUSE_CASE_INSENSITIVE: u64 = 1 << 29;
const FILE_MODE: u16 = 0o644;
const DIRECTORY_MODE: u16 = 0o755;
const SYMLINK_MODE: u16 = 0o777;

type Reply<T> = std::result::Result<T, Errno>;

/// The synthetic owner reported for every object; CarryFS stores no ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// The longest pending changes and cleanup go unpublished without a request.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
/// A pause in operations this long publishes, and the timer's tick. Publishing
/// after a pause costs nothing measurable and shortens the window a crash
/// loses; the cap above is unchanged.
const IDLE_PAUSE: Duration = Duration::from_millis(100);

pub struct CarryFs<D: BlockDevice> {
    state: Arc<Mutex<State<D>>>,
    owner: Owner,
    entry_ttl: Duration,
    attr_ttl: Duration,
    /// Cached names to invalidate, sent after the reply by another thread:
    /// the kernel's invalidation takes the parent directory's lock, which an
    /// unlink or rename holds until this adapter replies.
    #[cfg(target_os = "macos")]
    stale: Sender<(INodeNo, OsString)>,
}

#[cfg(target_os = "linux")]
thread_local! {
    /// Whether this request thread has asked for `run_at_full_speed`.
    static FULL_SPEED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Ask the scheduler to clock the CPU up whenever the calling thread runs.
///
/// A caller blocks while the daemon serves its request, so each of the two
/// CPUs is busy about half the time and `schedutil` picks a low frequency for
/// both: on one test machine every request then cost about twice what it does
/// at full speed. A utilization floor on the request thread
/// asks for the full frequency only while it runs. This is a hint: kernels
/// without `CONFIG_UCLAMP_TASK` and governors that ignore it change nothing.
/// The mounting thread asks too: a writable open checks the whole forest, on
/// a CPU that a starting process finds clocked down.
#[cfg(target_os = "linux")]
pub(crate) fn run_at_full_speed() {
    /// `struct sched_attr` up to the utilization clamps (Linux 5.3).
    #[repr(C)]
    struct SchedAttr {
        size: u32,
        policy: u32,
        flags: u64,
        nice: i32,
        priority: u32,
        runtime: u64,
        deadline: u64,
        period: u64,
        util_min: u32,
        util_max: u32,
    }
    const KEEP_POLICY_AND_PARAMS: u64 = 0x08 | 0x10;
    const UTIL_CLAMP_MIN: u64 = 0x20;
    let attr = SchedAttr {
        size: size_of::<SchedAttr>() as u32,
        policy: 0,
        flags: KEEP_POLICY_AND_PARAMS | UTIL_CLAMP_MIN,
        nice: 0,
        priority: 0,
        runtime: 0,
        deadline: 0,
        period: 0,
        util_min: 1024,
        util_max: 0,
    };
    // SAFETY: `attr` is a valid `sched_attr` of the size it states, and pid 0
    // names the calling thread. A refused hint leaves the thread as it was.
    unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0) };
}

/// A name's cache lifetime: only its stored spelling may be cached.
fn name_ttl(requested: &OsStr, stored: &[u8], entry_ttl: Duration) -> Duration {
    if requested.as_bytes() == stored {
        entry_ttl
    } else {
        NO_TTL
    }
}

struct State<D: BlockDevice> {
    volume: Volume<D>,
    open: HashMap<u64, Open>,
    next: u64,
    /// When an operation last entered the filesystem, for the timer's
    /// idle trigger.
    last_operation: Instant,
    /// The generation at which the volume last had no cleanup work. Until
    /// something publishes again, or an orphan loses its last pin, there is
    /// nothing to ask the core about.
    quiet: Option<u64>,
    /// Stored names, by parent, recently reached through another spelling,
    /// and when that was. For one entry lifetime they are not cached even
    /// under their stored spelling, so no reader can cache one while a change
    /// through the other spelling is on its way.
    #[cfg(target_os = "macos")]
    aliased: HashMap<(u64, Vec<u8>), Instant>,
    /// Installed after INIT, before the session can serve namespace requests.
    #[cfg(target_os = "linux")]
    notifier: Option<Notifier>,
    /// Counts the requests and timer steps that may have changed the volume.
    /// Attributes read ahead of a listing are used only while it stands still.
    #[cfg(target_os = "linux")]
    edits: u64,
    /// Up to 4,096 writable listing lifetimes, bounded by the plus attribute TTL.
    #[cfg(target_os = "linux")]
    listed: HashMap<u64, Instant>,
    /// Set before Linux's unmount publication, or by `destroy` on session
    /// exit. Stops the timer and signal-triggered checkpoints.
    ended: bool,
    /// The root's attributes from its last successful read. A faulted volume
    /// fails every read, and AppKit's eject stats the mount point first: it
    /// failed with an I/O error without ever reaching DiskArbitration, which
    /// left the volume mounted. Only the root is served from here.
    #[cfg(target_os = "macos")]
    root: Option<FileAttr>,
    /// The first publication error, which faulted the volume. Every later
    /// call reports only `Faulted`, and the eject alert should name the cause.
    #[cfg(target_os = "macos")]
    failure: Option<Failure>,
    /// The Trash folders seen by name, by object (`note_trash`).
    #[cfg(target_os = "macos")]
    trash: HashMap<u64, Trash>,
    /// A bounded prefix of the most recently listed directory, without attributes.
    #[cfg(target_os = "macos")]
    directory_cache: Option<CachedDirectory>,
    /// Writable attributes of a clean generation; pending edits always read the core.
    #[cfg(target_os = "macos")]
    attributes: (u64, HashMap<u64, Attributes>),
}

/// macOS keeps a volume's Trash in `.Trashes` at its top, owned by root with
/// mode 01311, and in a folder per user named by user ID, owned by that user
/// with mode 0700. CarryFS stores no owner or mode, and Finder deleted
/// immediately rather than use a Trash reported as the console user's with
/// mode 0755; setting them failed. exFAT avoids this with `noowners`, which
/// macFUSE ignores. So these folders report what macOS expects; the daemon
/// checks no permissions, so nothing else changes.
#[cfg(target_os = "macos")]
enum Trash {
    Volume,
    User(u32),
}

impl<D: BlockDevice> State<D> {
    fn new(volume: Volume<D>) -> Self {
        Self {
            volume,
            open: HashMap::new(),
            next: 1,
            last_operation: Instant::now(),
            quiet: None,
            #[cfg(target_os = "macos")]
            aliased: HashMap::new(),
            #[cfg(target_os = "linux")]
            notifier: None,
            #[cfg(target_os = "linux")]
            edits: 0,
            #[cfg(target_os = "linux")]
            listed: HashMap::new(),
            ended: false,
            #[cfg(target_os = "macos")]
            root: None,
            #[cfg(target_os = "macos")]
            failure: None,
            #[cfg(target_os = "macos")]
            trash: HashMap::new(),
            #[cfg(target_os = "macos")]
            directory_cache: None,
            #[cfg(target_os = "macos")]
            attributes: (0, HashMap::new()),
        }
    }

    /// Whether `stored` under `parent` may be given a lifetime now.
    #[cfg(target_os = "macos")]
    fn cacheable(&mut self, parent: u64, stored: &[u8]) -> bool {
        let key = (parent, stored.to_vec());
        match self.aliased.get(&key) {
            Some(since) if since.elapsed() < ALIAS_GUARD => false,
            Some(_) => {
                self.aliased.remove(&key);
                true
            }
            None => true,
        }
    }

    #[cfg(target_os = "macos")]
    fn alias(&mut self, parent: u64, stored: &[u8]) {
        if self.aliased.len() >= 4096 {
            self.aliased
                .retain(|_, since| since.elapsed() < ALIAS_GUARD);
        }
        self.aliased
            .insert((parent, stored.to_vec()), Instant::now());
    }
}

enum Open {
    File(ObjectId, Handle),
    Directory(Listing),
}

/// The volume from outside its session: for signal-triggered checkpoints,
/// and for DiskArbitration's unmount approval on macOS.
pub struct Finisher<D: BlockDevice>(Arc<Mutex<State<D>>>);

impl<D: BlockDevice> Finisher<D> {
    /// Attach the notifier after the session's protocol handshake.
    #[cfg(target_os = "linux")]
    pub fn set_notifier(&self, notifier: Notifier) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("filesystem state lock poisoned"))?
            .notifier = Some(notifier);
        Ok(())
    }

    /// Publish pending changes and wait for the device, unless the session
    /// has already ended. A failure is reported as the first publication
    /// error, which faulted the volume, rather than as the fault itself.
    #[cfg(target_os = "macos")]
    pub fn sync(&self) -> Result<(), Failure> {
        // A poisoned lock means an operation panicked partway.
        let mut state = self.0.lock().map_err(|_| Failure::from(&Error::Faulted))?;
        if state.ended {
            return Ok(());
        }
        let result = state.volume.sync();
        if let Err(error) = &result {
            state.failed(error);
        }
        result.map_err(|error| {
            state
                .failure
                .take()
                .unwrap_or_else(|| Failure::from(&error))
        })
    }

    /// The first publication error, unless an unmount approval already
    /// reported it. A plain umount(8) asks no approver, so `serve` reports
    /// this once the session has ended.
    #[cfg(target_os = "macos")]
    pub fn take_failure(&self) -> Option<Failure> {
        match self.0.lock() {
            Ok(mut state) => state.failure.take(),
            Err(_) => Some(Failure::from(&Error::Faulted)),
        }
    }

    /// Read the catalog's leaves ahead of the first requests on `reader`, a
    /// second handle to the mounted device, for a read-only mount. A first
    /// listing spent most of its time reading, verifying and decoding catalog
    /// pages on the request thread; this thread does that beside it and stops
    /// once the metadata cache is half full.
    #[cfg(target_os = "linux")]
    pub fn read_ahead<R: BlockDevice + Send + 'static>(&self, mut reader: R) -> io::Result<()>
    where
        D: Send + 'static,
    {
        let mut ahead = self
            .0
            .lock()
            .map_err(|_| io::Error::other("filesystem state lock poisoned"))?
            .volume
            .read_ahead()
            .map_err(io::Error::other)?;
        let state = Arc::downgrade(&self.0);
        std::thread::Builder::new()
            .name("carryfs-ahead".into())
            .spawn(move || {
                run_at_full_speed();
                // An error ends the read-ahead: the request that needs the
                // page reads it itself and reports the error.
                while let Ok(leaves) = ahead.next(&mut reader, 32) {
                    if leaves.is_empty() {
                        return;
                    }
                    let Some(state) = state.upgrade() else {
                        return;
                    };
                    let Ok(mut state) = state.lock() else {
                        return;
                    };
                    if state.ended || !state.volume.keep_read_ahead(leaves).unwrap_or(false) {
                        return;
                    }
                }
            })?;
        Ok(())
    }

    /// Publish pending changes now, as the timer would, unless the session
    /// has already ended. A failure is logged and kept for the end of the
    /// session, as the timer's is.
    pub fn checkpoint(&self) {
        if let Ok(mut state) = self.0.lock()
            && !state.ended
        {
            state.maintain();
        }
    }
}

/// Pass the names `CarryFs::new` returned to the kernel on a thread of their
/// own. It ends once the session drops the filesystem, which holds the sender.
#[cfg(target_os = "macos")]
pub fn forward_forgotten(
    notifier: Notifier,
    forgotten: Receiver<(INodeNo, OsString)>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("carryfs-forget".into())
        .spawn(move || {
            for (parent, name) in forgotten {
                // A name the kernel does not hold is the common case.
                if let Err(error) = notifier.inval_entry(parent, &name)
                    && error.raw_os_error() != Some(libc::ENOENT)
                {
                    eprintln!("carryfs: dropping a cached name failed: {error}");
                }
            }
        })?;
    Ok(())
}

impl<D: BlockDevice> CarryFs<D> {
    /// The filesystem and macOS's queued name invalidations. Linux installs
    /// a notifier after INIT and leaves the receiver disconnected. Strict
    /// mounts give names no lifetime and send no invalidations.
    pub fn new(
        volume: Volume<D>,
        owner: Owner,
        strict: bool,
    ) -> (Self, Receiver<(INodeNo, OsString)>) {
        let (stale, forgotten) = channel();
        #[cfg(target_os = "linux")]
        drop(stale);
        let fs = Self {
            state: Arc::new(Mutex::new(State::new(volume))),
            owner,
            entry_ttl: if strict { NO_TTL } else { ENTRY_TTL },
            attr_ttl: ATTR_TTL,
            #[cfg(target_os = "macos")]
            stale,
        };
        (fs, forgotten)
    }

    /// What `serve` needs to publish outside the session.
    pub fn finisher(&self) -> Finisher<D> {
        Finisher(Arc::clone(&self.state))
    }

    /// Keep names and attributes for `CACHED_TTL` on a read-only mount.
    /// Nothing changes attributes while the mount holds the device. Other
    /// spellings and strict mounts stay uncached.
    pub fn set_read_only_cache(&mut self) {
        #[cfg(target_os = "macos")]
        if self.entry_ttl != NO_TTL {
            self.entry_ttl = CACHED_TTL;
        }
        self.attr_ttl = CACHED_TTL;
    }

    /// Invalidate `stored` under `parent` when a change used another
    /// spelling: the kernel updates the dentry of the spelling it used, and
    /// only the stored one can be cached besides.
    #[cfg(target_os = "macos")]
    fn forget(&self, parent: INodeNo, requested: &OsStr, stored: &[u8]) {
        if self.entry_ttl != NO_TTL && requested.as_bytes() != stored {
            // A closed receiver means the session is ending and nothing is
            // cached any more.
            let _ = self
                .stale
                .send((parent, OsStr::from_bytes(stored).to_os_string()));
        }
    }

    fn lookup_ttl(
        &self,
        state: &mut State<D>,
        parent: INodeNo,
        requested: &OsStr,
        stored: &[u8],
    ) -> Duration {
        #[cfg(target_os = "linux")]
        {
            let _ = (state, parent);
            name_ttl(requested, stored, self.entry_ttl)
        }
        #[cfg(target_os = "macos")]
        {
            if self.entry_ttl == NO_TTL {
                return NO_TTL;
            }
            if requested.as_bytes() == stored {
                if state.cacheable(u64::from(parent), stored) {
                    self.entry_ttl
                } else {
                    NO_TTL
                }
            } else {
                // A change through another spelling starts with this lookup.
                // Dropping the stored spelling now queues ahead of the change
                // for the directory lock, and the guard keeps it uncached.
                state.alias(u64::from(parent), stored);
                self.forget(parent, requested, stored);
                NO_TTL
            }
        }
    }

    /// Invalidate before changing names, while the state lock serializes
    /// lookups. A failed notification must leave the namespace unchanged.
    #[cfg(target_os = "linux")]
    fn invalidate_names(&self, notifier: &mut Option<Notifier>, needed: bool) -> Result<(), Error> {
        if !needed || self.entry_ttl == NO_TTL {
            return Ok(());
        }
        let current = notifier.as_ref().ok_or(Error::Faulted)?;
        if let Err(error) = current.inc_epoch() {
            // Do not retry with caching disabled: old cached entries remain.
            // Refuse further alias-sensitive mutations until remount.
            *notifier = None;
            return Err(Error::Io {
                operation: "invalidating cached names failed; remount required",
                source: error,
            });
        }
        Ok(())
    }

    /// Run a request that may change the volume.
    fn with<T>(&self, operation: impl FnOnce(&mut State<D>) -> Reply<T>) -> Reply<T> {
        self.reading(|state| {
            #[cfg(target_os = "linux")]
            {
                state.edits += 1;
            }
            operation(state)
        })
    }

    /// Run a request that only reads the volume, so attributes read ahead of
    /// an open listing stay valid across it.
    fn reading<T>(&self, operation: impl FnOnce(&mut State<D>) -> Reply<T>) -> Reply<T> {
        #[cfg(target_os = "linux")]
        if !FULL_SPEED.replace(true) {
            run_at_full_speed();
        }
        let mut state = self.state.lock().map_err(|_| Errno::EIO)?;
        state.last_operation = Instant::now();
        operation(&mut state)
    }

    fn attr(&self, object: ObjectId, attributes: &Attributes) -> Reply<FileAttr> {
        let (kind, perm) = file_type(attributes.kind);
        let modification = system_time(attributes.modification)?;
        Ok(FileAttr {
            ino: INodeNo(object.get()),
            size: attributes.size,
            blocks: attributes.allocated_bytes / 512,
            // Access times are not stored.
            atime: modification,
            mtime: modification,
            ctime: system_time(attributes.metadata_change)?,
            crtime: system_time(attributes.creation)?,
            kind,
            perm,
            // Link counts are not stored; 1 tells tools such as find not to infer
            // subdirectories. An unlinked object still open reports 0, as fstat expects.
            nlink: u32::from(!attributes.orphan),
            uid: self.owner.uid,
            gid: self.owner.gid,
            rdev: 0,
            flags: 0,
            blksize: IO_BYTES,
        })
    }
}

impl<D: BlockDevice> State<D> {
    /// Keep old cookies as name markers, but rescan after a namespace change.
    fn changed_directory(&mut self, directory: ObjectId) {
        #[cfg(target_os = "linux")]
        self.listed.remove(&directory.get());
        #[cfg(target_os = "macos")]
        if self
            .directory_cache
            .as_ref()
            .is_some_and(|cached| cached.directory == directory)
        {
            self.directory_cache = None;
        }
        for open in self.open.values_mut() {
            if let Open::Directory(listing) = open
                && listing.directory == directory
            {
                listing.refresh = true;
            }
        }
    }

    /// Expired plus attributes need a fresh listing, otherwise each stat costs
    /// a GETATTR request.
    #[cfg(target_os = "linux")]
    fn directory_flags(&mut self, directory: ObjectId, attr_ttl: Duration) -> FopenFlags {
        let flags = FopenFlags::FOPEN_CACHE_DIR;
        let now = Instant::now();
        if attr_ttl == CACHED_TTL
            || self
                .listed
                .get(&directory.get())
                .is_some_and(|since| now.duration_since(*since) < attr_ttl)
        {
            return flags | FopenFlags::FOPEN_KEEP_CACHE;
        }
        if self.listed.len() >= 4096 {
            self.listed.clear();
        }
        self.listed.insert(directory.get(), now);
        flags
    }

    fn insert(&mut self, open: Open) -> u64 {
        let fh = self.next;
        self.next += 1;
        self.open.insert(fh, open);
        fh
    }

    fn sync_handle(&mut self, fh: FileHandle, directory: bool) -> Reply<()> {
        match self.open.get(&u64::from(fh)) {
            Some(Open::Directory(_)) if directory => {}
            Some(Open::File(_, _)) if !directory => {}
            _ => return Err(Errno::EBADF),
        }
        self.sync()
    }

    fn sync(&mut self) -> Reply<()> {
        let result = self.volume.sync_publication();
        #[cfg(target_os = "macos")]
        if let Err(error) = &result {
            self.failed(error);
        }
        result.map_err(|e| errno(&e))
    }

    /// Keep the first publication error; later ones are its consequence.
    #[cfg(target_os = "macos")]
    fn failed(&mut self, error: &Error) {
        if self.failure.is_none() && !matches!(error, Error::Faulted) {
            self.failure = Some(Failure::from(error));
        }
    }

    /// Close an open file or directory, as `release` and `releasedir` require.
    fn close(&mut self, fh: FileHandle, directory: bool) -> Reply<()> {
        let fh = u64::from(fh);
        let (object, handle) = match self.open.remove(&fh) {
            Some(Open::File(object, handle)) if !directory => (object, handle),
            Some(Open::Directory(listing)) if directory => (listing.directory, listing.handle),
            Some(other) => {
                self.open.insert(fh, other);
                return Err(Errno::EBADF);
            }
            None => return Err(Errno::EBADF),
        };
        self.volume.close_handle(handle).map_err(|e| errno(&e))?;
        let pinned = self.open.values().any(|open| match open {
            Open::File(other, _) => *other == object,
            Open::Directory(listing) => listing.directory == object,
        });
        // An unpinned orphan is new cleanup work. Marking the volume unquiet
        // lets the timer pick it up; a close does not publish. If attributes
        // fail, the next maintenance step reports the underlying error.
        let unpinned_orphan = !pinned
            && self
                .volume
                .attributes(object)
                .map_or(true, |attributes| attributes.orphan);
        if unpinned_orphan {
            self.quiet = None;
        }
        Ok(())
    }

    /// Publish pending changes and run bounded cleanup when there may be work.
    /// The triggering operations were already acknowledged, so a failure here
    /// is logged and faults later requests instead.
    fn maintain(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.edits += 1;
        }
        match self.maintenance_step() {
            Ok(()) | Err(Error::Faulted) => {}
            Err(error) => {
                eprintln!("carryfs: maintenance failed: {error}");
                #[cfg(target_os = "macos")]
                self.failed(&error);
            }
        }
    }

    fn maintenance_step(&mut self) -> Result<(), Error> {
        let generation = self.volume.info()?.generation;
        if self.volume.has_unsynced_changes() {
            return self.volume.sync();
        }
        if self.quiet == Some(generation) {
            return Ok(());
        }
        // Asking costs no flush, where a sync that finds nothing still
        // flushes. A checkpoint can make retirement eligible, so the step
        // after one asks again.
        if self.volume.has_cleanup_work()? {
            self.volume.sync()
        } else {
            self.quiet = Some(generation);
            Ok(())
        }
    }

    /// Apply a `setattr` in as few commits as it allows. The kernel sends a
    /// size change with a modification time of now, which the core's truncate
    /// sets in the same commit, so the usual request is one commit and cannot
    /// fail halfway. `size` is `None` when the size does not change.
    fn set_size_and_times(
        &mut self,
        object: ObjectId,
        size: Option<u64>,
        mtime: Option<TimeOrNow>,
        creation: Option<Timestamp>,
    ) -> Reply<()> {
        let modification = match mtime {
            Some(TimeOrNow::Now) if size.is_some() => None,
            mtime => mtime.map(time_or_now).transpose()?,
        };
        if size.is_none() && modification.is_none() && creation.is_none() {
            return Ok(());
        }
        // A temporary handle also reaches an unlinked file still open elsewhere.
        let access = if size.is_some() {
            Access::ReadWrite
        } else {
            Access::Read
        };
        let handle = self
            .volume
            .open_object(object, access)
            .map_err(|e| errno(&e))?;
        let result = size
            .map_or(Ok(()), |size| {
                with_space(&mut self.volume, |volume| volume.truncate(&handle, size))
            })
            .and_then(|()| {
                if modification.is_some() || creation.is_some() {
                    self.volume.set_times(&handle, creation, modification)
                } else {
                    Ok(())
                }
            });
        let close = self.volume.close_handle(handle);
        result.and(close).map_err(|e| errno(&e))
    }

    fn entry(&mut self, fs: &CarryFs<D>, object: ObjectId) -> Reply<FileAttr> {
        #[cfg(target_os = "macos")]
        let attributes = if fs.attr_ttl == ATTR_TTL {
            self.cached_attributes(object)?
        } else {
            self.volume.attributes(object).map_err(|e| errno(&e))?
        };
        #[cfg(not(target_os = "macos"))]
        let attributes = self.volume.attributes(object).map_err(|e| errno(&e))?;
        let attr = fs.attr(object, &attributes)?;
        #[cfg(target_os = "macos")]
        let attr = self.as_trash(object, attr);
        Ok(attr)
    }

    #[cfg(target_os = "macos")]
    fn cached_attributes(&mut self, object: ObjectId) -> Reply<Attributes> {
        if self.volume.is_faulted() {
            return Err(Errno::EIO);
        }
        if self.volume.has_unsynced_changes() {
            return self.volume.attributes(object).map_err(|e| errno(&e));
        }
        let generation = self.volume.info().map_err(|e| errno(&e))?.generation;
        if generation != self.attributes.0 {
            self.attributes.1.clear();
            self.attributes.0 = generation;
        }
        if let Some(attributes) = self.attributes.1.get(&object.get()) {
            return Ok(*attributes);
        }
        let attributes = self.volume.attributes(object).map_err(|e| errno(&e))?;
        if self.attributes.1.len() == DIRECTORY_CACHE_ENTRIES {
            self.attributes.1.clear();
        }
        self.attributes.1.insert(object.get(), attributes);
        Ok(attributes)
    }

    /// `attr` with the owner and mode macOS expects if `object` is a Trash
    /// folder.
    #[cfg(target_os = "macos")]
    fn as_trash(&self, object: ObjectId, mut attr: FileAttr) -> FileAttr {
        match self.trash.get(&object.get()) {
            Some(Trash::Volume) => (attr.uid, attr.gid, attr.perm) = (0, 0, 0o1311),
            Some(&Trash::User(uid)) => (attr.uid, attr.perm) = (uid, 0o700),
            None => {}
        }
        attr
    }

    /// Remember `object` if `name` under `parent` makes it a Trash folder.
    #[cfg(target_os = "macos")]
    fn note_trash(&mut self, parent: ObjectId, name: &[u8], object: ObjectId) {
        let trash = if parent == ROOT && name == b".Trashes" {
            Trash::Volume
        } else if let Some(Trash::Volume) = self.trash.get(&parent.get())
            && let Some(uid) = std::str::from_utf8(name)
                .ok()
                .and_then(|name| name.parse().ok())
        {
            Trash::User(uid)
        } else {
            return;
        };
        self.trash.insert(object.get(), trash);
    }
}

/// Listed entries a directory handle keeps, so that a seek back within them
/// resumes exactly. Four core pages.
const KEPT_ENTRIES: usize = 256;

/// Keep only one directory prefix; larger listings continue through the core.
#[cfg(target_os = "macos")]
const DIRECTORY_CACHE_ENTRIES: usize = 16_384;

#[cfg(target_os = "macos")]
pub(crate) struct CachedDirectory {
    directory: ObjectId,
    entries: Vec<DirectoryEntry>,
    complete: bool,
}

#[cfg(target_os = "macos")]
impl CachedDirectory {
    /// A resumed handle may keep cookies from before a mutation. Its marker
    /// must still agree with the rebuilt cache's ordinal.
    fn follows(&self, listing: &Listing, end: u64) -> bool {
        self.directory == listing.directory
            && end <= self.entries.len() as u64
            && (end == 0
                || self
                    .entries
                    .get(end as usize - 1)
                    .zip(listing.entries.back())
                    .is_some_and(|(cached, marker)| cached.name.stored() == marker.name.stored()))
    }
}

/// One open directory enumeration.
///
/// FUSE offsets: 1 follows ".", 2 follows "..", and `3 + n` follows the n-th
/// entry listed since opendir. Pages resume after the last listed name, so
/// removing entries while listing, as `rm -rf` does, neither skips nor repeats
/// unchanged entries. The last `KEPT_ENTRIES` listed stay buffered, which
/// covers a seekdir back to a recent telldir, as Samba does when a reply is
/// full.
pub(crate) struct Listing {
    handle: Handle,
    directory: ObjectId,
    parent: ObjectId,
    /// Entries listed since opendir, the first of them at index `start`.
    entries: VecDeque<DirectoryEntry>,
    start: u64,
    more: bool,
    /// Buffered names may have moved or been replaced since the last reply.
    refresh: bool,
    #[cfg(target_os = "linux")]
    ahead: Ahead,
}

/// Attributes of the entries that follow a full plus reply, read while the
/// kernel links that reply's entries. The kernel takes about as long over a
/// reply as the core takes to read the next one's attributes, so reading them
/// ahead lets the two overlap.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Ahead {
    /// The listing index of the first entry in `attrs`.
    first: u64,
    /// `State::edits` when these were read.
    edits: u64,
    attrs: Vec<(u64, FileAttr)>,
}

#[cfg(target_os = "linux")]
impl Ahead {
    /// The attributes read ahead for the entry a reply gives cookie `next`.
    fn get(&self, next: u64, ino: u64) -> Option<FileAttr> {
        let index = next.checked_sub(3)?.checked_sub(self.first)?;
        let (object, attr) = self.attrs.get(usize::try_from(index).ok()?)?;
        (*object == ino).then_some(*attr)
    }
}

impl Listing {
    pub(crate) fn new(handle: Handle, directory: ObjectId, parent: ObjectId) -> Self {
        Self {
            handle,
            directory,
            parent,
            entries: VecDeque::new(),
            start: 0,
            more: true,
            refresh: false,
            #[cfg(target_os = "linux")]
            ahead: Ahead::default(),
        }
    }

    /// Buffer the `count` entries from listing index `first` on and read their
    /// attributes with `attr`, for the plus request expected next. A failure
    /// here only ends the read-ahead: the request reads what is missing
    /// itself and reports the error.
    #[cfg(target_os = "linux")]
    fn prepare<D: BlockDevice>(
        &mut self,
        volume: &mut Volume<D>,
        first: u64,
        count: usize,
        edits: u64,
        mut attr: impl FnMut(&mut Volume<D>, ObjectId) -> Option<FileAttr>,
    ) {
        self.ahead = Ahead::default();
        if self.refresh || first < self.start {
            return;
        }
        while self.more && self.start + (self.entries.len() as u64) < first + count as u64 {
            let Some(last) = self.entries.back() else {
                return;
            };
            let Ok(page) = volume.read_directory_after(&self.handle, &last.name) else {
                return;
            };
            self.more = page.cursor.is_some() && !page.entries.is_empty();
            self.entries.extend(page.entries);
        }
        // The entries before `first` stay kept for a seek back, as in `fill`.
        while self.start + (KEPT_ENTRIES as u64) < first {
            self.entries.pop_front();
            self.start += 1;
        }
        let mut attrs = Vec::with_capacity(count);
        for entry in self
            .entries
            .iter()
            .skip((first - self.start) as usize)
            .take(count)
        {
            match attr(volume, entry.object) {
                Some(attr) => attrs.push((entry.object.get(), attr)),
                None => break,
            }
        }
        self.ahead = Ahead {
            first,
            edits,
            attrs,
        };
    }

    /// Emit entries from FUSE `offset` until `add` reports a full buffer or the listing ends.
    pub(crate) fn fill<D: BlockDevice>(
        &mut self,
        volume: &mut Volume<D>,
        offset: u64,
        #[cfg(target_os = "macos")] cache: &mut Option<CachedDirectory>,
        mut add: impl FnMut(&mut Volume<D>, u64, u64, FileType, &OsStr) -> bool,
    ) -> Result<(), Error> {
        #[cfg(target_os = "macos")]
        volume.info()?; // A cached listing must still refuse a faulted volume.
        let mut index = offset.saturating_sub(2);
        if self.refresh {
            self.parent = volume.attributes(self.directory)?.parent;
            // Preserve the names before the resume cookie, including a removed
            // marker. Rescan its successors so a plus reply cannot cache an old
            // name-to-object association.
            self.entries.truncate(
                index
                    .saturating_sub(self.start)
                    .min(self.entries.len() as u64) as usize,
            );
            self.more = true;
            self.refresh = false;
        }
        let dot = FileType::Directory;
        if offset < 1 && add(volume, self.directory.get(), 1, dot, OsStr::new(".")) {
            return Ok(());
        }
        if offset < 2 && add(volume, self.parent.get(), 2, dot, OsStr::new("..")) {
            return Ok(());
        }
        // Offset 0 starts a new pass, and macFUSE reuses one handle across
        // opendirs, so the buffered entries may predate entries added since.
        if offset == 0 || index < self.start {
            // Seeking before the buffered entries rescans from the start. After
            // directory changes, the same offset may refer to a different entry.
            self.entries.clear();
            self.start = 0;
            self.more = true;
        }
        loop {
            let end = self.start + self.entries.len() as u64;
            if index >= end {
                if !self.more {
                    return Ok(());
                }
                #[cfg(target_os = "macos")]
                let cached = cache.as_ref().and_then(|cached| {
                    if !cached.follows(self, end)
                        || end == cached.entries.len() as u64 && !cached.complete
                    {
                        return None;
                    }
                    let start = end as usize;
                    let stop = (start + KEPT_ENTRIES).min(cached.entries.len());
                    Some((
                        cached.entries[start..stop].to_vec(),
                        stop < cached.entries.len() || !cached.complete,
                    ))
                });
                #[cfg(not(target_os = "macos"))]
                let cached = None;
                let (entries, more) = match cached {
                    Some(page) => page,
                    None => {
                        let page = match self.entries.back() {
                            Some(last) => volume.read_directory_after(&self.handle, &last.name)?,
                            None => volume.read_directory(&self.handle, None)?,
                        };
                        let more = page.cursor.is_some() && !page.entries.is_empty();
                        #[cfg(target_os = "macos")]
                        {
                            if end == 0 {
                                *cache = Some(CachedDirectory {
                                    directory: self.directory,
                                    entries: Vec::new(),
                                    complete: false,
                                });
                            }
                            if let Some(cached) = cache
                                && cached.follows(self, end)
                                && end == cached.entries.len() as u64
                            {
                                let count = page
                                    .entries
                                    .len()
                                    .min(DIRECTORY_CACHE_ENTRIES - cached.entries.len());
                                cached
                                    .entries
                                    .extend(page.entries.iter().take(count).cloned());
                                cached.complete = !more && count == page.entries.len();
                            }
                        }
                        (page.entries, more)
                    }
                };
                self.more = more;
                self.entries.extend(entries);
                // Core pages and cached chunks fit within the kept entries.
                while self.entries.len() > KEPT_ENTRIES {
                    self.entries.pop_front();
                    self.start += 1;
                }
                continue;
            }
            let entry = &self.entries[(index - self.start) as usize];
            if add(
                volume,
                entry.object.get(),
                index + 3,
                file_type(entry.kind).0,
                OsStr::from_bytes(entry.name.stored()),
            ) {
                return Ok(());
            }
            index += 1;
        }
    }
}

impl<D: BlockDevice + Send + 'static> Filesystem for CarryFs<D> {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if self.entry_ttl != NO_TTL && config.kernel_abi() < fuser::Version(7, 44) {
            self.entry_ttl = NO_TTL;
            eprintln!("carryfs: FUSE ABI below 7.44; name caching disabled");
        }
        // One FUSE write maps to one bounded core write.
        config
            .set_max_write(MAX_WRITE)
            .map_err(|_| io::Error::other("kernel rejected the 1 MiB write size"))?;
        // Full plus replies favor list-all-then-stat callers such as file
        // managers and rsync. Strict mounts and older kernels, whose plus
        // entries carry zero TTL, let Linux switch to plain replies (AUTO).
        // macFUSE speaks protocol 7.19, which predates plus listings.
        #[cfg(target_os = "linux")]
        {
            let mut flags = InitFlags::FUSE_DO_READDIRPLUS;
            if self.entry_ttl == NO_TTL {
                flags |= InitFlags::FUSE_READDIRPLUS_AUTO;
            }
            config
                .add_capabilities(flags)
                .map_err(|_| io::Error::other("kernel lacks plus listings"))?;
        }
        // A macFUSE bit (its fuse_kernel.h): names match case-insensitively,
        // which pathconf reports to applications.
        #[cfg(target_os = "macos")]
        config
            .add_capabilities(
                InitFlags::from_bits_retain(MACFUSE_CASE_INSENSITIVE) | InitFlags::FUSE_VOL_RENAME,
            )
            .map_err(|_| {
                io::Error::other("macFUSE lacks case-insensitive names or volume rename")
            })?;
        // The thread ends once the session drops the filesystem.
        let state = Arc::downgrade(&self.state);
        std::thread::Builder::new()
            .name("carryfs-checkpoint".into())
            .spawn(move || {
                let mut published = Instant::now();
                loop {
                    std::thread::sleep(IDLE_PAUSE);
                    let Some(state) = state.upgrade() else {
                        return;
                    };
                    if let Ok(mut state) = state.lock() {
                        if state.ended {
                            return;
                        }
                        let now = Instant::now();
                        // Publish once operations have paused, or once the cap
                        // is reached under load, whichever comes first.
                        if now.duration_since(state.last_operation) >= IDLE_PAUSE
                            || now.duration_since(published) >= CHECKPOINT_INTERVAL
                        {
                            state.maintain();
                            published = now;
                        }
                    }
                }
            })?;
        Ok(())
    }

    /// Publish while the mount still owns the device, before unmount can
    /// return. A failed publication must allow removal, with a warning.
    #[cfg(target_os = "linux")]
    fn before_destroy(&self) {
        let result = self.with(|state| {
            state.ended = true;
            #[cfg(feature = "fault-injection")]
            if let Some(path) = std::env::var_os("CARRYFS_PAUSE_BEFORE_DESTROY")
                && std::path::Path::new(&path).exists()
            {
                eprintln!("carryfs: paused before the unmount publication");
                while std::path::Path::new(&path).exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            state.sync()
        });
        if result.is_err() {
            eprintln!(
                "carryfs: unmounted after a failed publication; recent changes may be lost. \
                 Reconnect and use carryfs check, scrub, and get to recover to another disk."
            );
        }
    }

    /// Stop the timer even if the session aborted without DESTROY. Linux
    /// must not write here, because the kernel may have released its claim.
    fn destroy(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.ended = true;
            #[cfg(target_os = "macos")]
            state.maintain();
        }
    }

    /// macFUSE's unmount forgets every cached vnode and then sends DESTROY,
    /// which fuser acknowledges before it calls `destroy`, so unmount can
    /// return before `destroy` publishes. Every change leaves a vnode to
    /// forget, and the session handles requests in order, so publishing here
    /// lands before that acknowledgement.
    #[cfg(target_os = "macos")]
    fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {
        if let Ok(mut state) = self.state.lock()
            && state.volume.has_unsynced_changes()
        {
            state.maintain();
        }
    }

    #[cfg(target_os = "macos")]
    fn setvolname(&self, _req: &Request, name: &OsStr, reply: ReplyEmpty) {
        let result = self.with(|state| {
            let name = name.to_str().ok_or(Errno::EINVAL)?;
            let label = carryfs_core::VolumeLabel::parse(name).map_err(|e| errno(&e))?;
            let result = state.volume.set_label(label);
            if let Err(error) = &result
                && !matches!(error, Error::ReadOnly | Error::InvalidInput(_))
            {
                state.failed(error);
            }
            result.map_err(|e| errno(&e))
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.reading(|s| {
            let directory = id(parent)?;
            let entry = s
                .volume
                .lookup_entry(directory, &parse(name)?)
                .map_err(|e| errno(&e))?;
            let stored = entry.name.stored();
            let ttl = self.lookup_ttl(s, parent, name, stored);
            #[cfg(target_os = "macos")]
            s.note_trash(directory, stored, entry.object);
            Ok((s.entry(self, entry.object)?, ttl))
        }) {
            Ok((attr, ttl)) => reply.entry_with_ttls(&self.attr_ttl, &ttl, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.reading(|s| {
            let attr = s.entry(self, id(ino)?);
            #[cfg(target_os = "macos")]
            if ino == INodeNo::ROOT {
                match attr {
                    Ok(attr) => s.root = Some(attr),
                    Err(_) => return s.root.ok_or(Errno::EIO),
                }
            }
            attr
        }) {
            Ok(attr) => reply.attr(&self.attr_ttl, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        _killpriv: bool,
        reply: ReplyAttr,
    ) {
        match self.with(|s| {
            let object = id(ino)?;
            let current = s.volume.attributes(object).map_err(|e| errno(&e))?;
            let shown = s.entry(self, object)?;
            // macOS copies restore mode and timestamps together. Accept mode
            // as a no-op so Finder can clear its unfinished-copy timestamp;
            // reported permissions stay synthetic.
            if (cfg!(target_os = "linux")
                && mode.is_some_and(|mode| mode & 0o7777 != u32::from(shown.perm)))
                || uid.is_some_and(|uid| uid != shown.uid)
                || gid.is_some_and(|gid| gid != shown.gid)
            {
                return Err(Errno::EPERM);
            }
            let creation = crtime.map(timestamp).transpose()?;
            let resize = size.filter(|&size| size != current.size);
            s.set_size_and_times(object, resize, mtime, creation)?;
            s.entry(self, object)
        }) {
            Ok(attr) => reply.attr(&self.attr_ttl, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _owner: fuser::Owner,
        reply: ReplyEntry,
    ) {
        match self.with(|s| {
            let (parent, requested, name) = (id(parent)?, name, parse(name)?);
            let object = with_space(&mut s.volume, |volume| {
                volume.create_directory(parent, name.clone())
            })
            .map_err(|e| errno(&e))?;
            s.changed_directory(parent);
            #[cfg(target_os = "macos")]
            s.note_trash(parent, name.stored(), object);
            Ok((
                s.entry(self, object)?,
                name_ttl(requested, name.stored(), self.entry_ttl),
            ))
        }) {
            Ok((attr, ttl)) => reply.entry_with_ttls(&self.attr_ttl, &ttl, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        _owner: fuser::Owner,
        reply: ReplyEntry,
    ) {
        match self.with(|s| {
            let (parent, requested, name) = (id(parent)?, link_name, parse(link_name)?);
            let object = with_space(&mut s.volume, |volume| {
                volume.create_symlink(parent, name.clone(), target.as_os_str().as_bytes())
            })
            .map_err(|e| errno(&e))?;
            s.changed_directory(parent);
            Ok((
                s.entry(self, object)?,
                name_ttl(requested, name.stored(), self.entry_ttl),
            ))
        }) {
            Ok((attr, ttl)) => reply.entry_with_ttls(&self.attr_ttl, &ttl, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.with(|s| s.volume.read_symlink(id(ino)?).map_err(|e| errno(&e))) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(reply, self.remove(parent, name, RemoveKind::File));
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        reply_empty(reply, self.remove(parent, name, RemoveKind::Directory));
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        _owner: Option<fuser::Owner>,
        reply: ReplyEmpty,
    ) {
        reply_empty(
            reply,
            self.with(|s| {
                let replace = rename_replace(flags)?;
                let (source_parent, source) = (id(parent)?, parse(name)?);
                let (destination_parent, destination) = (id(newparent)?, parse(newname)?);
                let moved = s.volume.lookup_entry(source_parent, &source).ok();
                let replaced = s.volume.lookup_entry(destination_parent, &destination).ok();
                #[cfg(target_os = "linux")]
                self.invalidate_names(
                    &mut s.notifier,
                    moved
                        .as_ref()
                        .is_some_and(|entry| name.as_bytes() != entry.name.stored())
                        || replaced
                            .as_ref()
                            .is_some_and(|entry| newname.as_bytes() != entry.name.stored())
                        || newname.as_bytes() != destination.stored(),
                )
                .map_err(|error| errno(&error))?;
                with_space(&mut s.volume, |volume| {
                    volume.rename(
                        source_parent,
                        &source,
                        destination_parent,
                        destination.clone(),
                        replace,
                    )
                })
                .map_err(|e| errno(&e))?;
                s.changed_directory(source_parent);
                s.changed_directory(destination_parent);
                if let Some(entry) = &moved {
                    s.changed_directory(entry.object);
                }
                // A replaced destination becomes an orphan; the next sync cleans it up.
                #[cfg(target_os = "macos")]
                if let Some(entry) = moved {
                    self.forget(parent, name, entry.name.stored());
                }
                #[cfg(target_os = "macos")]
                if let Some(entry) = replaced {
                    self.forget(newparent, newname, entry.name.stored());
                }
                // The kernel moves its dentry to the requested spelling, which
                // keeps its lifetime; drop it unless it is the stored one.
                #[cfg(target_os = "macos")]
                self.forget(
                    newparent,
                    OsStr::from_bytes(destination.stored()),
                    newname.as_bytes(),
                );
                Ok(())
            }),
        );
    }

    fn open(
        &self,
        _req: &Request,
        ino: INodeNo,
        flags: OpenFlags,
        _killpriv: bool,
        reply: ReplyOpen,
    ) {
        let access = match flags.acc_mode() {
            OpenAccMode::O_RDONLY => Access::Read,
            OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR => Access::ReadWrite,
        };
        // The kernel sends opendir, not open, for directories.
        match self.with(|s| {
            let object = id(ino)?;
            let handle = s
                .volume
                .open_object(object, access)
                .map_err(|e| open_errno(&e))?;
            Ok(s.insert(Open::File(object, handle)))
        }) {
            Ok(fh) => reply.opened(FileHandle(fh), FILE_OPEN),
            Err(e) => reply.error(e),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        _killpriv: bool,
        _owner: fuser::Owner,
        reply: ReplyCreate,
    ) {
        match self.with(|s| {
            let (parent, requested, name) = (id(parent)?, name, parse(name)?);
            let handle = with_space(&mut s.volume, |volume| {
                volume.create_file(parent, name.clone())
            })
            .map_err(|e| open_errno(&e))?;
            s.changed_directory(parent);
            let attr = s
                .volume
                .lookup(parent, &name)
                .map_err(|e| errno(&e))
                .and_then(|object| Ok((object, s.entry(self, object)?)));
            match attr {
                Ok((object, attr)) => Ok((
                    attr,
                    s.insert(Open::File(object, handle)),
                    name_ttl(requested, name.stored(), self.entry_ttl),
                )),
                // The file stays created, as after any failed publication-side step.
                Err(e) => {
                    s.volume.close_handle(handle).map_err(|e| errno(&e))?;
                    Err(e)
                }
            }
        }) {
            Ok((attr, fh, ttl)) => {
                reply.created_with_ttls(
                    &self.attr_ttl,
                    &ttl,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    FILE_OPEN,
                );
            }
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.with(|s| {
            let handle = match s.open.get(&u64::from(fh)) {
                Some(Open::File(_, handle)) => handle,
                _ => return Err(Errno::EBADF),
            };
            let mut buffer = vec![0; size as usize];
            let mut done = 0;
            while done < buffer.len() {
                let position = offset.checked_add(done as u64).ok_or(Errno::EINVAL)?;
                // A short reply reads as zeros past it, so damaged data must fail the whole request.
                let read = s
                    .volume
                    .read(handle, position, &mut buffer[done..])
                    .map_err(|e| errno(&e))?;
                if read == 0 {
                    break;
                }
                done += read;
            }
            buffer.truncate(done);
            Ok(buffer)
        }) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.with(|s| {
            let handle = match s.open.get(&u64::from(fh)) {
                Some(Open::File(_, handle)) => handle,
                _ => return Err(Errno::EBADF),
            };
            // `max_write` is the core's 1 MiB write limit, so one core write
            // takes the whole request and no error can follow a written prefix.
            let written = with_space(&mut s.volume, |volume| volume.write(handle, offset, data))
                .map_err(|e| errno(&e))?;
            u32::try_from(written).map_err(|_| Errno::EINVAL)
        }) {
            Ok(written) => reply.written(written),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.with(|s| s.close(fh, false)));
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.with(|s| s.sync_handle(fh, false)));
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.with(|s| s.sync_handle(fh, true)));
    }

    /// Publish for syncfs(2) while the fuseblk device claim is still held,
    /// before replying to the kernel.
    #[cfg(target_os = "linux")]
    fn syncfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyEmpty) {
        reply_empty(reply, self.with(State::sync));
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.reading(|s| {
            let object = id(ino)?;
            let attributes = s.volume.attributes(object).map_err(|e| errno(&e))?;
            if attributes.kind != ObjectKind::Directory {
                return Err(Errno::ENOTDIR);
            }
            let handle = s
                .volume
                .open_object(object, Access::Read)
                .map_err(|e| open_errno(&e))?;
            let fh = s.insert(Open::Directory(Listing::new(
                handle,
                object,
                attributes.parent,
            )));
            #[cfg(not(target_os = "linux"))]
            let flags = FopenFlags::empty();
            #[cfg(target_os = "linux")]
            let flags = s.directory_flags(object, self.attr_ttl);
            Ok((fh, flags))
        }) {
            Ok((fh, flags)) => reply.opened(FileHandle(fh), flags),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        match self.reading(|s| {
            let Some(Open::Directory(listing)) = s.open.get_mut(&u64::from(fh)) else {
                return Err(Errno::EBADF);
            };
            listing
                .fill(
                    &mut s.volume,
                    offset,
                    #[cfg(target_os = "macos")]
                    &mut s.directory_cache,
                    |_, ino, next, kind, name| reply.add(INodeNo(ino), next, kind, name),
                )
                .map_err(|e| errno(&e))
        }) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    /// A listing with each entry's attributes. Linux holds the directory's
    /// shared inode lock through reply processing; namespace mutations take
    /// it exclusively. Buffered associations are rescanned after mutations.
    fn readdirplus(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        match self.reading(|s| {
            #[cfg(target_os = "macos")]
            let directory = match s.open.get(&u64::from(fh)) {
                Some(Open::Directory(listing)) => listing.directory.get(),
                _ => return Err(Errno::EBADF),
            };
            #[cfg(target_os = "macos")]
            let guarded: HashSet<Vec<u8>> = s
                .aliased
                .iter()
                .filter(|((parent, _), since)| {
                    *parent == directory && since.elapsed() < ALIAS_GUARD
                })
                .map(|((_, stored), _)| stored.clone())
                .collect();
            #[cfg(target_os = "linux")]
            let edits = s.edits;
            let Some(Open::Directory(listing)) = s.open.get_mut(&u64::from(fh)) else {
                return Err(Errno::EBADF);
            };
            #[cfg(target_os = "linux")]
            let ahead = Some(std::mem::take(&mut listing.ahead))
                .filter(|ahead| ahead.edits == edits && !listing.refresh);
            let mut failed = None;
            // The cookie of the last entry that fit, and how many did.
            let mut fitted = (0, 0);
            let mut full = false;
            listing
                .fill(
                    &mut s.volume,
                    offset,
                    #[cfg(target_os = "macos")]
                    &mut s.directory_cache,
                    |volume, ino, next, _, name| {
                        #[cfg(target_os = "linux")]
                        let ready = ahead.as_ref().and_then(|ahead| ahead.get(next, ino));
                        #[cfg(not(target_os = "linux"))]
                        let ready = None;
                        let attr = match ready {
                            Some(attr) => Ok(attr),
                            None => ObjectId::new(ino)
                                .and_then(|object| Ok((object, volume.attributes(object)?)))
                                .map_err(|e| errno(&e))
                                .and_then(|(object, attributes)| self.attr(object, &attributes)),
                        };
                        match attr {
                            Ok(attr) => {
                                // iterate_dir holds i_rwsem until fuse_readdir has
                                // linked these names. An alias mutation cannot
                                // overtake this reply in the same directory.
                                #[cfg(target_os = "linux")]
                                let ttl = self.entry_ttl;
                                #[cfg(target_os = "macos")]
                                let ttl = if guarded.contains(name.as_bytes()) {
                                    NO_TTL
                                } else {
                                    self.entry_ttl
                                };
                                full = reply.add_with_ttls(
                                    INodeNo(ino),
                                    next,
                                    name,
                                    &self.attr_ttl,
                                    &ttl,
                                    &attr,
                                    Generation(0),
                                );
                                if !full {
                                    fitted = (next, fitted.1 + 1);
                                }
                                full
                            }
                            Err(error) => {
                                failed = Some(error);
                                true
                            }
                        }
                    },
                )
                .map_err(|e| errno(&e))?;
            failed.map_or(Ok(()), Err)?;
            Ok(full.then_some(fitted))
        }) {
            Ok(resume) => {
                reply.ok();
                #[cfg(target_os = "linux")]
                if let Some((next, count)) = resume {
                    self.read_ahead(fh, next, count);
                }
                #[cfg(not(target_os = "linux"))]
                let _ = resume;
            }
            Err(e) => reply.error(e),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.reading(|s| s.close(fh, true)));
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.reading(|s| s.volume.space().map_err(|e| errno(&e))) {
            // Objects consume metadata blocks, so free blocks bound new objects too.
            Ok(space) => reply.statfs(
                space.total_blocks,
                space.free_blocks,
                space.available_blocks,
                space.total_blocks,
                space.available_blocks,
                space.block_bytes,
                255,
                space.block_bytes,
            ),
            Err(e) => reply.error(e),
        }
    }
}

impl<D: BlockDevice> CarryFs<D> {
    /// After a full plus reply whose last entry got cookie `next`: read the
    /// attributes the next reply will carry, a few more than this one held in
    /// case its names are shorter. The reply is already with the kernel, which
    /// is linking its entries meanwhile.
    #[cfg(target_os = "linux")]
    fn read_ahead(&self, fh: FileHandle, next: u64, count: usize) {
        // Nothing to report: the next request meets any error itself.
        let _ = self.reading(|s| {
            let edits = s.edits;
            if let Some(Open::Directory(listing)) = s.open.get_mut(&u64::from(fh))
                && let Some(first) = next.checked_sub(2)
            {
                listing.prepare(&mut s.volume, first, count + 8, edits, |volume, object| {
                    let attributes = volume.attributes(object).ok()?;
                    self.attr(object, &attributes).ok()
                });
            }
            Ok(())
        });
    }

    fn remove(&self, parent: INodeNo, name: &OsStr, kind: RemoveKind) -> Reply<()> {
        self.with(|s| {
            let (directory, parsed) = (id(parent)?, parse(name)?);
            #[cfg(target_os = "macos")]
            let stored = s.volume.lookup_entry(directory, &parsed).ok();
            #[cfg(target_os = "linux")]
            let result = s.volume.remove_with(directory, &parsed, kind, |stored| {
                self.invalidate_names(&mut s.notifier, name.as_bytes() != stored)
            });
            #[cfg(target_os = "macos")]
            let result = s.volume.remove(directory, &parsed, kind);
            match result {
                Err(Error::WrongType) if kind == RemoveKind::File => return Err(Errno::EISDIR),
                result => result.map_err(|e| errno(&e))?,
            }
            s.changed_directory(directory);
            #[cfg(target_os = "macos")]
            if let Some(entry) = stored {
                self.forget(parent, name, entry.name.stored());
            }
            Ok(())
        })
    }
}

/// Retry an allocating operation once after draining cleanup. Removal
/// reclaims space in bounded steps, so `NoSpace` right after deleting a large
/// file is usually transient: 155 MiB took 81 steps on a 256 MiB volume.
/// The caller holds the state lock, so the whole mount waits for the drain.
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

/// Publish pending changes and run maintenance until no cleanup is left.
fn drain<D: BlockDevice>(volume: &mut Volume<D>) -> Result<(), Error> {
    // The cap only guards against a core that never quiesces.
    for _ in 0..1_000_000 {
        volume.sync()?;
        if !volume.has_cleanup_work()? {
            break;
        }
    }
    Ok(())
}

/// The replacement rule `renameat2` flags ask for. Exchange and whiteout are
/// unsupported, and rename(2) documents `EINVAL` for both.
#[cfg(target_os = "linux")]
fn rename_replace(flags: RenameFlags) -> Reply<Replace> {
    if flags.is_empty() {
        Ok(Replace::Allowed)
    } else if flags == RenameFlags::RENAME_NOREPLACE {
        Ok(Replace::Forbidden)
    } else {
        Err(Errno::EINVAL)
    }
}

/// The replacement rule `renamex_np` flags ask for. Swap is unsupported, and
/// `ENOTSUP` is what renamex_np(2) documents for that, so callers can fall back.
#[cfg(target_os = "macos")]
fn rename_replace(flags: RenameFlags) -> Reply<Replace> {
    if flags.contains(RenameFlags::RENAME_SWAP) {
        Err(Errno::ENOTSUP)
    } else if flags == RenameFlags::RENAME_EXCL {
        Ok(Replace::Forbidden)
    } else if flags.is_empty() {
        Ok(Replace::Allowed)
    } else {
        Err(Errno::EINVAL)
    }
}

fn reply_empty(reply: ReplyEmpty, result: Reply<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(e),
    }
}

fn id(ino: INodeNo) -> Reply<ObjectId> {
    ObjectId::new(u64::from(ino)).map_err(|_| Errno::ENOENT)
}

fn parse(name: &OsStr) -> Reply<Name> {
    let text = name.to_str().ok_or(Errno::EINVAL)?;
    Name::parse(text).map_err(|e| errno(&e))
}

/// The FUSE type and synthetic permissions of an object kind.
fn file_type(kind: ObjectKind) -> (FileType, u16) {
    match kind {
        ObjectKind::File => (FileType::RegularFile, FILE_MODE),
        ObjectKind::Directory => (FileType::Directory, DIRECTORY_MODE),
        ObjectKind::Symlink => (FileType::Symlink, SYMLINK_MODE),
    }
}

/// Map a core error to errno. Damage and device failures are logged here, once.
pub(crate) fn errno(error: &Error) -> Errno {
    match error {
        Error::NotFound => Errno::ENOENT,
        Error::NameCollision => Errno::EEXIST,
        Error::WrongType => Errno::ENOTDIR,
        Error::DirectoryNotEmpty => Errno::ENOTEMPTY,
        Error::ReadOnly => Errno::EROFS,
        Error::NoSpace => Errno::ENOSPC,
        Error::LimitExceeded => Errno::EFBIG,
        // FUSE does not enforce the reported 255-byte name limit, so the core does.
        Error::TooLong(_) => Errno::ENAMETOOLONG,
        Error::InvalidName(_) | Error::InvalidInput(_) | Error::InvalidGeometry => Errno::EINVAL,
        Error::InvalidHandle | Error::DirectoryChanged => Errno::EBADF,
        Error::Busy => Errno::EBUSY,
        Error::Ejected => Errno::ENODEV,
        Error::Faulted => Errno::EIO,
        Error::Io { .. }
        | Error::Corrupt(_)
        | Error::Corruption { .. }
        | Error::UnsupportedFormat => {
            eprintln!("carryfs: {error}");
            Errno::EIO
        }
    }
}

/// Opening only exceeds a limit when the 4,096-handle registry is full, and
/// only a directory refuses write access.
fn open_errno(error: &Error) -> Errno {
    match error {
        Error::LimitExceeded => Errno::EMFILE,
        Error::WrongType => Errno::EISDIR,
        error => errno(error),
    }
}

fn system_time(timestamp: Timestamp) -> Reply<SystemTime> {
    let seconds = Duration::from_secs(timestamp.seconds().unsigned_abs());
    let time = if timestamp.seconds() >= 0 {
        UNIX_EPOCH.checked_add(seconds)
    } else {
        UNIX_EPOCH.checked_sub(seconds)
    };
    time.and_then(|time| time.checked_add(Duration::from_nanos(u64::from(timestamp.nanos()))))
        .ok_or_else(|| errno(&Error::Corrupt("timestamp is outside host time range")))
}

fn timestamp(time: SystemTime) -> Reply<Timestamp> {
    let (seconds, nanos) = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => (
            i64::try_from(after.as_secs()).map_err(|_| Errno::EINVAL)?,
            after.subsec_nanos(),
        ),
        Err(before) => {
            let before = before.duration();
            let seconds = i64::try_from(before.as_secs()).map_err(|_| Errno::EINVAL)?;
            match before.subsec_nanos() {
                0 => (-seconds, 0),
                nanos => (-seconds - 1, 1_000_000_000 - nanos),
            }
        }
    };
    Timestamp::new(seconds, nanos).map_err(|_| Errno::EINVAL)
}

fn time_or_now(time: TimeOrNow) -> Reply<Timestamp> {
    timestamp(match time {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use carryfs_core::{ImageDevice, ROOT};

    struct Image(std::path::PathBuf);

    impl Drop for Image {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn volume(tag: &str) -> (Image, Volume<ImageDevice>) {
        let path =
            std::env::temp_dir().join(format!("carryfs-fuse-{tag}-{}.img", std::process::id()));
        let image = Image(path);
        let device = ImageDevice::create_new(&image.0, 256 << 20).unwrap();
        (image, Volume::format(device).unwrap())
    }

    // A datagram socket drives the real fuser INIT and request dispatcher,
    // without mounting. It proves wire ordering and TTLs, not kernel caching.
    #[cfg(target_os = "linux")]
    struct NameSession {
        client: std::os::unix::net::UnixDatagram,
        init_flags: u32,
        state: Finisher<ImageDevice>,
        runner: Option<std::thread::JoinHandle<io::Result<()>>>,
        _image: Image,
    }

    #[cfg(target_os = "linux")]
    impl NameSession {
        fn new(tag: &str, minor: u32, strict: bool) -> Self {
            let (image, mut volume) = volume(tag);
            for name in ["Foo", "Bar", "Écho"] {
                let handle = volume
                    .create_file(ROOT, Name::parse(name).unwrap())
                    .unwrap();
                volume.close_handle(handle).unwrap();
            }
            volume
                .create_directory(ROOT, Name::parse("Dir").unwrap())
                .unwrap();
            let (fs, _) = CarryFs::new(volume, Owner { uid: 0, gid: 0 }, strict);
            Self::start(image, fs, minor)
        }

        fn start(image: Image, fs: CarryFs<ImageDevice>, minor: u32) -> Self {
            use std::os::fd::OwnedFd;
            use std::os::unix::net::UnixDatagram;
            let state = fs.finisher();
            let (client, server) = UnixDatagram::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let handshake = std::thread::spawn(move || {
                fuser::Session::from_fd(
                    fs,
                    OwnedFd::from(server),
                    fuser::SessionACL::All,
                    fuser::Config::default(),
                )
                .unwrap()
            });
            // fuse_init_in: major, minor, readahead, READDIRPLUS capabilities.
            let mut init = vec![0; 64];
            for (offset, value) in [
                (0, 7u32),
                (4, minor),
                (8, MAX_WRITE),
                (12, (1 << 13) | (1 << 14)),
            ] {
                init[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
            }
            Self::send(&client, 26, 0, &init);
            let response = Self::recv(&client);
            assert_eq!(Self::error(&response), 0);
            let init_flags = u32::from_ne_bytes(response[28..32].try_into().unwrap());
            let session = handshake.join().unwrap();
            state.set_notifier(session.notifier()).unwrap();
            let runner = std::thread::spawn(move || session.run());
            Self {
                client,
                init_flags,
                state,
                runner: Some(runner),
                _image: image,
            }
        }

        fn send(client: &std::os::unix::net::UnixDatagram, opcode: u32, node: u64, body: &[u8]) {
            let mut message = vec![0; 40];
            message[..4].copy_from_slice(&(40u32 + body.len() as u32).to_ne_bytes());
            message[4..8].copy_from_slice(&opcode.to_ne_bytes());
            message[8..16].copy_from_slice(&1u64.to_ne_bytes());
            message[16..24].copy_from_slice(&node.to_ne_bytes());
            message.extend_from_slice(body);
            client.send(&message).unwrap();
        }

        fn recv(client: &std::os::unix::net::UnixDatagram) -> Vec<u8> {
            let mut packet = vec![0; 8192];
            let len = client.recv(&mut packet).unwrap();
            packet.truncate(len);
            packet
        }

        fn error(packet: &[u8]) -> i32 {
            i32::from_ne_bytes(packet[4..8].try_into().unwrap())
        }

        fn call(&self, opcode: u32, body: &[u8]) -> Vec<u8> {
            Self::send(&self.client, opcode, 1, body);
            Self::recv(&self.client)
        }

        fn mutation(&self, opcode: u32, body: &[u8], epoch: bool) {
            let packet = self.call(opcode, body);
            if epoch {
                assert_eq!(packet.len(), 16);
                assert_eq!(Self::error(&packet), 8); // FUSE_NOTIFY_INC_EPOCH
                assert_eq!(&packet[8..16], &[0; 8]); // notification, not request reply
                assert_eq!(Self::error(&Self::recv(&self.client)), 0);
            } else {
                assert_eq!(Self::error(&packet), 0);
            }
        }

        fn rename(&self, from: &str, to: &str, epoch: bool) {
            let mut body = 1u64.to_ne_bytes().to_vec();
            body.extend_from_slice(from.as_bytes());
            body.push(0);
            body.extend_from_slice(to.as_bytes());
            body.push(0);
            self.mutation(12, &body, epoch);
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for NameSession {
        fn drop(&mut self) {
            Self::send(&self.client, 38, 0, &[]); // DESTROY
            let _ = Self::recv(&self.client);
            self.runner.take().unwrap().join().unwrap().unwrap();
        }
    }

    /// A full plus reply leaves the next reply's attributes read ahead. They
    /// serve that reply unless a request changed the volume in between.
    #[cfg(target_os = "linux")]
    #[test]
    fn plus_attributes_read_ahead_are_dropped_after_a_change() {
        let (image, mut volume) = volume("plus-ahead");
        for index in 0..60usize {
            let name = Name::parse(&format!("f{index:02}")).unwrap();
            let handle = volume.create_file(ROOT, name).unwrap();
            volume.write(&handle, 0, &vec![7; index + 1]).unwrap();
            volume.close_handle(handle).unwrap();
        }
        let changed = volume.lookup(ROOT, &Name::parse("f55").unwrap()).unwrap();
        let (fs, _) = CarryFs::new(volume, Owner { uid: 0, gid: 0 }, false);
        let session = NameSession::start(image, fs, 44);
        let open = session.call(27, &[0; 8]); // OPENDIR
        let fh = u64::from_ne_bytes(open[16..24].try_into().unwrap());
        let ahead = || match session.state.0.lock().unwrap().open.get(&fh) {
            Some(Open::Directory(listing)) => listing.ahead.attrs.len(),
            _ => panic!("the directory is open"),
        };
        // Name, size and cookie of each entry in a READDIRPLUS reply from `offset`.
        let page = |offset: u64| {
            let mut body = vec![0; 40]; // fuse_read_in
            body[..8].copy_from_slice(&fh.to_ne_bytes());
            body[8..16].copy_from_slice(&offset.to_ne_bytes());
            body[16..20].copy_from_slice(&4096u32.to_ne_bytes());
            let listing = session.call(44, &body);
            assert_eq!(NameSession::error(&listing), 0);
            let mut entries = Vec::new();
            let mut at = 16;
            while at < listing.len() {
                // fuse_entry_out, whose attributes start at 40, then fuse_dirent.
                let size = u64::from_ne_bytes(listing[at + 48..at + 56].try_into().unwrap());
                let cookie = u64::from_ne_bytes(listing[at + 136..at + 144].try_into().unwrap());
                let len =
                    u32::from_ne_bytes(listing[at + 144..at + 148].try_into().unwrap()) as usize;
                let name = String::from_utf8(listing[at + 152..at + 152 + len].to_vec()).unwrap();
                entries.push((name, size, cookie));
                at += (152 + len + 7) & !7;
            }
            entries
        };
        let mut listed = page(0);
        // The session reads ahead after it replies; a later reply follows it.
        let barrier = || assert_eq!(NameSession::error(&session.call(3, &[0; 16])), 0);
        barrier();
        assert!(ahead() > 0, "a full reply reads the next one's attributes");

        // Unchanged, the next reply is served from them and reads ahead again.
        let next = page(listed.last().unwrap().2);
        assert!(!next.is_empty());
        listed.extend(next);
        barrier();
        assert!(ahead() > 0);
        assert!(listed.iter().all(|(name, _, _)| name != "f55"));

        // Grow a file the attributes read ahead cover: SETATTR with FATTR_SIZE.
        let mut setattr = vec![0; 88];
        setattr[..4].copy_from_slice(&(1u32 << 3).to_ne_bytes());
        setattr[16..24].copy_from_slice(&7777u64.to_ne_bytes());
        NameSession::send(&session.client, 4, changed.get(), &setattr);
        assert_eq!(NameSession::error(&NameSession::recv(&session.client)), 0);
        loop {
            let next = page(listed.last().unwrap().2);
            if next.is_empty() {
                break;
            }
            listed.extend(next);
        }
        let files: Vec<_> = listed
            .iter()
            .filter(|(name, _, _)| name.starts_with('f'))
            .collect();
        assert_eq!(files.len(), 60);
        for (index, (name, size, _)) in files.into_iter().enumerate() {
            assert_eq!(name, &format!("f{index:02}"));
            let expected = if index == 55 { 7777 } else { index as u64 + 1 };
            assert_eq!(*size, expected, "{name}");
        }
        let mut release = vec![0; 24];
        release[..8].copy_from_slice(&fh.to_ne_bytes());
        assert_eq!(NameSession::error(&session.call(29, &release)), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn epoch_precedes_alias_mutation_replies() {
        let session = NameSession::new("epoch-wire", 44, false);
        let entry = session.call(1, b"Foo\0");
        assert_eq!(u64::from_ne_bytes(entry[32..40].try_into().unwrap()), 3600);
        let alias = session.call(1, b"FOO\0");
        assert_eq!(u64::from_ne_bytes(alias[32..40].try_into().unwrap()), 0);
        session.rename("FOO", "BAR", true);
        assert_eq!(&session.call(1, b"BAR\0")[16..24], &entry[16..24]);
        assert_eq!(
            NameSession::error(&session.call(1, b"Foo\0")),
            -libc::ENOENT
        );
        session.mutation(10, "éCHO\0".as_bytes(), true);
        session.mutation(11, b"DIR\0", true);
        session.mutation(10, b"BAR\0", false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn created_names_outlive_their_attributes() {
        let session = NameSession::new("create-ttl", 44, false);
        let mut body = vec![0; 16]; // fuse_create_in: flags, mode, umask, open_flags
        body[..4].copy_from_slice(&(libc::O_WRONLY as u32).to_ne_bytes());
        body[4..8].copy_from_slice(&(libc::S_IFREG | 0o644).to_ne_bytes());
        body.extend_from_slice(b"New\0");
        let created = session.call(35, &body); // CREATE
        assert_eq!(NameSession::error(&created), 0);
        assert_eq!(
            u64::from_ne_bytes(created[32..40].try_into().unwrap()),
            3600
        );
        assert_eq!(u64::from_ne_bytes(created[40..48].try_into().unwrap()), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn older_abi_and_strict_have_zero_name_ttl_and_no_notifications() {
        for (tag, minor, strict) in [("old-abi", 43, false), ("strict-abi", 44, true)] {
            let session = NameSession::new(tag, minor, strict);
            let entry = session.call(1, b"Foo\0");
            assert_eq!(u64::from_ne_bytes(entry[32..40].try_into().unwrap()), 0);
            session.rename("FOO", "BAR", false);
            session.mutation(11, b"DIR\0", false);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn normalized_rename_destination_invalidates_names() {
        let session = NameSession::new("normalized-rename", 44, false);
        // Only the destination's NFC conversion requires invalidating the
        // spelling the VFS moves; the source uses its stored spelling.
        session.rename("Écho", "e\u{301}clair", true);
        let entry = session.call(1, "éclair\0".as_bytes());
        assert_eq!(NameSession::error(&entry), 0);
        assert_eq!(u64::from_ne_bytes(entry[32..40].try_into().unwrap()), 3600);
        session.mutation(10, "ÉCLAIR\0".as_bytes(), true);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn plus_entries_cache_names_only_when_epochs_are_supported_and_not_strict() {
        for (tag, minor, strict, ttl) in [
            ("plus-ttl", 44, false, 3600u64),
            ("plus-old", 43, false, 0),
            ("plus-strict", 44, true, 0),
        ] {
            let session = NameSession::new(tag, minor, strict);
            let expected_auto = strict || minor < 44;
            assert_eq!(
                session.init_flags & ((1 << 13) | (1 << 14)),
                (1 << 13) | if expected_auto { 1 << 14 } else { 0 }
            );
            let open = session.call(27, &[0; 8]); // OPENDIR
            assert_eq!(NameSession::error(&open), 0);
            assert_eq!(
                &open[24..28],
                &FopenFlags::FOPEN_CACHE_DIR.bits().to_ne_bytes()
            );
            let mut body = vec![0; 40]; // fuse_read_in
            body[..8].copy_from_slice(&open[16..24]);
            body[16..20].copy_from_slice(&4096u32.to_ne_bytes());
            let listing = session.call(44, &body); // READDIRPLUS
            assert_eq!(NameSession::error(&listing), 0);
            let mut at = 16;
            let mut count = 0;
            while at < listing.len() {
                // fuse_entry_out is 128 bytes, followed by fuse_dirent and name.
                // Name lifetime, then attribute lifetime, in seconds.
                assert_eq!(&listing[at + 16..at + 24], &ttl.to_ne_bytes());
                assert_eq!(&listing[at + 24..at + 32], &1u64.to_ne_bytes());
                assert_eq!(&listing[at + 32..at + 40], &[0; 8]);
                let len = u32::from_ne_bytes(listing[at + 144..at + 148].try_into().unwrap());
                at += (152 + len as usize + 7) & !7;
                count += 1;
            }
            assert_eq!(at, listing.len());
            assert!(count >= 4);
            let mut release = vec![0; 24];
            release[..8].copy_from_slice(&open[16..24]);
            assert_eq!(NameSession::error(&session.call(29, &release)), 0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cached_readonly_mount_retains_kernel_entries_names_and_attributes() {
        for (tag, cached) in [
            ("readonly-directory-fresh", false),
            ("readonly-directory-cache", true),
        ] {
            let (image, mut writer) = volume(tag);
            let file = writer
                .create_file(ROOT, Name::parse("Foo").unwrap())
                .unwrap();
            writer.close_handle(file).unwrap();
            writer.eject().unwrap();
            let mut reader = Volume::open(
                ImageDevice::open(&image.0, carryfs_core::MountMode::ReadOnly).unwrap(),
            )
            .unwrap();
            if cached {
                reader.enable_readonly_metadata_cache().unwrap();
            }
            let (mut fs, _) = CarryFs::new(reader, Owner { uid: 0, gid: 0 }, false);
            if cached {
                fs.set_read_only_cache();
            }
            let session = NameSession::start(image, fs, 44);
            let open = session.call(27, &[0; 8]);
            assert_eq!(NameSession::error(&open), 0);
            let expected = if cached {
                FopenFlags::FOPEN_CACHE_DIR | FopenFlags::FOPEN_KEEP_CACHE
            } else {
                FopenFlags::FOPEN_CACHE_DIR
            };
            assert_eq!(&open[24..28], &expected.bits().to_ne_bytes());
            let mut release = vec![0; 24];
            release[..8].copy_from_slice(&open[16..24]);
            assert_eq!(NameSession::error(&session.call(29, &release)), 0);
            // Entry and attribute lifetimes in seconds; another spelling stays uncached.
            let ttl = if cached { 3600 } else { 1 };
            for (name, entry_ttl) in [(&b"Foo\0"[..], 3600), (b"FOO\0", 0)] {
                let entry = session.call(1, name); // LOOKUP
                assert_eq!(NameSession::error(&entry), 0);
                assert_eq!(
                    u64::from_ne_bytes(entry[32..40].try_into().unwrap()),
                    entry_ttl
                );
                assert_eq!(u64::from_ne_bytes(entry[40..48].try_into().unwrap()), ttl);
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_directory_replay_ends_when_plus_attributes_expire() {
        let session = NameSession::new("directory-expiry", 44, false);
        let cached = FopenFlags::FOPEN_CACHE_DIR;
        for (pass, flags) in [cached, cached | FopenFlags::FOPEN_KEEP_CACHE, cached]
            .into_iter()
            .enumerate()
        {
            if pass == 2 {
                session
                    .state
                    .0
                    .lock()
                    .unwrap()
                    .listed
                    .insert(ROOT.get(), Instant::now() - Duration::from_secs(2));
            }
            let open = session.call(27, &[0; 8]);
            assert_eq!(NameSession::error(&open), 0);
            assert_eq!(&open[24..28], &flags.bits().to_ne_bytes());
            let mut release = vec![0; 24];
            release[..8].copy_from_slice(&open[16..24]);
            assert_eq!(NameSession::error(&session.call(29, &release)), 0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires /dev/fuse and fusermount3"]
    fn kernel_directory_cache_survives_reopen() {
        for (read_only, strict) in [(true, false), (false, false), (false, true)] {
            let tag = format!("kernel-directory-{read_only}-{strict}");
            let (image, mut writer) = volume(&tag);
            writer.set_deferred_checkpoints(true).unwrap();
            let mut expected: Vec<_> = (0..130).map(|index| format!("f{index:03}")).collect();
            expected.push("Écho".to_owned());
            for name in &expected {
                let file = writer
                    .create_file(ROOT, Name::parse(name).unwrap())
                    .unwrap();
                writer.close_handle(file).unwrap();
            }
            writer.eject().unwrap();
            let mode = if read_only {
                carryfs_core::MountMode::ReadOnly
            } else {
                carryfs_core::MountMode::ReadWrite
            };
            let mut volume = Volume::open(ImageDevice::open(&image.0, mode).unwrap()).unwrap();
            if read_only {
                volume.enable_readonly_metadata_cache().unwrap();
            } else {
                volume.set_deferred_checkpoints(true).unwrap();
            }
            let (mut fs, _) = CarryFs::new(volume, Owner { uid: 0, gid: 0 }, strict);
            if read_only {
                fs.set_read_only_cache();
            }
            let state = fs.finisher();
            let mountpoint = image.0.with_extension("mnt");
            std::fs::create_dir(&mountpoint).unwrap();
            let mut config = fuser::Config::default();
            config.mount_options = vec![fuser::MountOption::DefaultPermissions];
            if read_only {
                config.mount_options.push(fuser::MountOption::RO);
            }
            let session = fuser::Session::new(fs, &mountpoint, &config).unwrap();
            state.set_notifier(session.notifier()).unwrap();
            let background = session.spawn().unwrap();
            for pass in 0..3 {
                let mut directory = std::fs::read_dir(&mountpoint).unwrap();
                let mut names: Vec<_> = directory
                    .by_ref()
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect();
                names.sort();
                assert_eq!(names, expected);
                // A kernel replay leaves the new handle's daemon buffer empty.
                let locked = state.0.lock().unwrap();
                let mut listings = locked.open.values().filter_map(|open| match open {
                    Open::Directory(listing) => Some(listing),
                    _ => None,
                });
                let listing = listings.next().unwrap();
                assert!(listings.next().is_none());
                assert_eq!(
                    listing.entries.is_empty(),
                    pass == 1 || read_only && pass == 2
                );
                drop(locked);
                drop(directory);
                if pass == 1 {
                    std::thread::sleep(Duration::from_millis(1100));
                }
            }
            use std::os::unix::fs::MetadataExt;
            for (stored, alias) in [("f000", "F000"), ("Écho", "e\u{301}CHO")] {
                assert_eq!(
                    std::fs::metadata(mountpoint.join(stored)).unwrap().ino(),
                    std::fs::metadata(mountpoint.join(alias)).unwrap().ino(),
                );
            }
            if !read_only {
                let names = || {
                    let mut names: Vec<_> = std::fs::read_dir(&mountpoint)
                        .unwrap()
                        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                        .collect();
                    names.sort();
                    names
                };
                let source = mountpoint.join("f000");
                std::fs::write(&source, b"replacement").unwrap();
                assert_eq!(std::fs::metadata(&source).unwrap().len(), 11);
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&source)
                    .unwrap()
                    .set_len(3)
                    .unwrap();
                assert_eq!(std::fs::metadata(&source).unwrap().len(), 3);
                // Warm listings must lose replaced entries even through case aliases.
                std::fs::rename(mountpoint.join("F000"), mountpoint.join("F001")).unwrap();
                expected.retain(|name| name != "f000" && name != "f001");
                expected.push("F001".to_owned());
                expected.sort();
                assert_eq!(names(), expected);
                assert_eq!(std::fs::read(mountpoint.join("F001")).unwrap(), b"rep");
                std::fs::remove_file(mountpoint.join("e\u{301}CHO")).unwrap();
                expected.retain(|name| name != "Écho");
                assert_eq!(names(), expected);
                let directory = mountpoint.join("New");
                std::fs::create_dir(&directory).unwrap();
                expected.push("New".to_owned());
                expected.sort();
                assert_eq!(names(), expected);
                std::fs::write(directory.join("child"), b"child").unwrap();
                assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
                std::fs::rename(directory.join("child"), mountpoint.join("child")).unwrap();
                assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
                expected.push("child".to_owned());
                expected.sort();
                assert_eq!(names(), expected);
                std::fs::remove_dir(mountpoint.join("NEW")).unwrap();
                expected.retain(|name| name != "New");
                assert_eq!(names(), expected);
                std::fs::write(mountpoint.join("created"), b"new").unwrap();
                expected.push("created".to_owned());
                expected.sort();
                assert_eq!(names(), expected);
                std::fs::remove_file(mountpoint.join("created")).unwrap();
                expected.retain(|name| name != "created");
                assert_eq!(names(), expected);
            }
            background.umount_and_join().unwrap();
            std::fs::remove_dir(mountpoint).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn notification_failure_leaves_names_unchanged() {
        let session = NameSession::new("failed-epoch", 44, false);
        // A notifier from an older connection deterministically returns ENOTSUP.
        let older = NameSession::new("failed-epoch-old", 43, false);
        let notifier = older.state.0.lock().unwrap().notifier.clone().unwrap();
        session.state.set_notifier(notifier).unwrap();
        assert_eq!(NameSession::error(&session.call(10, b"FOO\0")), -libc::EIO);
        assert_eq!(NameSession::error(&session.call(1, b"Foo\0")), 0);
        assert!(session.state.0.lock().unwrap().notifier.is_none());
        assert_eq!(NameSession::error(&session.call(11, b"DIR\0")), -libc::EIO);
        assert_eq!(NameSession::error(&session.call(1, b"Dir\0")), 0);
        let mut rename = 1u64.to_ne_bytes().to_vec();
        rename.extend_from_slice(b"FOO\0Bar\0");
        assert_eq!(NameSession::error(&session.call(12, &rename)), -libc::EIO);
        assert_eq!(NameSession::error(&session.call(1, b"Foo\0")), 0);
        assert_eq!(NameSession::error(&session.call(1, b"Bar\0")), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn trash_folders_report_the_owner_and_mode_macos_expects() {
        let (_image, volume) = volume("trash");
        let owner = Owner { uid: 501, gid: 20 };
        let (fs, _forgotten) = CarryFs::new(volume, owner, false);
        let mut s = fs.state.lock().unwrap();
        let folder = |s: &mut State<ImageDevice>, parent, name: &str| {
            let object = s
                .volume
                .create_directory(parent, Name::parse(name).unwrap())
                .unwrap();
            s.note_trash(parent, name.as_bytes(), object);
            let attr = s.entry(&fs, object).unwrap();
            (object, (attr.uid, attr.gid, attr.perm))
        };
        let (trashes, shown) = folder(&mut s, ROOT, ".Trashes");
        assert_eq!(shown, (0, 0, 0o1311));
        assert_eq!(folder(&mut s, trashes, "502").1, (502, 20, 0o700));
        // Only numeric names under `.Trashes`, and `.Trashes` only at the top.
        assert_eq!(
            folder(&mut s, trashes, "notes").1,
            (501, 20, DIRECTORY_MODE)
        );
        let (other, _) = folder(&mut s, ROOT, "other");
        assert_eq!(
            folder(&mut s, other, ".Trashes").1,
            (501, 20, DIRECTORY_MODE)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn renameat2_flags_map_to_replacement_rules() {
        let noreplace = RenameFlags::RENAME_NOREPLACE;
        assert_eq!(rename_replace(RenameFlags::empty()), Ok(Replace::Allowed));
        assert_eq!(rename_replace(noreplace), Ok(Replace::Forbidden));
        for flags in [
            RenameFlags::RENAME_EXCHANGE,
            RenameFlags::RENAME_WHITEOUT,
            noreplace | RenameFlags::RENAME_WHITEOUT,
        ] {
            assert_eq!(rename_replace(flags), Err(Errno::EINVAL));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn renamex_flags_map_to_replacement_rules() {
        let (swap, excl) = (RenameFlags::RENAME_SWAP, RenameFlags::RENAME_EXCL);
        assert_eq!(rename_replace(RenameFlags::empty()), Ok(Replace::Allowed));
        assert_eq!(rename_replace(excl), Ok(Replace::Forbidden));
        assert_eq!(rename_replace(swap), Err(Errno::ENOTSUP));
        assert_eq!(rename_replace(swap | excl), Err(Errno::ENOTSUP));
        assert_eq!(
            rename_replace(RenameFlags::from_bits_retain(0x8)),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn sync_validates_handles_and_propagates_publication_failure() {
        struct Device {
            inner: ImageDevice,
            fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl BlockDevice for Device {
            fn mode(&self) -> carryfs_core::MountMode {
                self.inner.mode()
            }
            fn geometry(&mut self) -> carryfs_core::Result<carryfs_core::Geometry> {
                self.inner.geometry()
            }
            fn read_exact_at(&mut self, at: u64, out: &mut [u8]) -> carryfs_core::Result<()> {
                self.inner.read_exact_at(at, out)
            }
            fn write_all_at(&mut self, at: u64, data: &[u8]) -> carryfs_core::Result<()> {
                self.inner.write_all_at(at, data)
            }
            fn flush_durable(&mut self) -> carryfs_core::Result<()> {
                if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(Error::Io {
                        operation: "test flush",
                        source: std::io::Error::other("injected flush failure"),
                    });
                }
                self.inner.flush_durable()
            }
        }
        let (image, mut volume) = volume("sync-handles");
        volume.eject().unwrap();
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut volume = Volume::open(Device {
            inner: ImageDevice::open(&image.0, carryfs_core::MountMode::ReadWrite).unwrap(),
            fail: Arc::clone(&fail),
        })
        .unwrap();
        volume.set_deferred_checkpoints(true).unwrap();
        let (fs, _forgotten) = CarryFs::new(volume, Owner { uid: 0, gid: 0 }, false);
        let mut s = fs.state.lock().unwrap();
        let root = s.volume.open_directory(ROOT).unwrap();
        let dir = FileHandle(s.insert(Open::Directory(Listing::new(root, ROOT, ROOT))));
        let file = s
            .volume
            .create_file(ROOT, Name::parse("file").unwrap())
            .unwrap();
        let object = s
            .volume
            .lookup(ROOT, &Name::parse("file").unwrap())
            .unwrap();
        let file = FileHandle(s.insert(Open::File(object, file)));
        assert_eq!(s.sync_handle(FileHandle(u64::MAX), true), Err(Errno::EBADF));
        assert_eq!(s.sync_handle(file, true), Err(Errno::EBADF));
        assert_eq!(s.sync_handle(dir, false), Err(Errno::EBADF));
        assert!(s.volume.has_unsynced_changes());
        s.sync_handle(dir, true).unwrap();
        assert!(!s.volume.has_unsynced_changes());
        s.volume
            .create_directory(ROOT, Name::parse("next").unwrap())
            .unwrap();
        s.sync_handle(file, false).unwrap();
        assert!(!s.volume.has_unsynced_changes());
        #[cfg(target_os = "macos")]
        s.cached_attributes(ROOT).unwrap();
        s.volume
            .create_directory(ROOT, Name::parse("failed").unwrap())
            .unwrap();
        #[cfg(target_os = "macos")]
        let mut listing = {
            let handle = s.volume.open_directory(ROOT).unwrap();
            let mut listing = Listing::new(handle, ROOT, ROOT);
            let State {
                volume,
                directory_cache,
                ..
            } = &mut *s;
            listing
                .fill(volume, 0, directory_cache, |_, _, _, _, _| false)
                .unwrap();
            listing
        };
        fail.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(s.sync_handle(dir, true), Err(Errno::EIO));
        assert_eq!(s.sync_handle(file, false), Err(Errno::EIO));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(s.cached_attributes(ROOT), Err(Errno::EIO));
            let State {
                volume,
                directory_cache,
                ..
            } = &mut *s;
            assert!(matches!(
                listing.fill(volume, 0, directory_cache, |_, _, _, _, _| false),
                Err(Error::Faulted)
            ));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn strict_names_have_no_entry_lifetime_or_alias_tracking() {
        let owner = Owner { uid: 0, gid: 0 };
        let (_strict_image, strict_volume) = volume("strict-names");
        let (fs, forgotten) = CarryFs::new(strict_volume, owner, true);
        let mut state = fs.state.lock().unwrap();
        assert_eq!(
            fs.lookup_ttl(&mut state, INodeNo(1), OsStr::new("Foo"), b"Foo"),
            NO_TTL
        );
        assert_eq!(
            fs.lookup_ttl(&mut state, INodeNo(1), OsStr::new("FOO"), b"Foo"),
            NO_TTL
        );
        assert!(state.aliased.is_empty());
        assert!(forgotten.try_recv().is_err());
        assert_eq!(name_ttl(OsStr::new("Foo"), b"Foo", fs.entry_ttl), NO_TTL);

        let (_cached_image, cached_volume) = volume("cached-names");
        let (fs, forgotten) = CarryFs::new(cached_volume, owner, false);
        let mut state = fs.state.lock().unwrap();
        assert_eq!(
            fs.lookup_ttl(&mut state, INodeNo(1), OsStr::new("Foo"), b"Foo"),
            ENTRY_TTL
        );
        assert_eq!(
            fs.lookup_ttl(&mut state, INodeNo(1), OsStr::new("FOO"), b"Foo"),
            NO_TTL
        );
        assert_eq!(
            fs.lookup_ttl(&mut state, INodeNo(1), OsStr::new("Foo"), b"Foo"),
            NO_TTL
        );
        assert_eq!(
            forgotten.try_recv().unwrap(),
            (INodeNo(1), OsString::from("Foo"))
        );
    }

    /// Drive `fill` like the kernel: fixed-size buffers, resuming at the last
    /// offset, optionally removing each listed entry before the next call.
    fn list(volume: &mut Volume<ImageDevice>, batch: usize, remove: bool) -> Vec<String> {
        let handle = volume.open_object(ROOT, Access::Read).unwrap();
        let mut listing = Listing::new(handle, ROOT, ROOT);
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            let mut batch_names = Vec::new();
            listing
                .fill(
                    volume,
                    offset,
                    #[cfg(target_os = "macos")]
                    &mut None,
                    |_, _, next, _, name| {
                        if batch_names.len() == batch {
                            return true;
                        }
                        batch_names.push((next, name.to_str().unwrap().to_string()));
                        false
                    },
                )
                .unwrap();
            let Some(&(last, _)) = batch_names.last() else {
                break;
            };
            offset = last;
            for (_, name) in batch_names {
                if remove && !name.starts_with('.') {
                    volume
                        .remove(ROOT, &Name::parse(&name).unwrap(), RemoveKind::Directory)
                        .unwrap();
                    listing.refresh = true;
                }
                names.push(name);
            }
        }
        volume.close_handle(listing.handle).unwrap();
        names
    }

    #[test]
    fn listing_resumes_by_offset_while_entries_are_removed() {
        let (_image, mut volume) = volume("listing");
        let expected: Vec<String> = [".".to_string(), "..".to_string()]
            .into_iter()
            .chain((0..150).map(|index| format!("d{index:03}")))
            .collect();
        for name in &expected[2..] {
            volume
                .create_directory(ROOT, Name::parse(name).unwrap())
                .unwrap();
        }
        assert_eq!(list(&mut volume, 7, false), expected);
        assert_eq!(list(&mut volume, 100, true), expected);
        assert_eq!(list(&mut volume, 5, false), [".", ".."]);
    }

    #[test]
    fn listing_seeks_back_before_the_buffered_page() {
        let (_image, mut volume) = volume("seek");
        for index in 0..70 {
            volume
                .create_directory(ROOT, Name::parse(&format!("d{index:02}")).unwrap())
                .unwrap();
        }
        let handle = volume.open_object(ROOT, Access::Read).unwrap();
        let mut listing = Listing::new(handle, ROOT, ROOT);
        let mut first = None;
        let mut seen = 0;
        // Consume past the first 63-entry page, then seek back to offset 3.
        listing
            .fill(
                &mut volume,
                67,
                #[cfg(target_os = "macos")]
                &mut None,
                |_, _, _, _, name| {
                    first.get_or_insert_with(|| name.to_str().unwrap().to_string());
                    seen += 1;
                    false
                },
            )
            .unwrap();
        assert_eq!((first.as_deref(), seen), (Some("d65"), 5));
        let mut again = None;
        listing
            .fill(
                &mut volume,
                3,
                #[cfg(target_os = "macos")]
                &mut None,
                |_, _, next, _, name| {
                    again = Some((next, name.to_str().unwrap().to_string()));
                    true
                },
            )
            .unwrap();
        assert_eq!(again, Some((4, "d01".to_string())));
        volume.close_handle(listing.handle).unwrap();
    }

    #[test]
    fn a_seek_back_among_kept_entries_survives_earlier_removals() {
        let (_image, mut volume) = volume("seek-kept");
        for index in 0..300 {
            volume
                .create_directory(ROOT, Name::parse(&format!("d{index:03}")).unwrap())
                .unwrap();
        }
        let handle = volume.open_object(ROOT, Access::Read).unwrap();
        let mut listing = Listing::new(handle, ROOT, ROOT);
        listing
            .fill(
                &mut volume,
                0,
                #[cfg(target_os = "macos")]
                &mut None,
                |_, _, _, _, _| false,
            )
            .unwrap();
        for index in 0..10 {
            let name = Name::parse(&format!("d{index:03}")).unwrap();
            volume.remove(ROOT, &name, RemoveKind::Directory).unwrap();
        }
        listing.refresh = true;
        let mut at = |volume: &mut Volume<ImageDevice>, offset| {
            let mut found = None;
            listing
                .fill(
                    volume,
                    offset,
                    #[cfg(target_os = "macos")]
                    &mut None,
                    |_, _, _, _, name| {
                        found = Some(name.to_str().unwrap().to_string());
                        true
                    },
                )
                .unwrap();
            found
        };
        // Offset 3 + n resumes after the n-th entry listed, d{n}, as it did
        // before the removals.
        assert_eq!(at(&mut volume, 3 + 199).as_deref(), Some("d200"));
        // Past the kept entries, the offset names what is at that position now.
        assert_eq!(at(&mut volume, 3 + 9).as_deref(), Some("d020"));
        volume.close_handle(listing.handle).unwrap();
    }

    #[test]
    fn buffered_listing_rescans_moved_and_replaced_entries_before_caching() {
        let (_image, mut volume) = volume("listing-changed");
        let mut objects = Vec::new();
        for name in ["a", "b", "c", "d"] {
            objects.push(
                volume
                    .create_directory(ROOT, Name::parse(name).unwrap())
                    .unwrap(),
            );
        }
        let handle = volume.open_object(ROOT, Access::Read).unwrap();
        let (fs, _) = CarryFs::new(volume, Owner { uid: 0, gid: 0 }, false);
        let mut s = fs.state.lock().unwrap();
        let fh = s.insert(Open::Directory(Listing::new(handle, ROOT, ROOT)));
        let State { volume, open, .. } = &mut *s;
        let Some(Open::Directory(listing)) = open.get_mut(&fh) else {
            panic!("directory")
        };
        // Emit only a, leaving b, c, and d buffered from the same core page.
        listing
            .fill(
                volume,
                0,
                #[cfg(target_os = "macos")]
                &mut None,
                |_, _, next, _, _| next > 3,
            )
            .unwrap();
        volume
            .rename(
                ROOT,
                &Name::parse("b").unwrap(),
                ROOT,
                Name::parse("z").unwrap(),
                Replace::Allowed,
            )
            .unwrap();
        volume
            .rename(
                ROOT,
                &Name::parse("d").unwrap(),
                ROOT,
                Name::parse("C").unwrap(),
                Replace::Allowed,
            )
            .unwrap();
        s.changed_directory(ROOT);
        let State { volume, open, .. } = &mut *s;
        let Some(Open::Directory(listing)) = open.get_mut(&fh) else {
            panic!("directory")
        };
        let mut found = Vec::new();
        listing
            .fill(
                volume,
                3,
                #[cfg(target_os = "macos")]
                &mut None,
                |_, ino, _, _, name| {
                    found.push((name.to_str().unwrap().to_owned(), ino));
                    false
                },
            )
            .unwrap();
        assert_eq!(
            found,
            [
                ("C".to_owned(), objects[3].get()),
                ("z".to_owned(), objects[1].get())
            ]
        );
        s.close(FileHandle(fh), true).unwrap();
    }

    #[test]
    fn listing_from_offset_zero_shows_entries_added_since() {
        let (_image, mut volume) = volume("rewind");
        let handle = volume.open_object(ROOT, Access::Read).unwrap();
        let mut listing = Listing::new(handle, ROOT, ROOT);
        let names = |volume: &mut Volume<ImageDevice>, listing: &mut Listing| {
            let mut names = Vec::new();
            listing
                .fill(
                    volume,
                    0,
                    #[cfg(target_os = "macos")]
                    &mut None,
                    |_, _, _, _, name| {
                        names.push(name.to_str().unwrap().to_string());
                        false
                    },
                )
                .unwrap();
            names
        };
        assert_eq!(names(&mut volume, &mut listing), [".", ".."]);
        volume
            .create_directory(ROOT, Name::parse("new").unwrap())
            .unwrap();
        assert_eq!(names(&mut volume, &mut listing), [".", "..", "new"]);
        volume.close_handle(listing.handle).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn readonly_entries_use_the_existing_metadata_cache() {
        let (image, mut writer) = volume("readonly-attributes");
        writer.eject().unwrap();
        let mut reader =
            Volume::open(ImageDevice::open(&image.0, carryfs_core::MountMode::ReadOnly).unwrap())
                .unwrap();
        reader.enable_readonly_metadata_cache().unwrap();
        let (mut fs, _) = CarryFs::new(reader, Owner { uid: 0, gid: 0 }, false);
        fs.set_read_only_cache();
        fs.with(|state| {
            state.entry(&fs, ROOT)?;
            assert!(state.attributes.1.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cached_attributes_follow_pending_edits_and_publication() {
        let (_image, mut volume) = volume("attribute-cache");
        volume.set_deferred_checkpoints(true).unwrap();
        let name = Name::parse("file").unwrap();
        let file = volume.create_file(ROOT, name.clone()).unwrap();
        let object = volume.lookup(ROOT, &name).unwrap();
        volume.sync_publication().unwrap();
        let mut state = State::new(volume);
        let initial = state.cached_attributes(object).unwrap();
        assert_eq!(initial.size, 0);
        assert_eq!(state.attributes.1.len(), 1);
        assert_eq!(state.cached_attributes(object).unwrap(), initial);

        state.volume.write(&file, 0, b"changed").unwrap();
        assert!(state.volume.has_unsynced_changes());
        assert_eq!(state.cached_attributes(object).unwrap().size, 7);
        assert_eq!(state.attributes.1[&object.get()], initial);
        state.volume.truncate(&file, 2).unwrap();
        let stamp = Timestamp::new(1_000_000, 0).unwrap();
        state
            .volume
            .set_times(&file, Some(stamp), Some(stamp))
            .unwrap();
        state.volume.sync_publication().unwrap();
        let changed = state.cached_attributes(object).unwrap();
        assert_eq!(changed.size, 2);
        assert_eq!(changed.creation, stamp);
        assert_eq!(changed.modification, stamp);
        assert_eq!(state.attributes.1[&object.get()], changed);

        let directory = state
            .volume
            .create_directory(ROOT, Name::parse("dir").unwrap())
            .unwrap();
        state
            .volume
            .rename(ROOT, &name, directory, name.clone(), Replace::Forbidden)
            .unwrap();
        assert_eq!(state.cached_attributes(object).unwrap().parent, directory);
        state.volume.sync_publication().unwrap();
        assert_eq!(state.cached_attributes(object).unwrap().parent, directory);
        state
            .volume
            .remove(directory, &name, RemoveKind::File)
            .unwrap();
        assert!(state.cached_attributes(object).unwrap().orphan);
        state.volume.sync_publication().unwrap();
        assert!(state.cached_attributes(object).unwrap().orphan);
        state.volume.close_handle(file).unwrap();
        for _ in 0..8 {
            state.volume.sync().unwrap();
        }
        assert_eq!(state.cached_attributes(object), Err(Errno::ENOENT));

        let root = state.cached_attributes(ROOT).unwrap();
        state.attributes.1 = (1..=DIRECTORY_CACHE_ENTRIES as u64)
            .map(|id| (id + 10_000, root))
            .collect();
        assert_eq!(state.cached_attributes(ROOT).unwrap(), root);
        assert_eq!(state.attributes.1.len(), 1);
        state.volume.eject().unwrap();
        assert_eq!(state.cached_attributes(ROOT), Err(Errno::ENODEV));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cached_directory_rebuild_preserves_a_removed_resume_marker() {
        let (_image, mut volume) = volume("directory-cache-marker");
        for name in ["a", "b", "c", "d"] {
            volume
                .create_directory(ROOT, Name::parse(name).unwrap())
                .unwrap();
        }
        let mut state = State::new(volume);
        let handle = state.volume.open_directory(ROOT).unwrap();
        let fh = state.insert(Open::Directory(Listing::new(handle, ROOT, ROOT)));
        let State {
            volume,
            open,
            directory_cache,
            ..
        } = &mut state;
        let Some(Open::Directory(listing)) = open.get_mut(&fh) else {
            panic!("directory")
        };
        listing
            .fill(volume, 0, directory_cache, |_, _, next, _, _| next > 3)
            .unwrap();
        state
            .volume
            .remove(ROOT, &Name::parse("a").unwrap(), RemoveKind::Directory)
            .unwrap();
        state.changed_directory(ROOT);
        let second = state.volume.open_directory(ROOT).unwrap();
        let mut second = Listing::new(second, ROOT, ROOT);
        second
            .fill(
                &mut state.volume,
                0,
                &mut state.directory_cache,
                |_, _, _, _, _| false,
            )
            .unwrap();
        let State {
            volume,
            open,
            directory_cache,
            ..
        } = &mut state;
        let Some(Open::Directory(listing)) = open.get_mut(&fh) else {
            panic!("directory")
        };
        let mut names = Vec::new();
        listing
            .fill(volume, 3, directory_cache, |_, _, _, _, name| {
                names.push(name.to_str().unwrap().to_owned());
                false
            })
            .unwrap();
        assert_eq!(names, ["b", "c", "d"]);
        state.close(FileHandle(fh), true).unwrap();
        state.volume.close_handle(second.handle).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cached_directory_prefix_is_bounded_and_large_repeats_are_complete() {
        let (_image, mut volume) = volume("directory-cache-bound");
        volume.set_deferred_checkpoints(true).unwrap();
        let count = DIRECTORY_CACHE_ENTRIES + 7;
        for index in 0..count {
            volume
                .create_directory(ROOT, Name::parse(&format!("d{index:05}")).unwrap())
                .unwrap();
        }
        let handle = volume.open_directory(ROOT).unwrap();
        let mut listing = Listing::new(handle, ROOT, ROOT);
        let mut cache = None;
        for _ in 0..2 {
            let mut names = Vec::new();
            listing
                .fill(&mut volume, 0, &mut cache, |_, _, _, _, name| {
                    names.push(name.to_str().unwrap().to_owned());
                    false
                })
                .unwrap();
            let expected: Vec<_> = [".".to_owned(), "..".to_owned()]
                .into_iter()
                .chain((0..count).map(|index| format!("d{index:05}")))
                .collect();
            assert_eq!(names, expected);
            let cached = cache.as_ref().unwrap();
            assert_eq!(cached.entries.len(), DIRECTORY_CACHE_ENTRIES);
            assert!(!cached.complete);
        }
        volume.close_handle(listing.handle).unwrap();
    }

    #[test]
    fn no_space_after_deleting_a_full_file_is_retried_after_cleanup() {
        let (_image, mut volume) = volume("space");
        let block = vec![7_u8; 1 << 20];
        let mut filled = [0_u64; 2];
        for (round, total) in filled.iter_mut().enumerate() {
            let name = Name::parse(&format!("fill-{round}")).unwrap();
            let file = volume.create_file(ROOT, name.clone()).unwrap();
            while let Ok(written) =
                with_space(&mut volume, |volume| volume.write(&file, *total, &block))
            {
                *total += written as u64;
            }
            volume.close_handle(file).unwrap();
            volume.remove(ROOT, &name, RemoveKind::File).unwrap();
        }
        assert!(filled[0] > 100 << 20, "{filled:?}");
        assert_eq!(filled[0], filled[1]);
    }

    /// Closing a written file does not publish. Publishing on close cost 3.1x
    /// on creation; this is the adapter's durability contract, so it gets a
    /// test rather than only a comment.
    #[test]
    fn closing_a_written_file_does_not_publish() {
        let (_image, mut volume) = volume("close");
        volume.set_deferred_checkpoints(true).unwrap();
        let mut s = State::new(volume);
        let name = Name::parse("written").unwrap();
        let file = s.volume.create_file(ROOT, name).unwrap();
        s.volume.write(&file, 0, b"payload").unwrap();
        let object = s
            .volume
            .lookup(ROOT, &Name::parse("written").unwrap())
            .unwrap();
        let fh = FileHandle(s.insert(Open::File(object, file)));

        // `info().generation` does not move on a deferred publication, so the
        // pending flag is the signal here.
        s.close(fh, false).unwrap();
        assert!(s.volume.has_unsynced_changes(), "close published");

        // The timer's step is what makes it durable.
        s.maintain();
        assert!(
            !s.volume.has_unsynced_changes(),
            "the maintenance step published nothing"
        );
    }

    #[test]
    fn releasing_the_last_pin_on_a_quiet_volume_reclaims_the_orphan() {
        let (_image, volume) = volume("orphan");
        let mut s = State::new(volume);
        let name = Name::parse("pinned").unwrap();
        let file = s.volume.create_file(ROOT, name.clone()).unwrap();
        s.volume.close_handle(file).unwrap();
        let object = s.volume.lookup(ROOT, &name).unwrap();
        let handle = s.volume.open_object(object, Access::Read).unwrap();
        let fh = FileHandle(s.insert(Open::File(object, handle)));
        s.volume.remove(ROOT, &name, RemoveKind::File).unwrap();
        // Drain until a step finds nothing: the pinned orphan must survive.
        for _ in 0..100 {
            s.maintain();
        }
        let generation = s.volume.info().unwrap().generation;
        assert_eq!(s.quiet, Some(generation));
        assert!(s.volume.attributes(object).unwrap().orphan);

        // Without new publications, only the unpin can restart cleanup.
        s.close(fh, false).unwrap();
        for _ in 0..10 {
            s.maintain();
        }
        assert!(matches!(s.volume.attributes(object), Err(Error::NotFound)));
        assert_eq!(s.close(fh, false), Err(Errno::EBADF));
    }

    #[test]
    fn finding_the_volume_quiet_costs_no_flush() {
        struct Counting {
            inner: ImageDevice,
            flushes: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl BlockDevice for Counting {
            fn mode(&self) -> carryfs_core::MountMode {
                self.inner.mode()
            }
            fn geometry(&mut self) -> carryfs_core::Result<carryfs_core::Geometry> {
                self.inner.geometry()
            }
            fn read_exact_at(&mut self, at: u64, out: &mut [u8]) -> carryfs_core::Result<()> {
                self.inner.read_exact_at(at, out)
            }
            fn write_all_at(&mut self, at: u64, data: &[u8]) -> carryfs_core::Result<()> {
                self.inner.write_all_at(at, data)
            }
            fn flush_durable(&mut self) -> carryfs_core::Result<()> {
                self.flushes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.flush_durable()
            }
        }
        let (image, mut volume) = volume("quiet-flush");
        volume.eject().unwrap();
        let flushes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut volume = Volume::open(Counting {
            inner: ImageDevice::open(&image.0, carryfs_core::MountMode::ReadWrite).unwrap(),
            flushes: Arc::clone(&flushes),
        })
        .unwrap();
        volume.set_deferred_checkpoints(true).unwrap();
        let (fs, _forgotten) = CarryFs::new(volume, Owner { uid: 0, gid: 0 }, false);
        let mut s = fs.state.lock().unwrap();
        let file = s
            .volume
            .create_file(ROOT, Name::parse("file").unwrap())
            .unwrap();
        s.volume.write(&file, 0, b"one").unwrap();
        s.volume.write(&file, 0, b"two").unwrap();
        s.volume.close_handle(file).unwrap();
        s.maintain();
        assert!(!s.volume.has_unsynced_changes());
        // Steps run until the retired unit is reclaimed; the one that then
        // finds nothing left, and every step after it, flushes nothing.
        for _ in 0..8 {
            s.maintain();
        }
        let generation = s.volume.info().unwrap().generation;
        assert_eq!(s.quiet, Some(generation));
        let before = flushes.load(std::sync::atomic::Ordering::Relaxed);
        s.quiet = None;
        for _ in 0..8 {
            s.maintain();
        }
        assert_eq!(flushes.load(std::sync::atomic::Ordering::Relaxed), before);
        assert_eq!(s.quiet, Some(generation));
    }

    /// A step that scans a full window of pinned orphans publishes nothing,
    /// but work may lie beyond it, so the volume must not be marked quiet.
    #[test]
    fn a_closed_orphan_behind_a_pinned_window_is_reclaimed_without_other_changes() {
        let (_image, mut volume) = volume("pinned-window");
        volume.set_deferred_checkpoints(true).unwrap();
        let mut s = State::new(volume);
        let mut pins = Vec::new();
        for index in 0..33 {
            let name = Name::parse(&format!("pin{index}")).unwrap();
            let handle = s.volume.create_file(ROOT, name.clone()).unwrap();
            let object = s.volume.lookup(ROOT, &name).unwrap();
            s.volume.remove(ROOT, &name, RemoveKind::File).unwrap();
            pins.push(s.insert(Open::File(object, handle)));
        }
        let name = Name::parse("closed").unwrap();
        let handle = s.volume.create_file(ROOT, name.clone()).unwrap();
        let object = s.volume.lookup(ROOT, &name).unwrap();
        let fh = FileHandle(s.insert(Open::File(object, handle)));
        s.volume.remove(ROOT, &name, RemoveKind::File).unwrap();
        // Go quiet with the scan cursor wrapped back to the pinned window.
        for _ in 0..100 {
            s.maintain();
        }
        s.close(fh, false).unwrap();
        for _ in 0..10 {
            s.maintain();
        }
        assert!(matches!(s.volume.attributes(object), Err(Error::NotFound)));
        for fh in pins {
            s.close(FileHandle(fh), false).unwrap();
        }
    }

    #[test]
    fn a_truncate_with_the_kernels_time_of_now_is_one_commit() {
        let (_image, volume) = volume("setattr");
        let mut s = State::new(volume);
        let name = Name::parse("file").unwrap();
        let file = s.volume.create_file(ROOT, name.clone()).unwrap();
        s.volume.close_handle(file).unwrap();
        let object = s.volume.lookup(ROOT, &name).unwrap();
        let commits = |s: &mut State<ImageDevice>, size, mtime| {
            let before = s.volume.info().unwrap().generation;
            s.set_size_and_times(object, size, mtime, None).unwrap();
            s.volume.info().unwrap().generation - before
        };
        let time = |t: Timestamp| (t.seconds(), t.nanos());
        let old = s.volume.attributes(object).unwrap().modification;
        std::thread::sleep(Duration::from_millis(2));
        // ftruncate to a new size, and to the same size (no size change).
        assert_eq!(commits(&mut s, Some(5000), Some(TimeOrNow::Now)), 1);
        let grown = s.volume.attributes(object).unwrap();
        assert_eq!(grown.size, 5000);
        assert!(time(grown.modification) > time(old));
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(commits(&mut s, None, Some(TimeOrNow::Now)), 1);
        assert!(time(s.volume.attributes(object).unwrap().modification) > time(grown.modification));
        // A size with an explicit time keeps both.
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            commits(&mut s, Some(10), Some(TimeOrNow::SpecificTime(then))),
            2
        );
        let attributes = s.volume.attributes(object).unwrap();
        assert_eq!(attributes.size, 10);
        assert_eq!(attributes.modification.seconds(), 1_000_000);
        assert_eq!(commits(&mut s, None, None), 0);
    }

    #[test]
    fn errno_mapping_distinguishes_request_and_damage_errors() {
        assert_eq!(errno(&Error::NotFound), Errno::ENOENT);
        assert_eq!(errno(&Error::NameCollision), Errno::EEXIST);
        assert_eq!(errno(&Error::DirectoryNotEmpty), Errno::ENOTEMPTY);
        assert_eq!(errno(&Error::NoSpace), Errno::ENOSPC);
        assert_eq!(errno(&Error::ReadOnly), Errno::EROFS);
        assert_eq!(errno(&Error::InvalidName("x")), Errno::EINVAL);
        assert_eq!(
            errno(&Name::parse(&"x".repeat(256)).unwrap_err()),
            Errno::ENAMETOOLONG
        );
        assert_eq!(errno(&Error::Corrupt("x")), Errno::EIO);
        assert_eq!(errno(&Error::Faulted), Errno::EIO);
        assert_eq!(open_errno(&Error::LimitExceeded), Errno::EMFILE);
        assert_eq!(errno(&Error::LimitExceeded), Errno::EFBIG);
    }

    #[test]
    fn timestamps_round_trip_across_the_epoch() {
        for (seconds, nanos) in [(0, 0), (1_789_000_000, 5), (-1, 0), (-2, 999_999_999)] {
            let stored = Timestamp::new(seconds, nanos).unwrap();
            assert_eq!(timestamp(system_time(stored).unwrap()).unwrap(), stored);
        }
    }
}
