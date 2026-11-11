// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Mounting: device geometry, root selection, and super block operations.

use core::ffi::c_void;
use core::pin::Pin;
use core::ptr;

use core::ops::{Deref, DerefMut};
use kernel::bindings;
use kernel::ffi::c_int;
use kernel::prelude::*;

use kernel::sync::atomic::{Acquire, Atomic, Release};
use kernel::sync::{new_condvar, new_mutex, CondVar, Mutex, MutexGuard};

use crate::edit::Txn;
use crate::glue;
use crate::ondisk::format::{
    self, Root, TreeKind, TreeRef, BLOCK_BYTES, MAINTENANCE_RESERVE, RESERVED_TOTAL_BLOCKS,
};
use crate::tree::{Block, Tree};
use crate::vfs::SuperBlock;
use crate::{commit, corrupt, errno, inode, names};

/// `f_type` reported by statfs: ASCII "CFSX".
pub(crate) const MAGIC: u64 = 0x4346_5358;
const ROOT_A: u64 = 256;
// `SB_*` are macros the bindings omit; their values match the stable
// `MS_RDONLY` and `MS_NOATIME` mount flags.
const SB_RDONLY: usize = 1;
const SB_NOATIME: usize = 1 << 10;
const MIN_DEVICE_BYTES: u64 = 256 << 20;
const MAX_DEVICE_BYTES: u64 = 32 << 40;
/// Timestamp range of FORMAT.md section 5: 1601 through 9999.
const TIME_MIN: i64 = -11_644_473_600;
const TIME_MAX: i64 = 253_402_300_799;

/// Everything a mount reads or changes, under the volume's lock.
pub(crate) struct State {
    /// The committed root, durable in both root slots.
    pub(crate) root: Root,
    /// The open transaction of a writable mount.
    pub(crate) txn: Option<Txn>,
    /// The committed workspace list of a writable mount.
    pub(crate) slots: KVec<u64>,
    /// Set after a failed publication or an edit that stopped part-way;
    /// FORMAT.md section 3 allows no further mutation until remount.
    pub(crate) faulted: bool,
    /// Where the next transaction's data allocations start looking.
    pub(crate) hint: u64,
    /// The periodic-commit timer of a writable mount, from glue.c.
    pub(crate) timer: usize,
    /// Retired data may be reclaimable by a later commit.
    pub(crate) reclaim_pending: bool,
    /// Space held for dirty page-cache units.
    pub(crate) reservations: crate::reserve::Reservations,
    /// Data writes whose blocks the open transaction allocated but no
    /// record owns yet; commit waits for them (`Locked::quiesce`).
    pub(crate) writing: u32,
    /// Data reads in flight without the lock; commit waits for them, so no
    /// block they read is reclaimed and reused under them.
    reading: u32,
    /// Commits waiting for I/O; new reads wait for them, so that a stream
    /// of reads cannot starve a commit.
    committing: u32,
    /// Objects with data writes in flight or a size change under way; room
    /// for `CLAIMS` is reserved at mount. On the heap: inline, the table
    /// overflowed the kernel stack while `fill_super` built the state.
    flights: KVec<Flight>,
}

/// Objects written at once; more writers wait, which bounds the data
/// writes in flight.
const CLAIMS: usize = 64;
/// Batches of one object in flight at once, so that its next batch is
/// prepared and submitted while one is written.
pub(crate) const PIPELINE: usize = 2;

/// One object's data writes in flight. One writer at a time prepares and
/// allocates a batch; up to `PIPELINE` batches are then written at once
/// and recorded in allocation order. A size change holds the object alone.
#[derive(Clone, Copy, Default)]
struct Flight {
    object: u64,
    writer: bool,
    exclusive: bool,
    /// Batches allocated and not yet recorded.
    outstanding: usize,
    /// Tickets handed to allocated batches, and the next to record.
    issued: u64,
    recorded: u64,
    /// The size the last allocated batch records.
    planned: u64,
    /// Completions that came before their turn: ticket, batch, errno.
    parked: [(u64, usize, i32); PIPELINE],
    parked_count: usize,
}

impl State {
    pub(crate) fn tree_root(&self, kind: TreeKind) -> TreeRef {
        match kind {
            TreeKind::Catalog => self.root.catalog,
            TreeKind::Allocation => self.root.allocation,
            _ => self.root.cleanup,
        }
    }

    /// Whether a commit may free blocks: retired data, committed or in the
    /// open transaction, awaits reclamation.
    pub(crate) fn reclaimable(&self) -> bool {
        self.reclaim_pending || self.txn.as_ref().is_some_and(|txn| !txn.retired.is_empty())
    }

    /// Changes whenever a record may have: identifies the state a record
    /// was read in.
    pub(crate) fn stamp(&self) -> (u64, u64) {
        (
            self.root.generation,
            self.txn.as_ref().map_or(0, |txn| txn.edits),
        )
    }

    /// Free blocks, counting the open transaction's changes.
    pub(crate) fn free_blocks(&self) -> u64 {
        self.txn.as_ref().map_or(self.root.free_blocks, |txn| {
            txn.free_blocks(self.root.free_blocks)
        })
    }
}

/// A mounted volume, in `s_fs_info`.
#[pin_data]
pub(crate) struct Volume {
    pub(crate) uid: bindings::kuid_t,
    pub(crate) gid: bindings::kgid_t,
    pub(crate) writable: bool,
    #[pin]
    state: Mutex<State>,
    /// Held through each `writepages` call; see there.
    #[pin]
    pub(crate) writeback: Mutex<()>,
    /// The mount's background work, from glue.c: catalog prefetch, and
    /// writeback started at close. Stopped by `kill_sb`.
    background: usize,
    /// Set with `State::faulted`, for writes that skip the lock.
    fault_seen: Atomic<u32>,
    /// The block after the last small file read, and how many small reads
    /// in a row followed on from the one before: `file::small_read` reads
    /// ahead only for a run of them.
    pub(crate) next_small: Atomic<u64>,
    pub(crate) small_run: Atomic<u32>,
    /// Signalled when `State::writing`, `reading`, or `committing` drops.
    #[pin]
    written: CondVar,
}

impl Volume {
    // Metadata reads and writes share this lock.
    /// Lock the volume, in a NOFS allocation scope: reclaim that wrote back
    /// this filesystem's pages would wait for the lock.
    pub(crate) fn lock(&self) -> Locked<'_> {
        let nofs = NoFs::new();
        Locked {
            state: self.state.lock(),
            volume: self,
            _nofs: nofs,
        }
    }
}

/// A `memalloc_nofs_save` scope, restored on drop.
pub(crate) struct NoFs(u32);

impl NoFs {
    pub(crate) fn new() -> Self {
        // SAFETY: Saving and restoring the task's allocation scope nests.
        Self(unsafe { glue::carryfs_nofs_save() })
    }
}

impl Drop for NoFs {
    fn drop(&mut self) {
        // SAFETY: Restores what `new` saved, in nesting order.
        unsafe { glue::carryfs_nofs_restore(self.0) };
    }
}

/// The locked volume state. The lock is released before the scope ends.
pub(crate) struct Locked<'a> {
    state: MutexGuard<'a, State>,
    volume: &'a Volume,
    _nofs: NoFs,
}

impl Locked<'_> {
    /// Wait until no data write holds unrecorded blocks, releasing the lock
    /// meanwhile, so a publication never sees blocks allocated but unowned.
    pub(crate) fn wait_for_writes(&mut self) {
        while self.state.writing > 0 {
            self.volume.written.wait(&mut self.state);
        }
    }

    /// End a data write counted in `State::writing`.
    pub(crate) fn end_write(&mut self) {
        self.state.writing -= 1;
        self.volume.written.notify_all();
    }

    /// Wait for data writes and reads in flight before a commit, holding
    /// new reads back meanwhile; `end_quiesce` lets them go.
    pub(crate) fn quiesce(&mut self) {
        self.state.committing += 1;
        while self.state.writing > 0 || self.state.reading > 0 {
            self.volume.written.wait(&mut self.state);
        }
    }

    pub(crate) fn end_quiesce(&mut self) {
        self.state.committing -= 1;
        self.volume.written.notify_all();
    }

    /// Count a data read that continues without the lock.
    pub(crate) fn start_read(&mut self) {
        while self.state.committing > 0 {
            self.volume.written.wait(&mut self.state);
        }
        self.state.reading += 1;
    }

    pub(crate) fn end_read(&mut self) {
        self.state.reading -= 1;
        self.volume.written.notify_all();
    }

    /// Wait until `ready` holds for `object`'s flight, or a new one could
    /// be made; the flight's index, made if missing.
    fn flight(&mut self, object: u64, ready: impl Fn(&Flight) -> bool) -> usize {
        loop {
            let state = &mut *self.state;
            match state.find_flight(object) {
                Some(at) if ready(&state.flights[at]) => return at,
                None if state.flights.len() < CLAIMS => {
                    let flight = Flight {
                        object,
                        ..Default::default()
                    };
                    // Within the capacity reserved at mount: never fails.
                    if state.flights.push_within_capacity(flight).is_ok() {
                        return state.flights.len() - 1;
                    }
                }
                _ => {}
            }
            self.volume.written.wait(&mut self.state);
        }
    }

    /// Become `object`'s writer: the one preparing and allocating a batch,
    /// once fewer than `PIPELINE` are in flight.
    pub(crate) fn claim_writer(&mut self, object: u64) {
        let at = self.flight(object, |flight| {
            !flight.writer && !flight.exclusive && flight.outstanding < PIPELINE
        });
        self.state.flights[at].writer = true;
    }

    /// Stop being `object`'s writer, its batch allocated or abandoned.
    pub(crate) fn end_writer(&mut self, object: u64) {
        if let Some(at) = self.state.find_flight(object) {
            self.state.flights[at].writer = false;
            self.state.drop_idle_flight(at);
        }
        self.volume.written.notify_all();
    }

    /// Hold `object` alone for a size change, with no write in flight.
    pub(crate) fn claim_exclusive(&mut self, object: u64) {
        let at = self.flight(object, |flight| {
            !flight.writer && !flight.exclusive && flight.outstanding == 0
        });
        self.state.flights[at].exclusive = true;
    }

    pub(crate) fn end_exclusive(&mut self, object: u64) {
        if let Some(at) = self.state.find_flight(object) {
            self.state.flights[at].exclusive = false;
            self.state.drop_idle_flight(at);
        }
        self.volume.written.notify_all();
    }

    /// Wait until the batch with `ticket` is next to record.
    pub(crate) fn wait_turn(&mut self, object: u64, ticket: u64) {
        while !self.state.is_turn(object, ticket) {
            self.volume.written.wait(&mut self.state);
        }
    }

    /// Wait until no batch of `object` is in flight, so its record matches
    /// its blocks on disk.
    pub(crate) fn wait_recorded(&mut self, object: u64) {
        while self.state.planned_size(object).is_some() {
            self.volume.written.wait(&mut self.state);
        }
    }

    pub(crate) fn notify(&self) {
        self.volume.written.notify_all();
    }
}

impl State {
    fn find_flight(&self, object: u64) -> Option<usize> {
        self.flights.iter().position(|flight| flight.object == object)
    }

    fn drop_idle_flight(&mut self, at: usize) {
        let flight = &self.flights[at];
        if !flight.writer && !flight.exclusive && flight.outstanding == 0 && flight.parked_count == 0 {
            let last = self.flights.len() - 1;
            self.flights.swap(at, last);
            self.flights.pop();
        }
    }

    /// The size `object`'s batches in flight will leave recorded, if any.
    pub(crate) fn planned_size(&self, object: u64) -> Option<u64> {
        self.find_flight(object)
            .map(|at| &self.flights[at])
            .filter(|flight| flight.outstanding > 0)
            .map(|flight| flight.planned)
    }

    /// Hand the writer's allocated batch, which records `size`, its ticket.
    pub(crate) fn issue(&mut self, object: u64, size: u64) -> u64 {
        let Some(at) = self.find_flight(object) else {
            return 0;
        };
        let flight = &mut self.flights[at];
        flight.outstanding += 1;
        flight.planned = size;
        flight.issued += 1;
        flight.issued - 1
    }

    pub(crate) fn is_turn(&self, object: u64, ticket: u64) -> bool {
        self.find_flight(object)
            .is_some_and(|at| self.flights[at].recorded == ticket)
    }

    /// Note the batch whose turn it was recorded.
    pub(crate) fn recorded(&mut self, object: u64) {
        if let Some(at) = self.find_flight(object) {
            self.flights[at].recorded += 1;
            self.flights[at].outstanding = self.flights[at].outstanding.saturating_sub(1);
            self.drop_idle_flight(at);
        }
    }

    /// Keep a completed batch whose turn has not come, as `batch`, an
    /// address, with its errno.
    pub(crate) fn park(&mut self, object: u64, ticket: u64, batch: usize, errno: i32) {
        if let Some(at) = self.find_flight(object) {
            let flight = &mut self.flights[at];
            // Fewer than `PIPELINE` wait: the batch whose turn it is has not
            // completed.
            if let Some(slot) = flight.parked.get_mut(flight.parked_count) {
                *slot = (ticket, batch, errno);
                flight.parked_count += 1;
            }
        }
    }

    /// The parked batch whose turn it is, if any.
    pub(crate) fn take_parked(&mut self, object: u64) -> Option<(usize, i32)> {
        let at = self.find_flight(object)?;
        let flight = &mut self.flights[at];
        let position = flight.parked[..flight.parked_count]
            .iter()
            .position(|(ticket, _, _)| *ticket == flight.recorded)?;
        let (_, batch, errno) = flight.parked[position];
        flight.parked_count -= 1;
        flight.parked[position] = flight.parked[flight.parked_count];
        Some((batch, errno))
    }
}

impl Deref for Locked<'_> {
    type Target = State;

    fn deref(&self) -> &State {
        &self.state
    }
}

impl DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}

impl Volume {
    /// Whether the volume faulted, without the lock.
    pub(crate) fn faulted(&self) -> bool {
        self.fault_seen.load(Acquire) != 0
    }
}

/// Fault the volume: FORMAT.md section 3 allows no further mutation until
/// remount.
pub(crate) fn fault(sb: *mut bindings::super_block, state: &mut State) {
    state.faulted = true;
    of(sb).fault_seen.store(1, Release);
}

/// The volume of a mounted super block.
pub(crate) fn of<'a>(sb: *mut bindings::super_block) -> &'a Volume {
    // SAFETY: `fill_super` sets `s_fs_info` before the super block is
    // reachable, and `carryfs_free_volume` frees it only after
    // `kill_block_super` has shut every user down.
    unsafe { &*((*sb).s_fs_info as *const Volume) }
}

/// Fill a new super block; called by `get_tree_bdev` through glue.c.
#[no_mangle]
unsafe extern "C" fn carryfs_fill_super(
    sb: *mut bindings::super_block,
    uid: u32,
    gid: u32,
    silent: c_int,
) -> c_int {
    // SAFETY: The VFS passes a new, locked super block on the claimed device.
    match unsafe { fill_super(sb, uid, gid, silent != 0) } {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

/// Stop the periodic commit before unmounting; glue.c's `kill_sb` calls it
/// before `kill_block_super`, whose final sync commits what remains.
#[no_mangle]
unsafe extern "C" fn carryfs_stop_timer(sb: *mut bindings::super_block) {
    // SAFETY: `kill_sb` passes a super block that may not have finished
    // mounting; only one with a volume can have a timer.
    if unsafe { (*sb).s_fs_info }.is_null() {
        return;
    }
    let timer = core::mem::take(&mut of(sb).lock().timer);
    // SAFETY: The timer and background work were started by `fill_super`
    // and are stopped once, `kill_sb` running once; the lock is not held,
    // since their work takes it.
    unsafe {
        glue::carryfs_timer_stop(timer as *mut c_void);
        glue::carryfs_background_stop(of(sb).background as *mut c_void);
    }
}

/// The mount's background work for glue.c, or null if it has none.
#[no_mangle]
unsafe extern "C" fn carryfs_volume_background(sb: *mut bindings::super_block) -> *mut c_void {
    of(sb).background as *mut c_void
}

/// Catalog pages read ahead after mounting: 16 MiB, the FUSE adapter's
/// budget.
const PREFETCH_PAGES: usize = 4_096;
/// Catalog leaves a read-only SSD mount reads, in key order, before it
/// returns: 1 MiB, about 4 ms on a USB SSD.
const MOUNT_READ_ONLY_LEAVES: usize = 256;

/// Read the committed catalog ahead after mounting a disk that seeks;
/// glue.c's background work queues it.
#[no_mangle]
unsafe extern "C" fn carryfs_prefetch_catalog(sb: *mut bindings::super_block) {
    // Only a cache warms or not: a page a commit has since replaced or a
    // read failure ends the walk, and use reports real errors.
    let _ = committed_catalog(sb).prefetch(PREFETCH_PAGES, false);
}

/// The committed catalog, without the open transaction's drafts.
fn committed_catalog<'a>(sb: *mut bindings::super_block) -> Tree<'a> {
    let state = of(sb).lock();
    Tree {
        sb,
        uuid: state.root.uuid,
        blocks: state.root.blocks,
        kind: TreeKind::Catalog,
        root: crate::tree::Link::Disk(state.root.catalog),
        generation: state.root.generation,
        drafts: &[],
    }
}

/// Free the volume after `kill_block_super`, whether or not mounting
/// finished; glue.c's `kill_sb` calls it.
#[no_mangle]
unsafe extern "C" fn carryfs_free_volume(volume: *mut c_void) {
    if !volume.is_null() {
        // SAFETY: `fill_super` made `volume` from a pinned `KBox`, and the
        // super block is gone, so nothing borrows it.
        drop(unsafe { Pin::new_unchecked(KBox::from_raw(volume as *mut Volume)) });
    }
}

fn geometry(sb: *mut bindings::super_block, silent: bool) -> Result<u64> {
    // SAFETY: `sb` holds an open block device.
    let (bytes, logical, physical) = unsafe {
        (
            glue::carryfs_bdev_bytes(sb),
            glue::carryfs_bdev_logical_block_size(sb),
            glue::carryfs_bdev_physical_block_size(sb),
        )
    };
    let sectors = matches!((logical, physical), (512, 512 | 4096) | (4096, 4096));
    if !(MIN_DEVICE_BYTES..=MAX_DEVICE_BYTES).contains(&bytes)
        || bytes % BLOCK_BYTES as u64 != 0
        || !sectors
    {
        if !silent {
            pr_err!("unsupported geometry: {bytes} bytes, {logical}/{physical}-byte sectors\n");
        }
        return Err(EINVAL);
    }
    Ok(bytes / BLOCK_BYTES as u64)
}

/// The decoded root in `slot`, or `None` when its checksum or contents are
/// invalid. A read error fails the mount: that slot may hold the newer root.
fn read_root(sb: *mut bindings::super_block, slot: u64, blocks: u64) -> Result<Option<Root>> {
    let block = Block::read(sb, slot)?;
    format::supported_root(Root::decode(block.data(), slot, blocks)).map_err(errno)
}

/// # Safety
///
/// `sb` must be a new super block that `get_tree_bdev` is filling.
unsafe fn fill_super(sb: *mut bindings::super_block, uid: u32, gid: u32, silent: bool) -> Result {
    // SAFETY: `sb` is a new super block on a block device.
    if unsafe { bindings::sb_set_blocksize(sb, BLOCK_BYTES as c_int) } == 0 {
        if !silent {
            pr_err!("device sectors are larger than {BLOCK_BYTES} bytes\n");
        }
        return Err(EINVAL);
    }
    // SAFETY: As above.
    let writable = unsafe { (*sb).s_flags } & SB_RDONLY == 0;
    let blocks = geometry(sb, silent)?;
    let a = read_root(sb, ROOT_A, blocks)?;
    let b = read_root(sb, blocks - 257, blocks)?;
    let root = *format::select_root(a.as_ref(), b.as_ref()).map_err(|error| {
        if !silent {
            if let crate::ondisk::error::Error::Corrupt(reason) = error {
                pr_err!("no usable root: {reason}\n");
            }
        }
        errno(error)
    })?;
    let flights = KVec::with_capacity(CLAIMS, GFP_KERNEL)?;
    // SAFETY: Its work runs only once queued, after mounting.
    let background = unsafe { glue::carryfs_background_new(sb) };
    let volume = KBox::pin_init(
        try_pin_init!(Volume {
            uid: bindings::kuid_t { val: uid },
            gid: bindings::kgid_t { val: gid },
            writable,
            writeback <- new_mutex!(()),
            background: background as usize,
            written <- new_condvar!(),
            fault_seen: Atomic::new(0),
            next_small: Atomic::new(0),
            small_run: Atomic::new(0),
            state <- new_mutex!(State {
                root,
                txn: None,
                slots: KVec::new(),
                faulted: false,
                hint: 0,
                timer: 0,
                reclaim_pending: true,
                reservations: Default::default(),
                writing: 0,
                reading: 0,
                committing: 0,
                flights,
            }),
        }),
        GFP_KERNEL,
    )
    .inspect_err(|_| {
        // SAFETY: Never queued, and owned here alone.
        unsafe { glue::carryfs_background_stop(background) }
    })?;
    // SAFETY: `sb` is not yet visible to anyone else; `carryfs_free_volume`
    // unpins and frees the volume.
    unsafe {
        (*sb).s_fs_info = KBox::into_raw(Pin::into_inner_unchecked(volume)) as *mut c_void;
        (*sb).s_magic = MAGIC as _;
        (*sb).s_op = &SUPER_OPERATIONS.0;
        (*sb).s_maxbytes = i64::MAX;
        (*sb).s_time_gran = 1;
        (*sb).s_time_min = TIME_MIN;
        (*sb).s_time_max = TIME_MAX;
        (*sb).s_flags |= SB_NOATIME;
        glue::carryfs_set_uuid(sb, root.uuid.as_ptr());
        bindings::set_default_d_op(sb, &names::DENTRY_OPERATIONS.0);
    }
    {
        let state = of(sb).lock();
        for kind in [TreeKind::Catalog, TreeKind::Allocation, TreeKind::Cleanup] {
            Tree::new(sb, &state, kind).check_root()?;
        }
    }
    // SSD mounts prefetch synchronously to avoid competing with first listings.
    // Writable mounts warm the catalog; read-only mounts warm its first leaves.
    // Rotating disks use background prefetch to keep mount latency bounded.
    // SAFETY: `sb` holds its block device.
    let ssd = unsafe { glue::carryfs_bdev_nonrot(sb) };
    if ssd && writable {
        // Before validation, which walks the whole catalog: cold, it would
        // wait for each page in turn.
        let _ = committed_catalog(sb).prefetch(PREFETCH_PAGES, true);
    }
    if writable {
        commit::open(sb, &mut of(sb).lock(), [a, b])?;
        let orphans = commit::orphans(sb, &of(sb).lock())?;
        for object in orphans.iter() {
            crate::namei::remove_object(sb, *object)?;
        }
        // SAFETY: `kill_sb` stops the timer before the volume goes away.
        let timer = unsafe { glue::carryfs_timer_start(sb) };
        if timer.is_null() {
            return Err(ENOMEM);
        }
        of(sb).lock().timer = timer as usize;
    }

    if ssd && !writable {
        let _ = committed_catalog(sb).read_first_leaves(MOUNT_READ_ONLY_LEAVES);
    } else if !ssd && of(sb).background != 0 {
        // SAFETY: `kill_sb` stops the work before the volume goes away.
        unsafe { glue::carryfs_prefetch_queue(of(sb).background as *mut c_void) };
    }

    let root_inode = inode::get(sb, inode::ROOT)?;
    // SAFETY: `get` returned a referenced inode.
    if unsafe { (*root_inode).i_mode } as u32 & bindings::S_IFMT != bindings::S_IFDIR {
        // SAFETY: Drop the reference `get` took.
        unsafe { bindings::iput(root_inode) };
        return Err(corrupt(
            root.catalog.block,
            crate::ondisk::error::Error::Corrupt("root is not a directory"),
        ));
    }
    // SAFETY: `d_make_root` consumes the inode reference, even on failure.
    let dentry = unsafe { bindings::d_make_root(root_inode) };
    if dentry.is_null() {
        return Err(ENOMEM);
    }
    // SAFETY: `sb` is still private to this mount.
    unsafe { (*sb).s_root = dentry };
    Ok(())
}

unsafe extern "C" fn statfs(dentry: *mut bindings::dentry, buf: *mut bindings::kstatfs) -> c_int {
    // SAFETY: The VFS passes a live dentry of this filesystem.
    let sb = unsafe { SuperBlock::from_raw((*dentry).d_sb) };
    // Space reserved for dirty data is not free to anyone else.
    let (root, free) = {
        let state = sb.volume().lock();
        (state.root, crate::reserve::available(&state))
    };
    let fsid = root.uuid;
    let word = |offset: usize| {
        u32::from_le_bytes([
            fsid[offset],
            fsid[offset + 1],
            fsid[offset + 2],
            fsid[offset + 3],
        ])
    };
    // SAFETY: `buf` is the caller's statfs buffer.
    unsafe {
        glue::carryfs_statfs_fill(
            buf,
            MAGIC,
            BLOCK_BYTES as u64,
            root.blocks.saturating_sub(RESERVED_TOTAL_BLOCKS),
            free,
            free.saturating_sub(MAINTENANCE_RESERVE),
            255,
            word(0) ^ word(8),
            word(4) ^ word(12),
        )
    };
    0
}

unsafe extern "C" fn show_options(
    seq: *mut bindings::seq_file,
    root: *mut bindings::dentry,
) -> c_int {
    // SAFETY: The VFS passes the mount's root dentry.
    let volume = of(unsafe { (*root).d_sb });
    // SAFETY: Translating IDs into the initial namespace is always valid, and
    // the format string takes two unsigned ints.
    unsafe {
        let uid = bindings::from_kuid_munged(ptr::addr_of_mut!(bindings::init_user_ns), volume.uid);
        let gid = bindings::from_kgid_munged(ptr::addr_of_mut!(bindings::init_user_ns), volume.gid);
        bindings::seq_printf(seq, c",uid=%u,gid=%u".as_ptr().cast(), uid, gid);
    }
    0
}

unsafe extern "C" fn sync_fs(sb: *mut bindings::super_block, _wait: c_int) -> c_int {
    // SAFETY: The VFS passes a mounted super block of this filesystem.
    let volume = unsafe { SuperBlock::from_raw(sb) }.volume();
    if !volume.writable {
        return 0;
    }
    let mut state = volume.lock();
    match commit::commit(sb, &mut state) {
        Ok(()) => 0,
        Err(error) => error.to_errno(),
    }
}

static SUPER_OPERATIONS: inode::Ops<bindings::super_operations> =
    inode::Ops(bindings::super_operations {
        free_inode: Some(inode::free_inode),
        evict_inode: Some(inode::evict_inode),
        statfs: Some(statfs),
        show_options: Some(show_options),
        sync_fs: Some(sync_fs),
        // SAFETY: All-zero entries are absent operations.
        ..unsafe { core::mem::zeroed() }
    });
