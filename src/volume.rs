use std::collections::BTreeSet;
#[cfg(not(windows))]
use std::fs::File;
#[cfg(not(windows))]
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::format::{
    BLOCK_BYTES, Page, ROOT_MAGIC, Root, TreeKind, TreeRef, decode_page, encode_leaf,
};
use crate::{BlockDevice, Error, Geometry, MountMode, Result};

mod file;
mod handles;
mod inspection;
mod namespace;

pub use handles::Handle;
pub use inspection::{ExtractionSink, ExtractionSummary, Finding};
pub use namespace::{DirectoryCursor, DirectoryEntry, DirectoryPage};

const ROOT_A: u64 = 256;
const MAX_PUBLICATION_PAGES: usize = 4096 + crate::workspace::MAX_BANK_PAGES;
// Pages a deferred chain may write before it checkpoints, which also bounds
// what a device holds unflushed: 256 MiB. 64 MiB stalled streams for each
// checkpoint's flushes.
const CHECKPOINT_PAGES: usize = 65_536;
// Maintenance plans one deferred `sync` chains before its checkpoint.
const MAINTENANCE_STEPS: usize = 64;

/// A user-chosen volume label, preserved verbatim as UTF-8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeLabel(String);

impl VolumeLabel {
    /// Accept 1 through 255 UTF-8 bytes, excluding `/`, control characters,
    /// and the path components `.` and `..`.
    pub fn parse(label: &str) -> Result<Self> {
        if label.is_empty()
            || label.len() > 255
            || matches!(label, "." | "..")
            || label.chars().any(|c| c == '/' || c.is_control())
        {
            return Err(Error::InvalidInput("invalid volume label"));
        }
        Ok(Self(label.to_owned()))
    }

    /// The original spelling supplied by the user.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Root-record information. This is not a structural check of the filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Format revision of the roots, always 1.
    pub revision: u32,
    /// Persistent user-chosen label, absent on unnamed volumes.
    pub label: Option<VolumeLabel>,
    /// Opaque volume identity stored in both roots.
    pub uuid: [u8; 16],
    /// Highest valid published generation.
    pub generation: u64,
    /// Validated device length in bytes.
    pub byte_len: u64,
    /// Recorded free blocks, including the maintenance reserve.
    pub free_blocks: u64,
}

impl VolumeInfo {
    /// The stored label, or a UUID-derived name for an unnamed volume.
    pub fn name(&self) -> String {
        self.label.as_ref().map_or_else(
            || {
                format!(
                    "CarryFS-{:02X}{:02X}{:02X}{:02X}",
                    self.uuid[0], self.uuid[1], self.uuid[2], self.uuid[3]
                )
            },
            |label| label.as_str().to_owned(),
        )
    }
}

/// Volume capacity in allocation blocks, from the selected root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    /// Bytes per allocation block.
    pub block_bytes: u32,
    /// Blocks on the device, including reserved bands.
    pub total_blocks: u64,
    /// Recorded free blocks, including the maintenance reserve.
    pub free_blocks: u64,
    /// Free blocks outside the maintenance reserve.
    pub available_blocks: u64,
}

/// A catalog object's identity. Identifies a record; does not pin its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectId(pub(crate) u64);

impl ObjectId {
    /// Accept an adapter-supplied identifier, such as an inode number. IDs are
    /// never reused, so a stale identifier resolves to `NotFound`, not another object.
    pub fn new(raw: u64) -> Result<Self> {
        if raw == 0 || raw > i64::MAX as u64 {
            return Err(Error::InvalidInput("object ID is out of range"));
        }
        Ok(Self(raw))
    }

    /// The stored identifier, stable for the life of the object.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The root directory's object ID. The root is self-parented and never orphaned.
pub const ROOT: ObjectId = ObjectId(1);

/// Whether a catalog object is a regular file, a directory, or a symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link. Its size is the target's length in bytes.
    Symlink,
}

/// A validated Unix-epoch instant, as stored on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    seconds: i64,
    nanos: u32,
}

impl Timestamp {
    /// Validate and construct a timestamp from its stored components.
    pub fn new(seconds: i64, nanos: u32) -> Result<Self> {
        crate::format::validate_timestamp_range(seconds, nanos, crate::format::Validation::Input)?;
        Ok(Self { seconds, nanos })
    }

    /// Signed Unix seconds.
    pub fn seconds(&self) -> i64 {
        self.seconds
    }

    /// Nanoseconds within the second.
    pub fn nanos(&self) -> u32 {
        self.nanos
    }

    fn now() -> Result<Self> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| Error::InvalidInput("system time is before the Unix epoch"))?;
        let seconds = i64::try_from(now.as_secs())
            .map_err(|_| Error::InvalidInput("system time exceeds timestamp range"))?;
        Self::new(seconds, now.subsec_nanos())
    }
}

/// Type, size, and stored timestamps of a catalog object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attributes {
    /// Whether this object is a file, a directory, or a symlink.
    pub kind: ObjectKind,
    /// Logical size in bytes; zero for directories, the target length for symlinks.
    pub size: u64,
    /// Allocated data bytes backing the object's current extents.
    pub allocated_bytes: u64,
    /// The containing directory. The root is its own parent.
    pub parent: ObjectId,
    /// Creation timestamp.
    pub creation: Timestamp,
    /// Modification timestamp.
    pub modification: Timestamp,
    /// Metadata-change timestamp.
    pub metadata_change: Timestamp,
    /// Unlinked and kept alive only by open handles until maintenance reclaims it.
    pub orphan: bool,
    /// For a symlink created on Windows, whether it is a directory link;
    /// `None` for any other object or a link made elsewhere. Only the Windows
    /// adapter reads it; other OSes do not type links.
    pub directory_link: Option<bool>,
}

/// Read or read-write access requested when opening a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read-only access.
    Read,
    /// Read and write access.
    ReadWrite,
}

/// Whether a removal targets a non-directory or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveKind {
    /// Unlink a regular file or a symlink.
    File,
    /// Remove an empty directory.
    Directory,
}

/// Whether a rename may replace an existing destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replace {
    /// Reject an existing destination.
    Forbidden,
    /// Replace an existing destination of a compatible type.
    Allowed,
}

/// Reads a read-only volume's catalog leaves ahead of their use, on a device
/// handle of its own. See [`Volume::read_ahead`].
pub struct ReadAhead {
    root: Root,
    walk: crate::tree::LeafWalk,
}

impl ReadAhead {
    /// Read, verify and decode up to `pages` further leaves from `device`,
    /// which must be the medium the volume holds. Pages must match the
    /// volume's UUID and tree references. An empty result ends the walk,
    /// either at the tree's end or after its 4,096-page budget is spent.
    pub fn next<D: BlockDevice>(&mut self, device: &mut D, pages: usize) -> Result<LeavesRead> {
        Ok(LeavesRead {
            leaves: self.walk.next(device, &self.root, pages)?,
            root: self.root.clone(),
        })
    }
}

/// Catalog leaves a [`ReadAhead`] read, for [`Volume::keep_read_ahead`].
pub struct LeavesRead {
    root: Root,
    leaves: Vec<crate::tree::CheckedLeaf>,
}

impl LeavesRead {
    /// Whether the reader found no further leaf.
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }
}

/// An exclusively owned volume.
///
/// Supports formatting, structural checking, synchronization, and eject.
/// Writable open checks the entire selected forest before finishing mirroring.
/// Read-only open validates root pages; call `check` for a complete traversal.
/// Scrub and extraction require read-only access and report unreadable ranges.
pub struct Volume<D: BlockDevice> {
    device: Option<D>,
    geometry: Geometry,
    // None exists only while formatting, before the first publication succeeds.
    root: Option<Root>,
    fault: Option<Fault>,
    mount: u64,
    handles: handles::Registry,
    namespace_epoch: u64,
    // Allocation group where the last data allocation ended; a search start only.
    data_hint: u64,
    // Next orphan ID to examine. Mount pins and this cursor disappear on reopen.
    orphan_cursor: u64,
    cache: crate::tree::Cache,
    // Opt-in for a read-only mounted volume. Inspection bypasses this cache.
    readonly_metadata_cache: bool,
    // Set for the length of a scrub, which lends its catalog reads the cache.
    scrubbing: bool,
    chain: Option<Chain>,
    // Operations accepted since the last plan, not yet sealed. Only a volume
    // with deferred checkpoints batches; `Transaction::fits_batch` holds while
    // this is present, so sealing it cannot fail for want of space or budget.
    batch: Option<crate::allocation::Pending>,
}

/// Plans published since the last checkpoint. Their pages are written but not
/// flushed, and the roots still select `durable`. No block is written twice
/// between flushes: pages the chain supersedes stay out of use, retirement
/// newer than `durable` is not reclaimed, and the bank, which every chained
/// plan places in the location `durable` does not select, is written only at
/// the checkpoint.
struct Chain {
    durable: Root,
    excluded: BTreeSet<u64>,
    /// Blocks this chain wrote. No durable root references them, so one
    /// superseded before the checkpoint is dead and its write can be dropped.
    written: BTreeSet<u64>,
    /// The workspace list the chained root selects, encoded at the checkpoint.
    bank: Option<Vec<u64>>,
    pages: usize,
}

#[derive(Clone)]
struct BlockWrite {
    block: u64,
    page: Page,
}

enum Fault {
    Corruption,
    Publication,
}

/// Whether root slot A or B begins with the root magic, whatever else the
/// device holds. Adapters identify CarryFS devices by it, so a damaged
/// volume is still recognized and mounting reports the damage.
pub fn has_root_magic<D: BlockDevice>(device: &mut D) -> bool {
    let Ok(geometry) = device.geometry() else {
        return false;
    };
    if geometry.validate().is_err() {
        return false;
    }
    [ROOT_A, geometry.blocks() - 257].into_iter().any(|block| {
        // Devices read whole sectors, so read the page, not just the magic.
        let mut page = Page(Box::new([0; BLOCK_BYTES]));
        geometry
            .block_offset(block)
            .and_then(|at| device.read_exact_at(at, &mut page.0[..]))
            .is_ok_and(|()| page.0.starts_with(ROOT_MAGIC))
    })
}

// Handles and cursors only need to be unique among instances in this process.
fn next_mount() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(not(windows))]
fn system_random(out: &mut [u8]) -> std::io::Result<()> {
    File::open("/dev/urandom").and_then(|mut source| source.read_exact(out))
}

#[cfg(windows)]
fn system_random(out: &mut [u8]) -> std::io::Result<()> {
    use std::ffi::c_void;

    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, len: u32, flags: u32) -> i32;
    }
    let len = u32::try_from(out.len()).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: the buffer is live and writable for `len` bytes, and the system
    // preferred RNG needs no algorithm handle.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            out.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(std::io::Error::other(format!(
            "BCryptGenRandom failed: NTSTATUS {status:#010x}"
        )));
    }
    Ok(())
}

impl<D: BlockDevice> Volume<D> {
    /// Format media with zeroed reserved bands as an empty filesystem.
    /// The UUID comes from the system random source: `/dev/urandom` on Unix,
    /// `BCryptGenRandom` on Windows.
    /// An error can still leave a recoverable generation-1 root on disk.
    pub fn format(device: D) -> Result<Self> {
        Self::format_named(device, None)
    }

    /// Format zeroed media with a persistent volume label in the roots.
    pub fn format_with_label(device: D, label: VolumeLabel) -> Result<Self> {
        Self::format_named(device, Some(label))
    }

    fn format_named(device: D, label: Option<VolumeLabel>) -> Result<Self> {
        let mut uuid = [0; 16];
        system_random(&mut uuid).map_err(|error| Error::io("generate volume UUID", error))?;
        uuid[6] = (uuid[6] & 0x0f) | 0x40;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        let now = Timestamp::now()?;
        Self::format_labeled_at(device, uuid, now.seconds, now.nanos, label)
    }

    #[cfg(any(test, feature = "fuzzing"))]
    fn format_at(device: D, uuid: [u8; 16], seconds: i64, nanos: u32) -> Result<Self> {
        Self::format_labeled_at(device, uuid, seconds, nanos, None)
    }

    fn format_labeled_at(
        mut device: D,
        uuid: [u8; 16],
        seconds: i64,
        nanos: u32,
        label: Option<VolumeLabel>,
    ) -> Result<Self> {
        if device.mode() != MountMode::ReadWrite {
            return Err(Error::ReadOnly);
        }
        let geometry = device.geometry()?;
        geometry.validate()?;
        for block in (0..512).chain(geometry.blocks() - 512..geometry.blocks()) {
            let mut page = Page(Box::new([0; BLOCK_BYTES]));
            device.read_exact_at(geometry.block_offset(block)?, &mut page.0[..])?;
            if page.0.iter().any(|byte| *byte != 0) {
                return Err(Error::InvalidInput(
                    "existing or unfinished format; reserved bands must be zero",
                ));
            }
        }
        let (mut root, writes) = bootstrap(geometry, uuid, seconds, nanos)?;
        root.label = label;
        let mount = next_mount();
        let mut volume = Self {
            device: Some(device),
            geometry,
            root: None,
            fault: None,
            mount,
            handles: handles::Registry::new(mount),
            namespace_epoch: 0,
            data_hint: 0,
            orphan_cursor: 0,
            cache: crate::tree::Cache::default(),
            readonly_metadata_cache: false,
            scrubbing: false,
            chain: None,
            batch: None,
        };
        volume.publish(root, writes, &[], None, Vec::new(), 0)?;
        Ok(volume)
    }

    /// Select the newest valid root and validate its three tree-root pages.
    /// Writable open checks the complete forest, then finishes interrupted mirroring.
    /// Tree damage under the selected root never causes fallback to an older root.
    pub fn open(mut device: D) -> Result<Self> {
        let geometry = device.geometry()?;
        geometry.validate()?;
        let root_b = geometry.blocks() - 257;
        let mut a = Page(Box::new([0; BLOCK_BYTES]));
        let mut b = Page(Box::new([0; BLOCK_BYTES]));
        device.read_exact_at(geometry.block_offset(ROOT_A)?, &mut a.0[..])?;
        device.read_exact_at(geometry.block_offset(root_b)?, &mut b.0[..])?;
        let a = supported_root(Root::decode(&a.0[..], ROOT_A, geometry))?;
        let b = supported_root(Root::decode(&b.0[..], root_b, geometry))?;
        let selected = select_root(a.as_ref(), b.as_ref())?.clone();
        validate_root_pages(&mut device, geometry, &selected)?;
        let mut cache = crate::tree::Cache::default();
        if device.mode() == MountMode::ReadWrite {
            crate::check::validate_keeping(&mut device, &selected, &mut cache)?;
            for (block, existing) in [(ROOT_A, a.as_ref()), (root_b, b.as_ref())] {
                if existing != Some(&selected) {
                    let page = selected.encode(block, geometry)?;
                    device.write_all_at(geometry.block_offset(block)?, &page.0[..])?;
                    device.flush_durable()?;
                }
            }
        }
        let mount = next_mount();
        Ok(Self {
            device: Some(device),
            geometry,
            root: Some(selected),
            fault: None,
            mount,
            handles: handles::Registry::new(mount),
            namespace_epoch: 0,
            data_hint: 0,
            orphan_cursor: 0,
            cache,
            readonly_metadata_cache: false,
            scrubbing: false,
            chain: None,
            batch: None,
        })
    }

    /// Retain the existing bounded metadata cache across read-only operations.
    ///
    /// Use this for a mounted read-only volume. Structural checks and recovery
    /// always read metadata afresh. Later damage behind a cached metadata page
    /// is found after cache eviction or by a fresh inspection.
    pub fn enable_readonly_metadata_cache(&mut self) -> Result<()> {
        self.readable()?;
        if self.device.as_ref().ok_or(Error::Ejected)?.mode() != MountMode::ReadOnly {
            return Err(Error::InvalidInput(
                "read-only metadata cache requires a read-only device",
            ));
        }
        self.readonly_metadata_cache = true;
        Ok(())
    }

    /// Start reading this read-only volume's catalog leaves ahead of use.
    ///
    /// A mounted volume's first listing otherwise reads, verifies and decodes
    /// each catalog page as it reaches it, on the thread that answers the
    /// request. The returned reader does that work on a second handle to the
    /// same medium, on a thread of the caller's, and [`Volume::keep_read_ahead`]
    /// hands its leaves to the metadata cache. Requires
    /// [`Volume::enable_readonly_metadata_cache`].
    pub fn read_ahead(&self) -> Result<ReadAhead> {
        self.readable()?;
        if !self.readonly_metadata_cache {
            return Err(Error::InvalidInput(
                "read-ahead requires the read-only metadata cache",
            ));
        }
        let root = self
            .root
            .clone()
            .ok_or(Error::InvalidInput("volume is not published"))?;
        Ok(ReadAhead {
            walk: crate::tree::LeafWalk::new(&root, TreeKind::Catalog, root.catalog),
            root,
        })
    }

    /// Keep leaves a [`ReadAhead`] of this volume read. Returns whether the
    /// cache has room for more; a reader stops once it has none. Leaves read
    /// for another root are dropped.
    pub fn keep_read_ahead(&mut self, leaves: LeavesRead) -> Result<bool> {
        self.readable()?;
        if !self.readonly_metadata_cache || self.root.as_ref() != Some(&leaves.root) {
            return Ok(false);
        }
        for leaf in leaves.leaves {
            if !self.cache.has_room() {
                return Ok(false);
            }
            self.cache.keep(leaf);
        }
        Ok(self.cache.has_room())
    }

    /// Return selected-root facts, without claiming a full allocation/graph check.
    pub fn info(&self) -> Result<VolumeInfo> {
        self.readable()?;
        let root = self
            .root
            .as_ref()
            .ok_or(Error::InvalidInput("volume is not published"))?;
        Ok(VolumeInfo {
            revision: u32::from(crate::format::REVISION),
            label: root.label.clone(),
            uuid: root.uuid,
            generation: root.generation,
            byte_len: self.geometry.byte_len,
            free_blocks: root.free_blocks,
        })
    }

    /// Persist a volume label using the normal root publication sequence.
    /// This also publishes any deferred file changes; on publication failure,
    /// reopen is required and either the old or new label may be durable.
    pub fn set_label(&mut self, label: VolumeLabel) -> Result<()> {
        self.writable()?;
        self.seal_batch()?;
        let mut next = self
            .root
            .clone()
            .ok_or(Error::InvalidInput("volume is not published"))?;
        if next.label.as_ref() == Some(&label) {
            return self.sync_publication();
        }
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::InvalidInput("generation exhausted"))?;
        next.label = Some(label);
        self.publish(next, Vec::new(), &[], None, Vec::new(), 0)?;
        self.sync_publication()
    }

    /// Report capacity for adapter statistics. Available space excludes the
    /// maintenance reserve; metadata workspace can still make a write fail with `NoSpace`.
    pub fn space(&self) -> Result<Space> {
        // The published free count, less data the open batch has allocated.
        let batched = self
            .batch
            .as_ref()
            .map_or(0, |batch| batch.allocated_blocks() as u64);
        let free_blocks = self.info()?.free_blocks.saturating_sub(batched);
        Ok(Space {
            block_bytes: BLOCK_BYTES as u32,
            total_blocks: self.geometry.blocks(),
            free_blocks,
            available_blocks: free_blocks.saturating_sub(crate::allocation::RESERVE),
        })
    }

    /// Validate the complete selected forest and its allocation accounting.
    /// Requires a read-only device and stops at the first error without writes.
    /// This checks checksum presence, but does not read or scrub file contents.
    pub fn check(&mut self) -> Result<crate::CheckSummary> {
        self.readable()?;
        let device = self.device.as_mut().ok_or(Error::Ejected)?;
        if device.mode() != MountMode::ReadOnly {
            return Err(Error::InvalidInput(
                "structural inspection requires a read-only device",
            ));
        }
        crate::check::validate(
            device,
            self.root
                .as_ref()
                .ok_or(Error::InvalidInput("volume is not published"))?,
        )
    }

    /// Publish all pending changes and wait for device durability. With eager
    /// checkpoints this runs one bounded maintenance step; with deferred
    /// checkpoints it chains up to 64 steps, then checkpoints.
    /// Failed output or detected corruption blocks further mutation and cleanup.
    /// Planning failures issue no writes.
    /// Read-only synchronization performs no I/O.
    pub fn sync(&mut self) -> Result<()> {
        self.sync_with_maintenance(true)
    }

    /// Publish pending changes and wait for device durability, leaving eligible
    /// cleanup to a later full `sync`. This keeps explicit file and directory
    /// synchronization from waiting for unrelated reclamation. With nothing
    /// unsynced it makes no device call: the last checkpoint flushed both roots.
    pub fn sync_publication(&mut self) -> Result<()> {
        self.sync_with_maintenance(false)
    }

    fn sync_with_maintenance(&mut self, maintenance: bool) -> Result<()> {
        self.readable()?;
        let device = self.device.as_mut().ok_or(Error::Ejected)?;
        if device.mode() == MountMode::ReadOnly {
            return Ok(());
        }
        self.writable()?;
        // Maintenance plans against the published root.
        self.seal_batch()?;
        let steps = if !maintenance {
            0
        } else if self.chain.is_some() {
            MAINTENANCE_STEPS
        } else {
            1
        };
        let mut quiet_cursor = None;
        for _ in 0..steps {
            let plan = self.prepare_maintenance().and_then(|(plan, cursor)| {
                let root = self
                    .root
                    .as_ref()
                    .ok_or(Error::InvalidInput("volume is not published"))?;
                Ok((plan.map(|plan| plan.into_parts(root)).transpose()?, cursor))
            });
            match self.record_corruption(plan)? {
                (Some((next, writes, returned, slots, built, data_pages)), cursor) => {
                    self.publish(
                        next,
                        block_writes(writes),
                        &returned,
                        slots,
                        built,
                        data_pages,
                    )?;
                    self.orphan_cursor = cursor;
                }
                (None, cursor) => {
                    quiet_cursor = Some(cursor);
                    break;
                }
            }
        }
        if self.has_unsynced_changes() {
            self.checkpoint()?;
        } else if quiet_cursor.is_some() {
            // A full sync that found nothing to do still ends in a flush.
            // `sync_publication` plans no step, and with nothing unsynced the
            // last checkpoint has already flushed both roots.
            let device = self.device.as_mut().ok_or(Error::Ejected)?;
            if let Err(error) = device.flush_durable() {
                self.set_fault(Fault::Publication);
                return Err(error);
            }
        }
        if let Some(cursor) = quiet_cursor {
            self.orphan_cursor = cursor;
        }
        Ok(())
    }

    /// Whether a full `sync` would find cleanup to publish: retirement to
    /// reclaim, detached jobs, unpinned orphans, or workspace to balance. It
    /// plans that step and drops it, so it writes and flushes nothing, where a
    /// `sync` with no cleanup still flushes. Changes awaiting a deferred
    /// checkpoint are not cleanup; `has_unsynced_changes` reports those, and
    /// retirement they supersede becomes eligible only after it.
    pub fn has_cleanup_work(&mut self) -> Result<bool> {
        self.readable()?;
        if self.device.as_ref().ok_or(Error::Ejected)?.mode() == MountMode::ReadOnly {
            return Ok(false);
        }
        self.writable()?;
        let plan = self.prepare_maintenance().map(|(plan, _)| plan.is_some());
        self.record_corruption(plan)
    }

    /// Plan one maintenance step against the published root without output.
    fn prepare_maintenance(&mut self) -> Result<(Option<crate::allocation::Plan>, u64)> {
        let device = self.device.as_mut().ok_or(Error::Ejected)?;
        let root = self
            .root
            .as_ref()
            .ok_or(Error::InvalidInput("volume is not published"))?;
        let pinned = self.handles.pinned();
        let context = crate::allocation::Context {
            cache: &mut self.cache,
            durable: self
                .chain
                .as_ref()
                .map(|chain| (&chain.durable, &chain.excluded)),
        };
        crate::allocation::prepare_maintenance(
            device,
            root,
            &pinned,
            self.orphan_cursor,
            Some(context),
        )
    }

    /// Choose when publications reach the roots. Eager, the default, flushes
    /// and publishes both roots for every write, truncate, namespace change,
    /// and maintenance step. Deferred writes each change's pages but not the
    /// roots, and publishes at `sync`, `eject`, or when the chain reaches its
    /// work bound; a crash before then recovers the last published state.
    /// Switching to eager publishes pending changes first.
    pub fn set_deferred_checkpoints(&mut self, deferred: bool) -> Result<()> {
        self.writable()?;
        if deferred {
            if self.chain.is_none() {
                let durable = self
                    .root
                    .clone()
                    .ok_or(Error::InvalidInput("volume is not published"))?;
                self.chain = Some(Chain {
                    durable,
                    excluded: BTreeSet::new(),
                    written: BTreeSet::new(),
                    bank: None,
                    pages: 0,
                });
            }
        } else {
            self.seal_batch()?;
            self.checkpoint()?;
            self.chain = None;
        }
        Ok(())
    }

    /// Whether changes accepted with deferred checkpoints await `sync`.
    pub fn has_unsynced_changes(&self) -> bool {
        self.batch.is_some()
            || self
                .chain
                .as_ref()
                .is_some_and(|chain| self.root.as_ref() != Some(&chain.durable))
    }

    /// Whether a failed publication or detected corruption has stopped writes.
    /// A faulted volume cannot publish pending changes; reopen it to recover.
    pub fn is_faulted(&self) -> bool {
        self.fault.is_some()
    }

    /// Synchronize and release device ownership. Failed eject retains the lease.
    /// Requires no outstanding handles.
    pub fn eject(&mut self) -> Result<()> {
        self.readable()?;
        if !self.handles.is_empty() {
            return Err(Error::InvalidInput(
                "eject requires closing all outstanding handles",
            ));
        }
        self.sync()?;
        self.device.take();
        Ok(())
    }

    fn readable(&self) -> Result<()> {
        if self.device.is_none() {
            return Err(Error::Ejected);
        }
        if matches!(self.fault, Some(Fault::Publication)) {
            return Err(Error::Faulted);
        }
        Ok(())
    }

    fn writable(&self) -> Result<()> {
        self.readable()?;
        if self.fault.is_some() {
            return Err(Error::Faulted);
        }
        if self.device.as_ref().ok_or(Error::Ejected)?.mode() != MountMode::ReadWrite {
            return Err(Error::ReadOnly);
        }
        Ok(())
    }

    fn record_corruption<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(&result, Err(Error::Corrupt(_) | Error::Corruption { .. }))
            && self
                .device
                .as_ref()
                .is_some_and(|device| device.mode() == MountMode::ReadWrite)
        {
            self.set_fault(Fault::Corruption);
        }
        result
    }

    fn set_fault(&mut self, fault: Fault) {
        self.fault = Some(fault);
        self.cache = crate::tree::Cache::default();
        // A faulted volume accepts no further writes, a seal included, so the
        // batch's operations are lost as a crash would lose them: reopening
        // recovers the last published state.
        self.batch = None;
    }

    // Bootstrap and the allocation planner prepare every write before reaching
    // this boundary. Metadata uses mirrored workspace slots and the inactive bank.
    // Publication itself never chooses storage or performs reclamation.
    fn publish(
        &mut self,
        next: Root,
        writes: Vec<BlockWrite>,
        returned: &[u64],
        slots: Option<Vec<u64>>,
        built: Vec<crate::tree::Built>,
        data_pages: usize,
    ) -> Result<()> {
        self.writable()?;
        let generation = self
            .root
            .as_ref()
            .map_or(0, |root| root.generation)
            .checked_add(1)
            .ok_or(Error::InvalidInput("generation exhausted"))?;
        if next.generation != generation
            || (writes.is_empty() && self.root.is_none())
            || writes.len() > MAX_PUBLICATION_PAGES
        {
            return Err(Error::InvalidInput(
                "invalid publication generation or work budget",
            ));
        }
        if let Some(previous) = &self.root
            && (next.uuid != previous.uuid
                || next.blocks != previous.blocks
                || next.next_object < previous.next_object
                || next.next_job < previous.next_job)
        {
            return Err(Error::InvalidInput(
                "publication changes immutable identity or reuses IDs",
            ));
        }
        let workspace_changed = self
            .root
            .as_ref()
            .is_none_or(|previous| previous.workspace != next.workspace);
        let mut blocks = BTreeSet::new();
        for write in &writes {
            let ordinary = (512..self.geometry.blocks() - 512).contains(&write.block);
            let workspace = (next.workspace.block
                ..next.workspace.block + crate::workspace::MAX_BANK_PAGES as u64)
                .contains(&write.block)
                && workspace_changed;
            if (!ordinary && !workspace) || !blocks.insert(write.block) {
                return Err(Error::InvalidInput(
                    "duplicate or reserved replacement block",
                ));
            }
            self.geometry.block_offset(write.block)?;
        }

        let mut workspace = None;
        if workspace_changed {
            // A chained plan's bank goes where the durable roots do not look.
            let expected = self
                .chain
                .as_ref()
                .map(|chain| &chain.durable)
                .or(self.root.as_ref())
                .map(crate::workspace::next_reference)
                .transpose()?
                .map(|reference| TreeRef {
                    block: reference.block,
                    generation,
                });
            if next.workspace.generation != generation
                || expected.is_some_and(|reference| reference != next.workspace)
            {
                return Err(Error::InvalidInput("invalid workspace publication"));
            }
            // A chained plan hands over its list; the checkpoint encodes and
            // validates the one bank that reaches the device.
            workspace = if self.chain.is_some() {
                if writes
                    .iter()
                    .any(|write| crate::workspace::is_bank_block(write.block, next.blocks))
                {
                    return Err(Error::InvalidInput("chained workspace page in publication"));
                }
                Some(slots.ok_or(Error::InvalidInput("chained publication has no workspace"))?)
            } else {
                let mut consumed = BTreeSet::new();
                let read = crate::workspace::read_with(&next, |block| {
                    consumed.insert(block);
                    writes
                        .iter()
                        .find(|write| write.block == block)
                        .map(|write| write.page.clone())
                        .ok_or(Error::InvalidInput("workspace publication is incomplete"))
                })?;
                if writes.iter().any(|write| {
                    crate::workspace::is_bank_block(write.block, next.blocks)
                        && !consumed.contains(&write.block)
                }) {
                    return Err(Error::InvalidInput("unused workspace page in publication"));
                }
                Some(read)
            };
        }

        let Some(chain) = self.chain.as_mut() else {
            self.commit(&next, writes)?;
            // Only committed pages enter the cache; a failed commit faulted
            // the volume, which dropped it.
            self.cache.insert_built(built);
            if let Some(slots) = workspace {
                self.cache.workspace = Some((next.workspace, slots));
            }
            self.root = Some(next);
            return Ok(());
        };
        let device = self.device.as_mut().ok_or(Error::Ejected)?;
        // File data left with the operations; it still counts toward the
        // unflushed bound that schedules the checkpoint.
        let pages = writes.len() + data_pages;
        let result = (|| {
            // Excluded slots are not reused before the checkpoint, so a
            // discarded block cannot also be one of this publication's writes.
            for block in returned {
                if chain.written.remove(block) {
                    device.discard(self.geometry.block_offset(*block)?);
                }
            }
            for write in writes {
                device.write_page(self.geometry.block_offset(write.block)?, write.page.0)?;
                chain.written.insert(write.block);
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.set_fault(Fault::Publication);
            return Err(error);
        }
        if workspace_changed {
            chain.bank = workspace.clone();
        }
        chain.excluded.extend(returned);
        chain.pages += pages;
        // Excluded slots stay in the workspace until the checkpoint.
        let due = chain.pages >= CHECKPOINT_PAGES
            || workspace.as_ref().is_some_and(|slots| {
                slots.len().saturating_sub(chain.excluded.len())
                    < crate::allocation::CHECKPOINT_SLOTS
            });
        // The chain has accepted these pages and the in-memory root is about
        // to reference them, so no later plan reuses this generation.
        self.cache.insert_built(built);
        if let Some(slots) = workspace {
            self.cache.workspace = Some((next.workspace, slots));
        }
        self.root = Some(next);
        if due {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Publish the chained state: its bank, a flush, then both roots.
    fn checkpoint(&mut self) -> Result<()> {
        self.seal_batch()?;
        if !self.has_unsynced_changes() {
            return Ok(());
        }
        let root = self
            .root
            .clone()
            .ok_or(Error::InvalidInput("volume is not published"))?;
        let Some(chain) = self.chain.as_mut() else {
            return Ok(());
        };
        let slots = std::mem::take(&mut chain.bank);
        let bank = match slots {
            Some(slots) => {
                let pages = crate::workspace::encode(&root, &slots)?;
                // The same boundary check an eager publication makes, once.
                let read = crate::workspace::read_with(&root, |block| {
                    pages
                        .iter()
                        .find(|(candidate, _)| *candidate == block)
                        .map(|(_, page)| page.clone())
                        .ok_or(Error::InvalidInput("workspace checkpoint is incomplete"))
                })?;
                if read != slots {
                    return Err(Error::InvalidInput(
                        "workspace checkpoint does not round-trip",
                    ));
                }
                block_writes(pages)
            }
            None => Vec::new(),
        };
        self.commit(&root, bank)?;
        if let Some(chain) = self.chain.as_mut() {
            chain.durable = root;
            chain.excluded.clear();
            chain.written.clear();
            chain.pages = 0;
        }
        Ok(())
    }

    /// Write `writes`, flush, then root A, flush, root B, flush.
    fn commit(&mut self, root: &Root, writes: Vec<BlockWrite>) -> Result<()> {
        let a = root.encode(ROOT_A, self.geometry)?;
        let root_b = self.geometry.blocks() - 257;
        let b = root.encode(root_b, self.geometry)?;
        let device = self.device.as_mut().ok_or(Error::Ejected)?;
        let result = (|| {
            for write in writes {
                device.write_page(self.geometry.block_offset(write.block)?, write.page.0)?;
            }
            device.flush_durable()?;
            device.write_all_at(self.geometry.block_offset(ROOT_A)?, &a.0[..])?;
            device.flush_durable()?;
            device.write_all_at(self.geometry.block_offset(root_b)?, &b.0[..])?;
            device.flush_durable()
        })();
        if let Err(error) = result {
            self.set_fault(Fault::Publication);
            return Err(error);
        }
        Ok(())
    }
}

fn block_writes(writes: Vec<(u64, Page)>) -> Vec<BlockWrite> {
    writes
        .into_iter()
        .map(|(block, page)| BlockWrite { block, page })
        .collect()
}

fn supported_root(root: Result<Root>) -> Result<Option<Root>> {
    match root {
        Ok(root) => Ok(Some(root)),
        Err(Error::Corrupt(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn select_root<'a>(a: Option<&'a Root>, b: Option<&'a Root>) -> Result<&'a Root> {
    match (a, b) {
        (None, None) => Err(Error::Corrupt("neither current root is valid")),
        (Some(root), None) | (None, Some(root)) => Ok(root),
        (Some(a), Some(b)) => {
            if a.uuid != b.uuid {
                return Err(Error::Corrupt("root UUIDs conflict"));
            }
            if a.generation == b.generation && a != b {
                return Err(Error::Corrupt("same-generation roots conflict"));
            }
            Ok(if a.generation >= b.generation { a } else { b })
        }
    }
}

fn validate_root_pages(
    device: &mut impl BlockDevice,
    geometry: Geometry,
    root: &Root,
) -> Result<()> {
    for (reference, kind) in [
        (root.catalog, TreeKind::Catalog),
        (root.allocation, TreeKind::Allocation),
        (root.cleanup, TreeKind::Cleanup),
    ] {
        let mut page = Page(Box::new([0; BLOCK_BYTES]));
        device.read_exact_at(geometry.block_offset(reference.block)?, &mut page.0[..])?;
        let decoded = decode_page(
            &page.0[..],
            reference,
            &root.uuid,
            kind,
            root.generation,
            None,
            geometry.blocks(),
        )?;
        if kind == TreeKind::Catalog {
            let maximum_data = root
                .blocks
                .checked_sub(1027)
                .and_then(|blocks| blocks.checked_sub(root.free_blocks))
                .ok_or(Error::Corrupt(
                    "free count leaves no space for the root pages",
                ))?;
            if decoded.data_blocks > maximum_data {
                return Err(Error::Corrupt("catalog data exceeds recorded allocation"));
            }
            let root_key = [0, 0, 0, 0, 0, 0, 0, 1, 1];
            let (key, value) = decoded
                .records
                .first()
                .ok_or(Error::Corrupt("catalog has no root inode"))?;
            if *key != root_key {
                return Err(Error::Corrupt("catalog does not start at the root inode"));
            }
            if decoded.level == 0
                && (value[0] != 2 || value[1] != 0 || value[8..16] != 1_u64.to_le_bytes())
            {
                return Err(Error::Corrupt("invalid root directory inode"));
            }
        }
    }
    crate::workspace::read(device, root)?;
    Ok(())
}

fn bootstrap(
    geometry: Geometry,
    uuid: [u8; 16],
    seconds: i64,
    nanos: u32,
) -> Result<(Root, Vec<BlockWrite>)> {
    crate::format::validate_timestamp_range(seconds, nanos, crate::format::Validation::Input)?;
    let reference = |block| TreeRef {
        block,
        generation: 1,
    };
    let root = Root {
        label: None,
        uuid,
        generation: 1,
        blocks: geometry.blocks(),
        catalog: reference(512),
        allocation: reference(513),
        cleanup: reference(514),
        next_object: 2,
        free_blocks: geometry.blocks() - 1027,
        next_job: 1,
        workspace: reference(32),
        workspace_cursor: 0,
    };
    let mut inode_key = [0; 9];
    inode_key[..8].copy_from_slice(&1_u64.to_be_bytes());
    inode_key[8] = 1;
    let mut inode = [0; 128];
    inode[0] = 2;
    inode[8..16].copy_from_slice(&1_u64.to_le_bytes());
    for offset in [40, 56, 72] {
        inode[offset..offset + 8].copy_from_slice(&seconds.to_le_bytes());
        inode[offset + 8..offset + 12].copy_from_slice(&nanos.to_le_bytes());
    }
    let mut bitmap = [0; 2048];
    for block in (0..515).chain(crate::workspace::INITIAL) {
        bitmap[block as usize / 8] |= 1 << (block % 8);
    }
    let catalog = encode_leaf(
        root.catalog,
        &uuid,
        TreeKind::Catalog,
        &[(&inode_key, &inode)],
        root.blocks,
    )?;
    let allocation = encode_leaf(
        root.allocation,
        &uuid,
        TreeKind::Allocation,
        &[(&0_u64.to_be_bytes(), &bitmap)],
        root.blocks,
    )?;
    let cleanup = encode_leaf(root.cleanup, &uuid, TreeKind::Cleanup, &[], root.blocks)?;
    let mut writes = vec![
        BlockWrite {
            block: 512,
            page: catalog,
        },
        BlockWrite {
            block: 513,
            page: allocation,
        },
        BlockWrite {
            block: 514,
            page: cleanup,
        },
    ];
    writes.extend(
        crate::workspace::encode(&root, &crate::workspace::INITIAL.collect::<Vec<_>>())?
            .into_iter()
            .map(|(block, page)| BlockWrite { block, page }),
    );
    Ok((root, writes))
}

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "fuzzing"))]
pub(crate) mod model;
