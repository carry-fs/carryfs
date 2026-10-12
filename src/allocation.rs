//! Allocation and retirement are planned together, before any device writes.

use std::collections::{BTreeMap, BTreeSet};

use crate::format::{Page, Root, TreeKind, TreeRef, reserved_group_blocks};
use crate::tree::{Cache, Edge, Forest, Record, tree_ref, u64_be, u64_le};
use crate::{BlockDevice, Error, Result, workspace};

pub(crate) const GROUP: u64 = 16_384;
const BITMAP: usize = 2048;
const MAX_PAGES: usize = 4096;
// A new allocation record can split each of the eight permitted tree levels
// and add one root. Charge that complete net-growth bound before selecting raw
// blocks from an implicit bitmap; a short final group cannot fund itself.
const MAX_BITMAP_INSERTION_NET_PAGES: usize = 9;
pub(crate) const RESERVE: u64 = 16_384;
// Balancing publishes alone and keeps the workspace within TARGET ± BATCH
// before any cleanup plan. One plan returns at most MAX_PAGES superseded
// pages, so a successor never exceeds the bank capacity.
const TARGET: usize = 8192;
const BATCH: usize = 128;
const _: () = assert!(TARGET + BATCH + MAX_PAGES <= workspace::CAPACITY);
// Net metadata growth allowed for one maintenance step in the backlog tests.
// Measured maximum is 2 pages. Refill runs
// before cleanup below TARGET - BATCH, which is 3,968 slots above the floor.
#[cfg(test)]
const MAX_STEP_GROWTH: usize = 16;

// A deferred chain checkpoints before a plan could find fewer usable slots
// than the largest plan plus a workspace release.
pub(crate) const CHECKPOINT_SLOTS: usize = MAX_PAGES + BATCH;
// A batch of operations keeps well inside the per-plan limits, so that the
// pages `seal` adds cannot push a batch that fits over them.
// Tests use a small bound so that ordinary operations reach it: refusals,
// compaction and sealing get exercised without thousands of operations.
const MAX_DRAFTS_IN_BATCH: usize = if cfg!(test) {
    128
} else {
    crate::tree::MAX_DRAFTS / 2
};
// File data a batch may send before it is sealed: 16 MiB. Each accepted
// operation's pages leave at once (`Forest::send_data`), so this bounds
// unflushed data between publications, not memory.
const MAX_BATCH_DATA: usize = 4096;
const MAX_BATCH_READS: usize = 1 << 18;

pub(crate) struct Plan {
    base: Root,
    root: Root,
    writes: Vec<(u64, Page)>,
    returned: Vec<u64>,
    slots: Option<Vec<u64>>,
    built: Vec<crate::tree::Built>,
    data_pages: usize,
}

/// A plan's successor root, its writes, the superseded metadata pages it
/// returned to the workspace, its workspace list when that changed, and the
/// tree pages it built, for the volume to cache once they are published, and
/// the file data pages its transaction already sent to the device. A chained
/// plan leaves the list unencoded for the checkpoint to write.
pub(crate) type PlanParts = (
    Root,
    Vec<(u64, Page)>,
    Vec<u64>,
    Option<Vec<u64>>,
    Vec<crate::tree::Built>,
    usize,
);

impl Plan {
    pub(crate) fn into_parts(self, base: &Root) -> Result<PlanParts> {
        if self.base != *base {
            return Err(Error::InvalidInput(
                "allocation plan belongs to a different root",
            ));
        }
        Ok((
            self.root,
            self.writes,
            self.returned,
            self.slots,
            self.built,
            self.data_pages,
        ))
    }
}

static NO_BLOCKS: BTreeSet<u64> = BTreeSet::new();

/// Merge two ascending lists of blocks, rejecting one that appears in both.
fn merge_slots(a: &[u64], b: &[u64]) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut left, mut right) = (0, 0);
    while left < a.len() && right < b.len() {
        match a[left].cmp(&b[right]) {
            std::cmp::Ordering::Less => {
                out.push(a[left]);
                left += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[right]);
                right += 1;
            }
            std::cmp::Ordering::Equal => {
                return Err(Error::InvalidInput("workspace ownership overlaps"));
            }
        }
    }
    out.extend_from_slice(&a[left..]);
    out.extend_from_slice(&b[right..]);
    Ok(out)
}

/// Split ascending `slots` into those a plan may use and those it must keep
/// unused, dropping released ones. Sparse changes copy unchanged runs;
/// dense changes use a merge pass instead of per-slot set lookups.
fn partition_slots(
    slots: &[u64],
    release: &BTreeSet<u64>,
    excluded: &BTreeSet<u64>,
) -> (Vec<u64>, Vec<u64>) {
    let (mut usable, mut kept) = (Vec::with_capacity(slots.len()), Vec::new());
    // Bound the searches for sparse changes; dense sets keep linear cost.
    if release.len().saturating_add(excluded.len()) <= slots.len() / 32 {
        let mut from = 0;
        for block in release.union(excluded) {
            let at = from + slots[from..].partition_point(|slot| slot < block);
            usable.extend_from_slice(&slots[from..at]);
            from = at;
            if slots.get(at) == Some(block) {
                if !release.contains(block) {
                    kept.push(*block);
                }
                from += 1;
            }
        }
        usable.extend_from_slice(&slots[from..]);
        return (usable, kept);
    }
    let mut release = release.iter().peekable();
    let mut excluded = excluded.iter().peekable();
    for &slot in slots {
        while release.next_if(|block| **block < slot).is_some() {}
        while excluded.next_if(|block| **block < slot).is_some() {}
        if release.peek() == Some(&&slot) {
            continue;
        }
        if excluded.peek() == Some(&&slot) {
            kept.push(slot);
        } else {
            usable.push(slot);
        }
    }
    (usable, kept)
}

/// What a volume supplies beyond the base root: its verified-metadata cache and,
/// for plans chained between checkpoints, the last durable root and the
/// workspace slots that must not be written or released before the next one.
pub(crate) struct Context<'a> {
    pub cache: &'a mut Cache,
    pub durable: Option<(&'a Root, &'a BTreeSet<u64>)>,
}

#[derive(Clone, Copy, Debug)]
struct Range {
    start: u64,
    count: u64,
}

struct Job {
    key: Vec<u8>,
    owner: u64,
    low: u8,
    high: u8,
    tree: Edge,
}

pub(crate) struct Transaction<'a, D> {
    pub(crate) forest: Forest<'a, D>,
    pub(crate) catalog: Edge,
    allocation: Edge,
    cleanup: Edge,
    updates: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    jobs: Vec<Job>,
    freed: Vec<Range>,
    retired_data: Vec<Range>,
    next_job: u64,
    next_object: u64,
    allocated: BTreeSet<u64>,
    // Blocks `allocate_data` handed out, in order, so undoing one operation
    // releases exactly the blocks it took.
    allocation_log: Vec<u64>,
    workspace: Vec<u64>,
    refill: BTreeSet<u64>,
    release: BTreeSet<u64>,
    cursor: u64,
    // The newest generation both roots durably reference; newer retirement
    // still backs that state and is not reclaimed.
    durable_generation: u64,
    // Slots holding pages the durable state may reference.
    excluded: &'a BTreeSet<u64>,
    // The bank the durable roots do not select.
    bank: TreeRef,
    // Chained plans leave the bank for the checkpoint to encode and write.
    chained: bool,
    // Workspace slots a plan may use, counted once for a batch of operations.
    usable: Option<usize>,
}

/// A transaction between the operations of one batch. It borrows nothing, so
/// the volume can hold it; the base root, durable root, and excluded slots do
/// not change until the batch is sealed and published.
pub(crate) struct Pending {
    forest: crate::tree::Drafts,
    catalog: Edge,
    allocation: Edge,
    cleanup: Edge,
    updates: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    jobs: Vec<Job>,
    freed: Vec<Range>,
    retired_data: Vec<Range>,
    next_job: u64,
    next_object: u64,
    allocated: BTreeSet<u64>,
    allocation_log: Vec<u64>,
    workspace: Vec<u64>,
    refill: BTreeSet<u64>,
    release: BTreeSet<u64>,
    cursor: u64,
    durable_generation: u64,
    bank: TreeRef,
    chained: bool,
    usable: Option<usize>,
}

impl Pending {
    /// Data blocks the batch has allocated, which `space` reports as used.
    pub(crate) fn allocated_blocks(&self) -> usize {
        self.allocated.len()
    }

    /// Run a read-only operation against the batch's catalog, so it sees edits
    /// no published root holds yet. It gets a read budget of its own, and the
    /// batch is checked before anything moves, so a read cannot lose it.
    pub(crate) fn read<D: BlockDevice, T>(
        &mut self,
        device: &mut D,
        base: &Root,
        cache: &mut crate::tree::Cache,
        read: impl FnOnce(&mut Forest<'_, D>, Edge) -> Result<T>,
    ) -> Result<T> {
        if !self.forest.follows(base) {
            return Err(Error::InvalidInput("batch read against another root"));
        }
        let drafts = std::mem::take(&mut self.forest);
        let mut forest = Forest::resume(device, base, Some(cache), drafts)?;
        let reads = forest.reads();
        forest.set_reads(0);
        let result = read(&mut forest, self.catalog.clone());
        forest.set_reads(reads);
        self.forest = forest.suspend();
        result
    }
}

/// Where a transaction stood before one operation. Foreground edits change
/// only the state recorded here; `updates`, `freed`, the workspace, and the
/// refill and release sets belong to maintenance, which never runs in a batch.
pub(crate) struct Undo {
    forest: crate::tree::Mark,
    catalog: Edge,
    cleanup: Edge,
    next_job: u64,
    next_object: u64,
    jobs: usize,
    retired_data: usize,
    allocations: usize,
}

fn implicit_bitmap(group: u64, blocks: u64) -> Vec<u8> {
    let mut bits = vec![0; BITMAP];
    for block in reserved_group_blocks(group, blocks) {
        set(&mut bits, block, true);
    }
    bits
}

fn bit(bits: &[u8], block: u64) -> bool {
    bits[(block % GROUP) as usize / 8] & (1 << (block % 8)) != 0
}

fn set(bits: &mut [u8], block: u64, value: bool) {
    let byte = &mut bits[(block % GROUP) as usize / 8];
    let mask = 1 << (block % 8);
    if value {
        *byte |= mask;
    } else {
        *byte &= !mask;
    }
}

impl<'a, D: BlockDevice> Transaction<'a, D> {
    #[cfg(test)]
    pub(crate) fn new(device: &'a mut D, base: &'a Root) -> Result<Self> {
        Self::cached(device, base, None)
    }

    pub(crate) fn cached(
        device: &'a mut D,
        base: &'a Root,
        context: Option<Context<'a>>,
    ) -> Result<Self> {
        let chained = matches!(&context, Some(context) if context.durable.is_some());
        let (mut cache, (durable, excluded)) = match context {
            Some(context) => (
                Some(context.cache),
                context.durable.unwrap_or((base, &NO_BLOCKS)),
            ),
            None => (None, (base, &NO_BLOCKS)),
        };
        let bank = TreeRef {
            block: workspace::next_reference(durable)?.block,
            generation: base.generation.checked_add(1).ok_or(Error::LimitExceeded)?,
        };
        let memo = cache
            .as_ref()
            .and_then(|cache| cache.workspace.as_ref())
            .filter(|(reference, _)| *reference == base.workspace)
            .map(|(_, slots)| slots.clone());
        let workspace = match memo {
            Some(slots) => slots,
            None => {
                let slots = workspace::read(device, base)?;
                if let Some(cache) = cache.as_deref_mut() {
                    cache.workspace = Some((base.workspace, slots.clone()));
                }
                slots
            }
        };
        let mut forest = Forest::cached(device, base, cache)?;
        let catalog = forest.root(base.catalog, TreeKind::Catalog)?;
        let allocation = forest.root(base.allocation, TreeKind::Allocation)?;
        let cleanup = forest.root(base.cleanup, TreeKind::Cleanup)?;
        Ok(Self {
            forest,
            catalog,
            allocation,
            cleanup,
            updates: BTreeMap::new(),
            jobs: Vec::new(),
            freed: Vec::new(),
            retired_data: Vec::new(),
            next_job: base.next_job,
            next_object: base.next_object,
            allocated: BTreeSet::new(),
            allocation_log: Vec::new(),
            workspace,
            refill: BTreeSet::new(),
            release: BTreeSet::new(),
            cursor: base.workspace_cursor,
            durable_generation: durable.generation,
            excluded,
            bank,
            chained,
            usable: None,
        })
    }

    /// Continue a batch against the same base root and context it began with.
    pub(crate) fn resume(
        device: &'a mut D,
        base: &'a Root,
        context: Context<'a>,
        pending: Pending,
    ) -> Result<Self> {
        let excluded = context.durable.map_or(&NO_BLOCKS, |(_, excluded)| excluded);
        Ok(Self {
            forest: Forest::resume(device, base, Some(context.cache), pending.forest)?,
            catalog: pending.catalog,
            allocation: pending.allocation,
            cleanup: pending.cleanup,
            updates: pending.updates,
            jobs: pending.jobs,
            freed: pending.freed,
            retired_data: pending.retired_data,
            next_job: pending.next_job,
            next_object: pending.next_object,
            allocated: pending.allocated,
            allocation_log: pending.allocation_log,
            workspace: pending.workspace,
            refill: pending.refill,
            release: pending.release,
            cursor: pending.cursor,
            durable_generation: pending.durable_generation,
            excluded,
            bank: pending.bank,
            chained: pending.chained,
            usable: pending.usable,
        })
    }

    pub(crate) fn suspend(mut self) -> Pending {
        let mut roots = vec![&mut self.catalog, &mut self.allocation, &mut self.cleanup];
        roots.extend(self.jobs.iter_mut().map(|job| &mut job.tree));
        self.forest.compact(&mut roots, MAX_DRAFTS_IN_BATCH / 2);
        Pending {
            forest: self.forest.suspend(),
            catalog: self.catalog,
            allocation: self.allocation,
            cleanup: self.cleanup,
            updates: self.updates,
            jobs: self.jobs,
            freed: self.freed,
            retired_data: self.retired_data,
            next_job: self.next_job,
            next_object: self.next_object,
            allocated: self.allocated,
            allocation_log: self.allocation_log,
            workspace: self.workspace,
            refill: self.refill,
            release: self.release,
            cursor: self.cursor,
            durable_generation: self.durable_generation,
            bank: self.bank,
            chained: self.chained,
            usable: self.usable,
        }
    }

    pub(crate) fn mark(&self) -> Undo {
        Undo {
            forest: self.forest.mark(),
            catalog: self.catalog.clone(),
            cleanup: self.cleanup.clone(),
            next_job: self.next_job,
            next_object: self.next_object,
            jobs: self.jobs.len(),
            retired_data: self.retired_data.len(),
            allocations: self.allocation_log.len(),
        }
    }

    /// Undo one operation. Its data blocks were only queued, never written, so
    /// returning them to the batch cannot write any block twice.
    pub(crate) fn rewind(&mut self, undo: Undo) {
        self.forest.rewind(undo.forest);
        self.catalog = undo.catalog;
        self.cleanup = undo.cleanup;
        self.next_job = undo.next_job;
        self.next_object = undo.next_object;
        self.jobs.truncate(undo.jobs);
        self.retired_data.truncate(undo.retired_data);
        for block in self.allocation_log.drain(undo.allocations..) {
            self.allocated.remove(&block);
        }
    }

    /// Whether `seal` cannot fail for this transaction. A batch only holds
    /// acknowledged operations while this holds, so a batch's seal never loses
    /// one; an operation that breaks it is undone and runs as its own plan.
    /// Each bound mirrors a check in `seal`, with worst cases for the pages
    /// `seal` itself adds.
    pub(crate) fn fits_batch(&mut self) -> bool {
        let usable = *self.usable.get_or_insert_with(|| {
            self.workspace
                .iter()
                .filter(|slot| !self.release.contains(slot) && !self.excluded.contains(slot))
                .count()
        });
        let height = [&self.catalog, &self.allocation, &self.cleanup]
            .iter()
            .map(|edge| usize::from(edge.level))
            .max()
            .unwrap_or(0);
        // A put copies one path and may split every page on it and add a root.
        let per_put = 2 * (height + 2);
        // Distinct allocation groups, one bitmap put each. Jumping to the next
        // group keeps this per-operation check from walking every block.
        let mut groups = 0;
        let mut from = 0;
        while let Some(&block) = self.allocated.range(from..).next() {
            groups += 1;
            match (block / GROUP + 1).checked_mul(GROUP) {
                Some(next) => from = next,
                None => break,
            }
        }
        let puts = self.jobs.len() + self.retired_data.len() + self.updates.len() + groups;
        let destinations = self.forest.drafts() + puts * per_put;
        // Retired pages rejoin the workspace and count as free at `seal`: the
        // capacity check takes the most `seal` could add, the free check only
        // what is already retired.
        let retired_now = self.forest.retired.len();
        let retired = retired_now + puts * (height + 1);
        let allocated = self.allocated.len();
        let base = self.forest.base;
        let free = base
            .free_blocks
            .saturating_add(retired_now as u64)
            .saturating_sub((destinations + allocated) as u64);
        let raw = base
            .free_blocks
            .saturating_sub((self.workspace.len() + allocated) as u64);
        destinations <= MAX_DRAFTS_IN_BATCH
            && destinations <= usable
            && self.workspace.len() >= destinations + MAX_PAGES
            && self.workspace.len() + retired <= workspace::CAPACITY
            && free >= RESERVE
            && (allocated == 0 || raw >= RESERVE)
            && destinations + self.forest.held_pages() <= MAX_PAGES
            && self.forest.data_pages() <= MAX_BATCH_DATA
            && self.forest.reads() <= MAX_BATCH_READS
    }

    /// Allocate a fresh object ID for a new catalog inode.
    pub(crate) fn new_object(&mut self) -> Result<u64> {
        let id = self.next_object;
        self.next_object = id
            .checked_add(1)
            .filter(|next| *next <= i64::MAX as u64)
            .ok_or(Error::LimitExceeded)?;
        Ok(id)
    }

    /// Allocate `count` raw free blocks owned by the mirrored base, starting
    /// near the last allocation in group `hint`, and return ascending runs. `seal`
    /// sets their bitmap bits and charges them against the foreground reserve.
    pub(crate) fn allocate_data(&mut self, count: u64, hint: u64) -> Result<Vec<(u64, u64)>> {
        let groups = self.forest.base.blocks.div_ceil(GROUP);
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut needed = count;
        // Without per-group free-space summaries, even full groups require a
        // bitmap lookup and word scan.
        for offset in 0..groups {
            if needed == 0 {
                break;
            }
            let group = (hint % groups + offset) % groups;
            let bitmap = self.bitmap(group)?;
            let first = group * GROUP;
            let end = (first + GROUP).min(self.forest.base.blocks);
            // The allocation set is already saved and restored with the batch.
            let next = self
                .allocated
                .range(first..end)
                .next_back()
                .map_or(first, |b| b + 1);
            let next = if next == end { first } else { next };
            for (from, to) in [(next, end), (first, next)] {
                for word in (from - first) / 64..(to - first).div_ceil(64) {
                    let base = first + word * 64;
                    let bytes = bitmap
                        .get(word as usize * 8..word as usize * 8 + 8)
                        .ok_or(Error::Corrupt("allocation bitmap word length"))?;
                    let mut free = !u64::from_le_bytes(
                        bytes
                            .try_into()
                            .map_err(|_| Error::Corrupt("allocation bitmap word length"))?,
                    );
                    if from > base {
                        free &= u64::MAX << (from - base);
                    }
                    if to < base + 64 {
                        free &= (1 << (to - base)) - 1;
                    }
                    while free != 0 && needed != 0 {
                        let bit = free.trailing_zeros() as u64;
                        free &= free - 1;
                        let block = base + bit;
                        if !self.allocated.insert(block) {
                            continue;
                        }
                        self.allocation_log.push(block);
                        needed -= 1;
                        match runs.last_mut() {
                            Some((start, run)) if *start + *run == block => *run += 1,
                            _ => runs.push((block, 1)),
                        }
                    }
                    if needed == 0 {
                        break;
                    }
                }
            }
        }
        if needed != 0 {
            return Err(Error::NoSpace);
        }
        runs.sort_unstable_by_key(|(start, _)| *start);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(runs.len());
        for (start, count) in runs {
            match merged.last_mut() {
                Some((previous, length)) if *previous + *length == start => *length += count,
                _ => merged.push((start, count)),
            }
        }
        Ok(merged)
    }

    /// Queue the contents of a block allocated by this transaction. Data pages
    /// are written with, and flushed before, the new roots.
    pub(crate) fn write_data(&mut self, block: u64, page: Page) -> Result<()> {
        if !self.allocated.contains(&block) {
            return Err(Error::InvalidInput("data write outside this allocation"));
        }
        self.forest.write_data(block, page)
    }

    /// Retire superseded file data. Maintenance frees it once both roots
    /// durably reference this transaction's generation.
    pub(crate) fn retire_data(&mut self, start: u64, count: u64) {
        self.retired_data.push(Range { start, count });
    }

    /// Detach `owner`'s catalog records in `[low, high)` into a cleanup job
    /// whose record kinds lie in `kinds`, and return the detached extent-block
    /// total. Only boundary paths are copied; maintenance drains the job.
    pub(crate) fn detach_job(
        &mut self,
        owner: u64,
        low: &[u8],
        high: &[u8],
        kinds: (u8, u8),
    ) -> Result<u64> {
        let (catalog, detached) = self.forest.detach(self.catalog.clone(), low, high)?;
        self.catalog = catalog;
        let Some(tree) = detached else {
            return Ok(0);
        };
        let job_id = self.next_job;
        self.next_job = job_id
            .checked_add(1)
            .filter(|next| *next <= i64::MAX as u64)
            .ok_or(Error::LimitExceeded)?;
        let mut key = vec![2];
        key.extend_from_slice(&job_id.to_be_bytes());
        let blocks = tree.data_blocks;
        self.jobs.push(Job {
            key,
            owner,
            low: kinds.0,
            high: kinds.1,
            tree,
        });
        Ok(blocks)
    }

    /// Record a cleanup orphan marker for a just-unlinked object. Bounded
    /// maintenance later detaches its catalog interval and clears the marker.
    pub(crate) fn mark_orphan(&mut self, object: u64) -> Result<()> {
        let mut key = vec![3];
        key.extend_from_slice(&object.to_be_bytes());
        self.cleanup = self
            .forest
            .put(self.cleanup.clone(), TreeKind::Cleanup, &key, &[])?;
        Ok(())
    }

    fn bitmap(&mut self, group: u64) -> Result<Vec<u8>> {
        Ok(self
            .forest
            .lookup(&self.allocation, TreeKind::Allocation, &group.to_be_bytes())?
            .unwrap_or_else(|| implicit_bitmap(group, self.forest.base.blocks)))
    }

    fn retirement(&mut self, records: &[Record]) -> Result<()> {
        let mut remaining = 512;
        for (key, value) in records.iter().take(32) {
            if key[0] != 1 || remaining == 0 {
                break;
            }
            let generation = u64_be(&key[1..9])?;
            if generation > self.forest.base.generation {
                return Err(Error::Corrupt("retirement is newer than mirrored root"));
            }
            // Records are ordered by generation, so the rest are newer too.
            if generation > self.durable_generation {
                break;
            }
            let start = u64_be(&key[9..17])?;
            let count = u64::from(u32::from_le_bytes(
                value[..4]
                    .try_into()
                    .map_err(|_| Error::Corrupt("retirement count length"))?,
            ));
            let take = count.min(remaining);
            self.freed.push(Range { start, count: take });
            self.updates.insert(key.clone(), None);
            if take < count {
                let mut next = key.clone();
                next[9..17].copy_from_slice(&(start + take).to_be_bytes());
                let mut value = vec![0; 8];
                value[..4].copy_from_slice(
                    &u32::try_from(count - take)
                        .map_err(|_| Error::Corrupt("retirement count"))?
                        .to_le_bytes(),
                );
                self.updates.insert(next, Some(value));
            }
            remaining -= take;
        }
        Ok(())
    }

    fn detached(&mut self, key: Vec<u8>, value: &[u8]) -> Result<()> {
        let owner = u64_le(&value[..8])?;
        let low = value[8];
        let high = value[9];
        let mut tree = self
            .forest
            .root(tree_ref(&value[16..])?, TreeKind::Catalog)?;
        let records = self.forest.scan(&tree, TreeKind::Catalog, &[], 32)?;
        let mut remaining = 512;
        for (key, value) in records {
            if u64_be(&key[..8])? != owner || !(low..=high).contains(&key[8]) {
                return Err(Error::Corrupt("detached record owner or kind mismatch"));
            }
            if key[8] == 3 {
                if remaining == 0 {
                    break;
                }
                let physical = u64_le(&value[..8])?;
                let count = u64::from(u32::from_le_bytes(
                    value[8..12]
                        .try_into()
                        .map_err(|_| Error::Corrupt("extent count length"))?,
                ));
                let take = count.min(remaining);
                self.retired_data.push(Range {
                    start: physical,
                    count: take,
                });
                tree = self.forest.remove(tree, TreeKind::Catalog, &key)?;
                if take < count {
                    let mut next_key = key.clone();
                    next_key[9..17].copy_from_slice(
                        &u64_be(&key[9..17])?
                            .checked_add(take)
                            .ok_or(Error::Corrupt("extent logical end overflow"))?
                            .to_be_bytes(),
                    );
                    let mut next_value = value.clone();
                    next_value[..8].copy_from_slice(&(physical + take).to_le_bytes());
                    next_value[8..12].copy_from_slice(
                        &u32::try_from(count - take)
                            .map_err(|_| Error::Corrupt("extent count overflow"))?
                            .to_le_bytes(),
                    );
                    tree = self
                        .forest
                        .put(tree, TreeKind::Catalog, &next_key, &next_value)?;
                }
                remaining -= take;
            } else {
                tree = self.forest.remove(tree, TreeKind::Catalog, &key)?;
            }
        }
        if tree.minimum.is_empty() {
            self.forest.retire_root(&tree);
            self.updates.insert(key, None);
        } else {
            self.jobs.push(Job {
                key,
                owner,
                low: value[8],
                high: value[9],
                tree,
            });
        }
        Ok(())
    }

    fn orphan(&mut self, key: Vec<u8>) -> Result<()> {
        let owner = u64_be(&key[1..])?;
        let mut low = owner.to_be_bytes().to_vec();
        low.push(1);
        let mut high = owner.to_be_bytes().to_vec();
        high.push(5);
        let inode = self
            .forest
            .lookup(&self.catalog, TreeKind::Catalog, &low)?
            .ok_or(Error::Corrupt("orphan marker has no inode"))?;
        if inode[1] & 1 == 0 || owner == 1 {
            return Err(Error::Corrupt("invalid orphan inode"));
        }
        if self.remove_small_object(owner)? {
            self.updates.insert(key, None);
            return Ok(());
        }
        let job_id = self.next_job;
        self.next_job = job_id
            .checked_add(1)
            .filter(|next| *next <= i64::MAX as u64)
            .ok_or(Error::LimitExceeded)?;
        let (catalog, detached) = self.forest.detach(self.catalog.clone(), &low, &high)?;
        self.catalog = catalog;
        let tree = detached.ok_or(Error::Corrupt("orphan interval is empty"))?;
        let mut job_key = vec![2];
        job_key.extend_from_slice(&job_id.to_be_bytes());
        self.jobs.push(Job {
            key: job_key,
            owner,
            low: 1,
            high: 4,
            tree,
        });
        self.updates.insert(key, None);
        Ok(())
    }

    /// Remove an object's catalog records in this transaction when they are
    /// no more than one job drain step would take: fewer than
    /// `DIRECT_RECORDS` records holding at most `DIRECT_BLOCKS` data blocks.
    /// Detaching a small file into a job copied and repacked leaves on both
    /// sides of its key range and then took a second plan to drain three
    /// records. The removed records and retired extents are what draining
    /// the job would have produced. Returns false, having changed nothing,
    /// for a larger object, which the caller detaches instead.
    pub(crate) fn remove_small_object(&mut self, owner: u64) -> Result<bool> {
        let mut low = owner.to_be_bytes().to_vec();
        low.push(1);
        let mut high = owner.to_be_bytes().to_vec();
        high.push(5);
        let records = self.forest.scan_before(
            &self.catalog,
            TreeKind::Catalog,
            &low,
            Some(&high),
            DIRECT_RECORDS,
        )?;
        if records.len() == DIRECT_RECORDS {
            return Ok(false);
        }
        let mut extents = Vec::new();
        let mut blocks = 0u64;
        for (key, value) in &records {
            if key.len() < 9 || u64_be(&key[..8])? != owner || !(1..=4).contains(&key[8]) {
                return Err(Error::Corrupt("orphan record owner or kind mismatch"));
            }
            if key[8] == 3 {
                let start = value
                    .get(..8)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u64::from_le_bytes)
                    .ok_or(Error::Corrupt("extent physical length"))?;
                let count = value
                    .get(8..12)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(|bytes| u64::from(u32::from_le_bytes(bytes)))
                    .ok_or(Error::Corrupt("extent count length"))?;
                blocks = blocks
                    .checked_add(count)
                    .ok_or(Error::Corrupt("orphan extent total overflow"))?;
                extents.push(Range { start, count });
            }
        }
        if blocks > DIRECT_BLOCKS {
            return Ok(false);
        }
        match self
            .forest
            .remove_run(&self.catalog, TreeKind::Catalog, &low, &high)?
        {
            Some(catalog) => self.catalog = catalog,
            None => {
                for (key, _) in &records {
                    self.catalog =
                        self.forest
                            .remove(self.catalog.clone(), TreeKind::Catalog, key)?;
                }
            }
        }
        self.retired_data.extend(extents);
        Ok(true)
    }

    fn balance_workspace(&mut self) -> Result<bool> {
        if self.workspace.len() > TARGET + BATCH {
            let excluded = self.excluded;
            self.release.extend(
                self.workspace
                    .iter()
                    .rev()
                    .filter(|block| !excluded.contains(block))
                    .take(BATCH),
            );
            return Ok(true);
        }
        if self.workspace.len() >= TARGET - BATCH
            || self.forest.base.free_blocks < self.workspace.len() as u64 + BATCH as u64
        {
            return Ok(false);
        }
        let groups = self.forest.base.blocks.div_ceil(GROUP);
        let mut inserted_bitmaps = 0usize;
        // A cursor publication bounds each search even on a fragmented 32 TiB volume.
        'scan: for _ in 0..groups.min(BATCH as u64) {
            let group = self.cursor;
            self.cursor = (group + 1) % groups;
            let stored =
                self.forest
                    .lookup(&self.allocation, TreeKind::Allocation, &group.to_be_bytes())?;
            let implicit = stored.is_none();
            let bitmap = stored.unwrap_or_else(|| implicit_bitmap(group, self.forest.base.blocks));
            let before = self.refill.len();
            for block in group * GROUP..((group + 1) * GROUP).min(self.forest.base.blocks) {
                if !bit(&bitmap, block) {
                    self.refill.insert(block);
                    if self.refill.len() == BATCH {
                        if implicit {
                            inserted_bitmaps = inserted_bitmaps
                                .checked_add(1)
                                .ok_or(Error::LimitExceeded)?;
                        }
                        break 'scan;
                    }
                }
            }
            if implicit && self.refill.len() != before {
                inserted_bitmaps = inserted_bitmaps
                    .checked_add(1)
                    .ok_or(Error::LimitExceeded)?;
            }
        }
        let insertion_charge = inserted_bitmaps
            .checked_mul(MAX_BITMAP_INSERTION_NET_PAGES)
            .ok_or(Error::LimitExceeded)?;
        let charged_successor = self
            .workspace
            .len()
            .checked_add(self.refill.len())
            .and_then(|slots| slots.checked_sub(insertion_charge));
        if charged_successor.is_none_or(|slots| slots < MAX_PAGES) {
            // Cursor progress is still publishable. Leaving an underfunded
            // partial implicit group raw lets a later window reach refillable
            // space instead of retrying this failing batch forever.
            self.refill.clear();
        }
        Ok(true)
    }

    pub(crate) fn seal(mut self, use_reserve: bool) -> Result<Plan> {
        let (available, excluded) = partition_slots(&self.workspace, &self.release, self.excluded);
        let mut primary = vec![self.catalog.clone()];
        primary.extend(self.jobs.iter().map(|job| job.tree.clone()));
        let primary_ids = self.forest.draft_ids(&primary)?;
        if primary_ids.len() > available.len() {
            return Err(Error::NoSpace);
        }
        let mut assignments: BTreeMap<_, _> = primary_ids
            .iter()
            .copied()
            .zip(available.iter().copied())
            .collect();
        let mut cleanup = self.cleanup.clone();
        for (key, value) in &self.updates {
            cleanup = match value {
                Some(value) => self.forest.put(cleanup, TreeKind::Cleanup, key, value)?,
                None => self.forest.remove(cleanup, TreeKind::Cleanup, key)?,
            };
        }
        for job in &self.jobs {
            let reference = self.forest.resolve(&job.tree, &assignments)?;
            let mut value = vec![0; 32];
            value[..8].copy_from_slice(&job.owner.to_le_bytes());
            value[8] = job.low;
            value[9] = job.high;
            value[16..24].copy_from_slice(&reference.block.to_le_bytes());
            value[24..32].copy_from_slice(&reference.generation.to_le_bytes());
            cleanup = self
                .forest
                .put(cleanup, TreeKind::Cleanup, &job.key, &value)?;
        }
        for range in &self.retired_data {
            let mut key = vec![1];
            key.extend_from_slice(&self.forest.generation.to_be_bytes());
            key.extend_from_slice(&range.start.to_be_bytes());
            let mut value = vec![0; 8];
            value[..4].copy_from_slice(
                &u32::try_from(range.count)
                    .map_err(|_| Error::LimitExceeded)?
                    .to_le_bytes(),
            );
            cleanup = self.forest.put(cleanup, TreeKind::Cleanup, &key, &value)?;
        }
        let mut freed = BTreeSet::new();
        for range in &self.freed {
            if range.count > 512 {
                return Err(Error::LimitExceeded);
            }
            for block in range.start
                ..range
                    .start
                    .checked_add(range.count)
                    .ok_or(Error::Corrupt("freed range overflow"))?
            {
                if !freed.insert(block) {
                    return Err(Error::Corrupt("overlapping freed ranges"));
                }
            }
        }
        let groups: BTreeSet<_> = freed
            .iter()
            .chain(&self.release)
            .chain(&self.refill)
            .chain(&self.allocated)
            .map(|block| block / GROUP)
            .collect();
        let mut allocation = self.allocation.clone();
        for group in groups {
            let mut bitmap = self.bitmap(group)?;
            let range = group * GROUP..(group + 1) * GROUP;
            for block in freed
                .range(range.clone())
                .chain(self.release.range(range.clone()))
            {
                if !bit(&bitmap, *block) {
                    return Err(Error::Corrupt("released block is already free"));
                }
                set(&mut bitmap, *block, false);
            }
            for block in self.refill.range(range.clone()) {
                if bit(&bitmap, *block) {
                    return Err(Error::InvalidInput("workspace refill was not free"));
                }
                set(&mut bitmap, *block, true);
            }
            for block in self.allocated.range(range) {
                if bit(&bitmap, *block) {
                    return Err(Error::InvalidInput("allocated data block was not free"));
                }
                set(&mut bitmap, *block, true);
            }
            // Retaining an implicit bitmap preserves topology during pool balance.
            allocation = self.forest.put(
                allocation,
                TreeKind::Allocation,
                &group.to_be_bytes(),
                &bitmap,
            )?;
        }
        let mut roots = primary;
        roots.push(allocation.clone());
        roots.push(cleanup.clone());
        let ids = self.forest.draft_ids(&roots)?;
        if ids.len() > MAX_PAGES || self.forest.retired.len() > MAX_PAGES {
            return Err(Error::LimitExceeded);
        }
        if ids.len() > available.len() {
            return Err(Error::NoSpace);
        }
        let remaining: Vec<_> = ids
            .iter()
            .filter(|id| !assignments.contains_key(id))
            .copied()
            .collect();
        assignments.extend(
            remaining
                .into_iter()
                .zip(available.iter().copied().skip(primary_ids.len())),
        );
        // Excluded slots stay owned by the workspace; they are only unusable.
        // Every list here is ascending, so merging beats a set of thousands of
        // blocks rebuilt for each plan.
        let retired: Vec<_> = self.forest.retired.iter().copied().collect();
        let returning: Vec<_> = self.refill.iter().copied().collect();
        for block in retired.iter().chain(&returning) {
            if available.binary_search(block).is_ok() {
                return Err(Error::InvalidInput("workspace ownership overlaps"));
            }
        }
        let slots = merge_slots(&available[ids.len()..], &excluded)?;
        let slots = merge_slots(&slots, &retired)?;
        let slots = merge_slots(&slots, &returning)?;
        if slots.len() > workspace::CAPACITY {
            return Err(Error::LimitExceeded);
        }
        // The engineering admission gate: every successor can fund the next
        // plan of any admitted size.
        if slots.len() < MAX_PAGES {
            return Err(Error::NoSpace);
        }
        let free_blocks = self
            .forest
            .base
            .free_blocks
            .checked_add(freed.len() as u64)
            .and_then(|free| free.checked_add(self.forest.retired.len() as u64))
            .and_then(|free| free.checked_sub(ids.len() as u64))
            .and_then(|free| free.checked_sub(self.allocated.len() as u64))
            .ok_or(Error::NoSpace)?;
        if !use_reserve && free_blocks < RESERVE {
            return Err(Error::NoSpace);
        }
        // free_blocks counts workspace slots, which cannot hold file data. Data
        // allocation must leave the reserve as raw blocks so workspace can refill.
        if !use_reserve && !self.allocated.is_empty() {
            let raw_after = self
                .forest
                .base
                .free_blocks
                .checked_sub(self.workspace.len() as u64)
                .ok_or(Error::Corrupt("workspace exceeds free count"))?
                .checked_add((freed.len() + self.release.len()) as u64)
                .and_then(|raw| raw.checked_sub((self.refill.len() + self.allocated.len()) as u64));
            if raw_after.is_none_or(|raw| raw < RESERVE) {
                return Err(Error::NoSpace);
            }
        }
        let mut root = self.forest.base.clone();
        root.generation = self.forest.generation;
        root.catalog = self.forest.resolve(&self.catalog, &assignments)?;
        root.allocation = self.forest.resolve(&allocation, &assignments)?;
        root.cleanup = self.forest.resolve(&cleanup, &assignments)?;
        root.free_blocks = free_blocks;
        root.next_job = self.next_job;
        root.next_object = self.next_object;
        root.workspace_cursor = self.cursor;
        let (mut writes, built) = self.forest.encode(&assignments)?;
        writes.extend(self.forest.take_data());
        let mut published = None;
        if slots != self.workspace {
            root.workspace = self.bank;
            if !self.chained {
                writes.extend(workspace::encode(&root, &slots)?);
            }
            published = Some(slots);
        }
        Ok(Plan {
            base: self.forest.base.clone(),
            root,
            writes,
            returned: retired,
            slots: published,
            built,
            data_pages: self.forest.sent_pages(),
        })
    }
}

pub(crate) fn prepare_maintenance(
    device: &mut impl BlockDevice,
    root: &Root,
    pinned: &BTreeSet<u64>,
    orphan_cursor: u64,
    context: Option<Context<'_>>,
) -> Result<(Option<Plan>, u64)> {
    if root.generation == u64::MAX {
        return Ok((None, orphan_cursor));
    }
    let mut transaction = Transaction::cached(device, root, context)?;
    if transaction.balance_workspace()? {
        return transaction
            .seal(true)
            .map(|plan| (Some(plan), orphan_cursor));
    }
    let retired = transaction
        .forest
        .scan(&transaction.cleanup, TreeKind::Cleanup, &[1], 32)?;
    transaction.retirement(&retired)?;
    // A plan drains detached jobs first, then marked orphans, until its draft
    // budget, so deleting many files does not cost two publications per file.
    let jobs = transaction.forest.scan(
        &transaction.cleanup,
        TreeKind::Cleanup,
        &[2],
        MAINTENANCE_BATCH,
    )?;
    for (key, value) in jobs {
        if key.first() != Some(&2) || transaction.forest.drafts() >= MAINTENANCE_DRAFTS {
            break;
        }
        transaction.detached(key, &value)?;
    }
    let mut next_cursor = orphan_cursor;
    if transaction.forest.drafts() < MAINTENANCE_DRAFTS {
        // Windows without a candidate cost reads only, so keep scanning until
        // one has work or the scan has covered every marker once. Then a step
        // without a plan means there is no unpinned cleanup, which callers
        // take as quiet. Pins are handles, so a lap is at most 4,096 / 32
        // full windows plus a short one each side of the wrap.
        let mut wrapped = false;
        let candidates = loop {
            let mut start = vec![3];
            start.extend_from_slice(&next_cursor.to_be_bytes());
            let orphans = transaction.forest.scan(
                &transaction.cleanup,
                TreeKind::Cleanup,
                &start,
                ORPHAN_WINDOW,
            )?;
            let (candidates, cursor) = unpinned_orphans(orphans, pinned, MAINTENANCE_BATCH)?;
            next_cursor = cursor;
            if !candidates.is_empty() {
                break candidates;
            }
            // A full window moves the cursor forward; a short one wraps it.
            if cursor == 0 {
                if wrapped || orphan_cursor == 0 {
                    break candidates;
                }
                wrapped = true;
            } else if wrapped && cursor >= orphan_cursor {
                break candidates;
            }
        };
        for key in candidates {
            if transaction.forest.drafts() >= MAINTENANCE_DRAFTS {
                break;
            }
            transaction.orphan(key)?;
        }
    }
    if transaction.freed.is_empty() && transaction.updates.is_empty() && transaction.jobs.is_empty()
    {
        return Ok((None, next_cursor));
    }
    transaction.seal(true).map(|plan| (Some(plan), next_cursor))
}

const ORPHAN_WINDOW: usize = 32;
// The largest orphan removed directly, in records and data blocks: what one
// `detached` drain step takes (32 records, 512 blocks).
const DIRECT_RECORDS: usize = 32;
const DIRECT_BLOCKS: u64 = 512;
// Cleanup items one plan may take, and the share of the draft budget it fills.
const MAINTENANCE_BATCH: usize = 16;
const MAINTENANCE_DRAFTS: usize = crate::tree::MAX_DRAFTS / 4;

// The cursor names the next object to examine. Exhausting a short window wraps
// to the start; a full pinned window still advances without a disk publication.
// The cursor follows the last unpinned marker taken, so a plan that takes
// several still resumes after them.
fn unpinned_orphans(
    records: Vec<Record>,
    pinned: &BTreeSet<u64>,
    limit: usize,
) -> Result<(Vec<Vec<u8>>, u64)> {
    let full = records.len() == ORPHAN_WINDOW;
    let mut taken = Vec::new();
    let mut next = 0;
    for (key, _) in records {
        if key.first() != Some(&3) {
            return Err(Error::Corrupt("invalid orphan marker kind"));
        }
        let object = u64_be(&key[1..])?;
        next = object
            .checked_add(1)
            .ok_or(Error::Corrupt("orphan cursor overflow"))?;
        if !pinned.contains(&object) {
            taken.push(key);
            if taken.len() == limit {
                return Ok((taken, next));
            }
        }
    }
    if taken.is_empty() && !full {
        next = 0;
    }
    Ok((taken, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{BLOCK_BYTES, TreeRef, encode_leaf, encode_node};
    use crate::volume::model::UUID;
    use crate::volume::model::{
        Event, FaultDevice, GEOMETRY, advance, formatted_volume, orphan_image,
    };
    use crate::{MountMode, Volume};

    #[test]
    fn slot_partition_matches_set_filters() {
        let check = |slots: &[u64], release: &BTreeSet<u64>, excluded: &BTreeSet<u64>| {
            let expect = |keep: bool| -> Vec<u64> {
                slots
                    .iter()
                    .copied()
                    .filter(|b| !release.contains(b) && excluded.contains(b) == keep)
                    .collect()
            };
            assert_eq!(
                partition_slots(slots, release, excluded),
                (expect(false), expect(true))
            );
        };
        let slots: Vec<u64> = (1..201).map(|index| index * 3).collect();
        // Sparse and dense changes, shared entries, and blocks outside the list.
        for step in [1, 7, 32, 200, 601] {
            let release = (0..602).step_by(step).collect();
            let excluded = (1..602).step_by(step).collect();
            check(&slots, &release, &excluded);
            check(&slots, &release, &release);
        }
        check(&slots, &BTreeSet::new(), &BTreeSet::new());
        check(&[], &BTreeSet::from([0, u64::MAX]), &BTreeSet::new());
        check(
            &[0, u64::MAX],
            &BTreeSet::from([u64::MAX]),
            &BTreeSet::from([0, u64::MAX]),
        );
    }

    #[test]
    fn orphan_window_selection_advances_and_wraps() {
        let records = |range: std::ops::Range<u64>| -> Vec<Record> {
            range
                .map(|object| {
                    let mut key = vec![3];
                    key.extend_from_slice(&object.to_be_bytes());
                    (key, Vec::new())
                })
                .collect()
        };
        let pinned: BTreeSet<_> = (2..67).collect();
        let objects = |keys: &[Vec<u8>]| -> Vec<u64> {
            keys.iter().map(|key| u64_be(&key[1..]).unwrap()).collect()
        };
        // A full window of pinned markers advances the cursor without work.
        assert_eq!(
            unpinned_orphans(records(2..34), &pinned, 16).unwrap(),
            (Vec::new(), 34)
        );
        assert_eq!(
            unpinned_orphans(records(34..66), &pinned, 16).unwrap(),
            (Vec::new(), 66)
        );
        // A short window without a candidate wraps to the start.
        assert_eq!(
            unpinned_orphans(records(66..67), &pinned, 16).unwrap(),
            (Vec::new(), 0)
        );
        let (keys, next) = unpinned_orphans(records(66..68), &pinned, 16).unwrap();
        assert_eq!((objects(&keys), next), (vec![67], 68));
        // Several unpinned markers in one window, and the cursor follows the last.
        let (keys, next) = unpinned_orphans(records(60..80), &pinned, 16).unwrap();
        assert_eq!((objects(&keys), next), ((67..80).collect::<Vec<_>>(), 80));
        // The limit stops the batch and leaves the rest for the next plan.
        let (keys, next) = unpinned_orphans(records(60..90), &pinned, 4).unwrap();
        assert_eq!((objects(&keys), next), (vec![67, 68, 69, 70], 71));
        assert_eq!(
            unpinned_orphans(Vec::new(), &pinned, 16).unwrap(),
            (Vec::new(), 0)
        );
        assert!(matches!(
            unpinned_orphans(vec![(vec![3], vec![])], &pinned, 16),
            Err(Error::Corrupt(_))
        ));
    }

    fn selected(device: &FaultDevice) -> Root {
        Root::decode(
            &device.media.borrow().durable[&256].0[..],
            256,
            device.geometry,
        )
        .unwrap()
    }

    fn verify_plan(device: &mut FaultDevice, plan: &Plan) {
        let events = device.media.borrow().events.clone();
        let old_slots: BTreeSet<_> = workspace::read(device, &plan.base)
            .unwrap()
            .into_iter()
            .collect();
        let mut old = Forest::new(device, &plan.base).unwrap();
        let allocation = old
            .root(plan.base.allocation, TreeKind::Allocation)
            .unwrap();
        let cleanup = old.root(plan.base.cleanup, TreeKind::Cleanup).unwrap();
        let mut retired = BTreeSet::new();
        let mut cursor = vec![1];
        loop {
            let records = old.scan(&cleanup, TreeKind::Cleanup, &cursor, 64).unwrap();
            if records.is_empty() {
                break;
            }
            for (key, value) in &records {
                if key[0] != 1 {
                    break;
                }
                let start = u64_be(&key[9..]).unwrap();
                let count = u32::from_le_bytes(value[..4].try_into().unwrap());
                retired.extend(start..start + u64::from(count));
            }
            cursor = records.last().unwrap().0.clone();
            if cursor[0] != 1 {
                break;
            }
            cursor.push(0);
        }
        let mut blocks = BTreeSet::new();
        for (block, _) in &plan.writes {
            assert!(blocks.insert(*block), "duplicate replacement {block}");
            if workspace::is_bank_block(*block, plan.base.blocks) {
                assert_ne!(plan.base.workspace.block, plan.root.workspace.block);
            } else {
                assert!(
                    old_slots.contains(block),
                    "replacement outside base workspace {block}"
                );
            }
        }
        let mut overlay = old.device.media.borrow().durable.clone();
        overlay.extend(plan.writes.iter().cloned());
        let mut overlay =
            FaultDevice::with_geometry(overlay, MountMode::ReadOnly, old.device.geometry);
        let checked = crate::check::validate(&mut overlay, &plan.root).unwrap();
        assert_eq!(checked.free_blocks, plan.root.free_blocks);
        let new_slots: BTreeSet<_> = workspace::read(&mut overlay, &plan.root)
            .unwrap()
            .into_iter()
            .collect();
        let mut new = Forest::new(&mut overlay, &plan.root).unwrap();
        let new_allocation = new
            .root(plan.root.allocation, TreeKind::Allocation)
            .unwrap();
        let mut cleared = 0;
        let mut allocated = 0;
        for group in 0..plan.base.blocks.div_ceil(GROUP) {
            let key = group.to_be_bytes();
            let before = old
                .lookup(&allocation, TreeKind::Allocation, &key)
                .unwrap()
                .unwrap_or_else(|| implicit_bitmap(group, plan.base.blocks));
            let after = new
                .lookup(&new_allocation, TreeKind::Allocation, &key)
                .unwrap()
                .unwrap_or_else(|| implicit_bitmap(group, plan.base.blocks));
            for block in group * GROUP..((group + 1) * GROUP).min(plan.base.blocks) {
                match (bit(&before, block), bit(&after, block)) {
                    (true, false) => {
                        assert!(
                            retired.contains(&block) || old_slots.contains(&block),
                            "freed live block {block}"
                        );
                        cleared += 1;
                    }
                    (false, true) => {
                        assert!(
                            new_slots.contains(&block),
                            "new allocation outside workspace {block}"
                        );
                        allocated += 1;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(
            plan.root.free_blocks,
            plan.base.free_blocks + cleared + new_slots.len() as u64
                - allocated
                - old_slots.len() as u64
        );
        assert_eq!(
            device.media.borrow().events,
            events,
            "planning issued output"
        );
    }

    #[test]
    fn orphan_detachment_and_partial_jobs_preserve_accounting() {
        for data_blocks in [0, 16, 1024] {
            // A small orphan is removed in the first plan; a large one is
            // detached into a job and drained 512 blocks at a time.
            let direct = u64::from(data_blocks) <= DIRECT_BLOCKS;
            let mut device = FaultDevice::new(orphan_image(data_blocks), MountMode::ReadWrite);
            for step in 0..8 {
                let before = selected(&device);
                let Some(plan) =
                    prepare_maintenance(&mut device, &before, &BTreeSet::new(), 0, None)
                        .map(|(plan, _)| plan)
                        .unwrap()
                else {
                    assert_eq!(
                        crate::check::validate(&mut device, &before)
                            .unwrap()
                            .data_blocks,
                        0
                    );
                    break;
                };
                verify_plan(&mut device, &plan);
                let expected = plan.root.clone();
                let writes: Vec<_> = plan
                    .writes
                    .iter()
                    .map(|(block, page)| (*block, page.0.clone()))
                    .collect();
                let repeated = prepare_maintenance(&mut device, &before, &BTreeSet::new(), 0, None)
                    .map(|(plan, _)| plan)
                    .unwrap()
                    .unwrap();
                assert_eq!(repeated.root, expected);
                assert_eq!(
                    repeated
                        .writes
                        .iter()
                        .map(|(block, page)| (*block, page.0.clone()))
                        .collect::<Vec<_>>(),
                    writes
                );
                let mut volume = Volume::open(device).unwrap();
                volume.sync().unwrap();
                let mut next = FaultDevice::new(volume.into_model_image(), MountMode::ReadWrite);
                assert_eq!(selected(&next), expected);
                let summary = crate::check::validate(&mut next, &expected).unwrap();
                let remaining = if direct {
                    0
                } else {
                    u64::from(data_blocks).saturating_sub(step * 512)
                };
                assert_eq!(summary.objects, 1, "only the root remains reachable");
                assert_eq!(summary.data_blocks, remaining);
                let mut forest = Forest::new(&mut next, &expected).unwrap();
                let cleanup = forest.root(expected.cleanup, TreeKind::Cleanup).unwrap();
                let jobs = forest.scan(&cleanup, TreeKind::Cleanup, &[2], 64).unwrap();
                assert!(
                    jobs.iter().all(|(key, _)| key[0] == 2),
                    "orphan marker survived detach"
                );
                assert_eq!(
                    jobs.len(),
                    usize::from(!direct && (step == 0 || remaining != 0))
                );
                // Detachment and its first job step cannot release any data,
                // and direct removal only retires it.
                // Data retirement has a separate 512-block reclamation budget.
                let allocation = forest
                    .root(expected.allocation, TreeKind::Allocation)
                    .unwrap();
                let bitmap = forest
                    .lookup(&allocation, TreeKind::Allocation, &0u64.to_be_bytes())
                    .unwrap()
                    .unwrap();
                for offset in 0..u64::from(data_blocks) {
                    if step == 0 || (!direct && step == 1) {
                        assert!(bit(&bitmap, 4096 + offset));
                    }
                }
                device = next;
            }
        }
    }

    #[test]
    fn sealed_plan_rejects_another_base_before_output() {
        let mut device = FaultDevice::new(orphan_image(16), MountMode::ReadWrite);
        let base = selected(&device);
        let plan = prepare_maintenance(&mut device, &base, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        let mut different = base.clone();
        different.next_job += 1;
        assert!(matches!(
            plan.into_parts(&different),
            Err(Error::InvalidInput(_))
        ));
        assert!(device.media.borrow().events.is_empty());
        let plan = prepare_maintenance(&mut device, &base, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        let (root, writes, ..) = plan.into_parts(&base).unwrap();
        assert_eq!(root.generation, base.generation + 1);
        assert!(!writes.is_empty());
    }

    #[test]
    fn allocated_data_is_charged_published_retired_and_later_freed() {
        let mut device =
            FaultDevice::new(formatted_volume().into_model_image(), MountMode::ReadWrite);
        let base = selected(&device);
        let mut transaction = Transaction::new(&mut device, &base).unwrap();
        let runs = transaction.allocate_data(20, 0).unwrap();
        assert_eq!(runs.iter().map(|(_, count)| count).sum::<u64>(), 20);
        assert!(
            transaction
                .write_data(0, Page(Box::new([0; BLOCK_BYTES])))
                .is_err()
        );
        for &(start, count) in &runs {
            for block in start..start + count {
                transaction
                    .write_data(block, Page(Box::new([0xA5; BLOCK_BYTES])))
                    .unwrap();
            }
            transaction.retire_data(start, count);
        }
        assert!(
            transaction
                .write_data(runs[0].0, Page(Box::new([0; BLOCK_BYTES])))
                .is_err()
        );
        let plan = transaction.seal(false).unwrap();
        let data_blocks: BTreeSet<_> = runs
            .iter()
            .flat_map(|&(start, count)| start..start + count)
            .collect();
        assert_eq!(
            plan.writes
                .iter()
                .filter(
                    |(block, page)| data_blocks.contains(block) && *page.0 == [0xA5; BLOCK_BYTES]
                )
                .count(),
            20
        );
        assert!(device.media.borrow().events.is_empty());
        let mut root = plan.root.clone();
        device.media.borrow_mut().durable.extend(plan.writes);
        crate::check::validate(&mut device, &root).unwrap();

        for _ in 0..8 {
            let Some(plan) = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
            else {
                break;
            };
            root = plan.root.clone();
            device.media.borrow_mut().durable.extend(plan.writes);
            crate::check::validate(&mut device, &root).unwrap();
        }
        let mut transaction = Transaction::new(&mut device, &root).unwrap();
        assert!(
            transaction
                .forest
                .scan(&transaction.cleanup, TreeKind::Cleanup, &[1], 1)
                .unwrap()
                .first()
                .is_none_or(|(key, _)| key[0] != 1),
            "retired data remains after maintenance"
        );
        assert_eq!(transaction.allocate_data(20, 0).unwrap(), runs);
    }

    #[test]
    fn allocation_wraps_sparse_free_bits_and_rewinds_without_duplicates() {
        let mut device =
            FaultDevice::new(formatted_volume().into_model_image(), MountMode::ReadWrite);
        let base = selected(&device);
        let mut transaction = Transaction::new(&mut device, &base).unwrap();
        let mut bitmap = vec![0xff; BITMAP];
        for block in [513, GROUP - 1] {
            set(&mut bitmap, block, false);
        }
        transaction.allocation = transaction
            .forest
            .put(
                transaction.allocation.clone(),
                TreeKind::Allocation,
                &0u64.to_be_bytes(),
                &bitmap,
            )
            .unwrap();
        transaction.allocated.insert(GROUP - 2);

        let mark = transaction.mark();
        assert_eq!(
            transaction.allocate_data(1, 0).unwrap(),
            vec![(GROUP - 1, 1)]
        );
        assert_eq!(transaction.allocate_data(1, 0).unwrap(), vec![(513, 1)]);
        transaction.rewind(mark);
        assert_eq!(
            transaction.allocate_data(2, 0).unwrap(),
            vec![(513, 1), (GROUP - 1, 1)]
        );
        assert_eq!(transaction.allocated.len(), 3);
    }

    #[test]
    #[ignore = "diagnostic allocation scan timing"]
    fn allocation_scan_cost_at_group_occupancy() {
        for percent in [0, 50, 90, 98] {
            for repetition in 0..3 {
                let mut device =
                    FaultDevice::new(formatted_volume().into_model_image(), MountMode::ReadWrite);
                let base = selected(&device);
                let mut transaction = Transaction::new(&mut device, &base).unwrap();
                let mut bitmap = transaction.bitmap(0).unwrap();
                for block in 512..GROUP * percent / 100 {
                    set(&mut bitmap, block, true);
                }
                transaction.allocation = transaction
                    .forest
                    .put(
                        transaction.allocation.clone(),
                        TreeKind::Allocation,
                        &0u64.to_be_bytes(),
                        &bitmap,
                    )
                    .unwrap();
                let start = std::time::Instant::now();
                for _ in 0..256 {
                    assert_eq!(transaction.allocate_data(1, 0).unwrap()[0].1, 1);
                }
                println!(
                    "allocation_scan\t{percent}\t{repetition}\t{:.3}",
                    start.elapsed().as_secs_f64() * 1_000.0 / 256.0
                );
            }
        }
    }

    #[test]
    fn data_allocation_cannot_consume_the_foreground_reserve() {
        let mut device =
            FaultDevice::new(formatted_volume().into_model_image(), MountMode::ReadWrite);
        let base = selected(&device);
        let raw_free = base.free_blocks - workspace::read(&mut device, &base).unwrap().len() as u64;
        let mut transaction = Transaction::new(&mut device, &base).unwrap();
        assert!(matches!(
            transaction.allocate_data(raw_free + 1, 0),
            Err(Error::NoSpace)
        ));

        let mut transaction = Transaction::new(&mut device, &base).unwrap();
        let runs = transaction
            .allocate_data(raw_free - RESERVE + 1, 0)
            .unwrap();
        for &(start, count) in &runs {
            transaction.retire_data(start, count);
        }
        assert!(matches!(transaction.seal(false), Err(Error::NoSpace)));
        assert!(device.media.borrow().events.is_empty());
        assert_eq!(selected(&device), base);

        // Leaving exactly the reserve as raw blocks is admitted and checks.
        let mut transaction = Transaction::new(&mut device, &base).unwrap();
        let runs = transaction.allocate_data(raw_free - RESERVE, 0).unwrap();
        for &(start, count) in &runs {
            transaction.retire_data(start, count);
        }
        let plan = transaction.seal(false).unwrap();
        device.media.borrow_mut().durable.extend(plan.writes);
        let summary = crate::check::validate(&mut device, &plan.root).unwrap();
        let slots = workspace::read(&mut device, &plan.root).unwrap().len() as u64;
        assert_eq!(summary.free_blocks - slots, RESERVE);
    }

    #[test]
    fn workspace_replacement_finishes_without_creating_metadata_retirement() {
        let mut volume = formatted_volume();
        advance(&mut volume).unwrap();
        let mut device = FaultDevice::new(volume.into_model_image(), MountMode::ReadWrite);
        let root = selected(&device);
        let plan = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        verify_plan(&mut device, &plan);
        assert_eq!(plan.root.free_blocks, root.free_blocks + 3);
        let next = plan.root.clone();
        device.media.borrow_mut().durable.extend(plan.writes);
        assert!(
            prepare_maintenance(&mut device, &next, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
                .is_none()
        );
    }

    fn build(
        kind: TreeKind,
        records: &[Record],
        next: &mut u64,
        pages: &mut BTreeMap<u64, Page>,
        geometry: crate::Geometry,
        generation: u64,
    ) -> (TreeRef, u64) {
        let mut edges: Vec<(Vec<u8>, TreeRef, u64)> = Vec::new();
        let mut at = 0;
        while at < records.len() {
            let start = at;
            let mut bytes = 0;
            while at < records.len()
                && bytes + 4 + records[at].0.len() + records[at].1.len() <= 4016
            {
                bytes += 4 + records[at].0.len() + records[at].1.len();
                at += 1;
            }
            assert!(at > start);
            let reference = TreeRef {
                block: *next,
                generation,
            };
            *next += 1;
            let borrowed: Vec<_> = records[start..at]
                .iter()
                .map(|(k, v)| (k.as_slice(), v.as_slice()))
                .collect();
            let page = encode_leaf(reference, &UUID, kind, &borrowed, geometry.blocks()).unwrap();
            let total = u64_le(&page.0[56..64]).unwrap();
            pages.insert(reference.block, page);
            edges.push((records[start].0.clone(), reference, total));
        }
        let mut level = 0;
        while edges.len() > 1 {
            level += 1;
            let mut parents = Vec::new();
            let mut at = 0;
            while at < edges.len() {
                let start = at;
                let mut bytes = 0;
                while at < edges.len() && bytes + 20 + edges[at].0.len() <= 4016 {
                    bytes += 20 + edges[at].0.len();
                    at += 1;
                }
                if edges.len() - at == 1 {
                    at -= 1;
                }
                let reference = TreeRef {
                    block: *next,
                    generation,
                };
                *next += 1;
                let owned: Vec<_> = edges[start..at]
                    .iter()
                    .map(|(key, child, _)| {
                        let mut value = child.block.to_le_bytes().to_vec();
                        value.extend_from_slice(&child.generation.to_le_bytes());
                        (key.clone(), value)
                    })
                    .collect();
                let borrowed: Vec<_> = owned
                    .iter()
                    .map(|(k, v)| (k.as_slice(), v.as_slice()))
                    .collect();
                let total = edges[start..at].iter().map(|(_, _, total)| *total).sum();
                let page = encode_node(
                    reference,
                    &UUID,
                    kind,
                    &borrowed,
                    geometry.blocks(),
                    level,
                    total,
                )
                .unwrap();
                pages.insert(reference.block, page);
                parents.push((edges[start].0.clone(), reference, total));
            }
            edges = parents;
        }
        let (_, reference, total) = edges.pop().unwrap();
        (reference, total)
    }

    fn full_detached_image(fragmented: bool) -> BTreeMap<u64, Page> {
        let initial = formatted_volume().into_model_image();
        let initial_root = Root::decode(&initial[&256].0[..], 256, GEOMETRY).unwrap();
        let mut data_count = GEOMETRY.blocks() - 1024 - RESERVE - 8;
        loop {
            let mut pages = BTreeMap::from([(512, initial[&512].clone())]);
            let mut next = 514;
            let records: Vec<_> = (0..if fragmented { data_count } else { 1 })
                .map(|logical| {
                    let mut key = 2u64.to_be_bytes().to_vec();
                    key.push(3);
                    key.extend_from_slice(&logical.to_be_bytes());
                    let mut value = 8192u64.to_le_bytes().to_vec();
                    value.extend_from_slice(
                        &(if fragmented { 1 } else { data_count as u32 }).to_le_bytes(),
                    );
                    value.extend_from_slice(&[0; 4]);
                    (key, value)
                })
                .collect();
            let (detached, _) = build(
                TreeKind::Catalog,
                &records,
                &mut next,
                &mut pages,
                GEOMETRY,
                1,
            );
            let data_start = next + 5;
            let actual = GEOMETRY.blocks() - 512 - data_start - RESERVE;
            if actual != data_count {
                data_count = actual;
                continue;
            }
            // Rotate physical placement by half the file on each record, spreading
            // consecutive logical extents across the image's allocation groups.
            let records: Vec<_> = records
                .into_iter()
                .enumerate()
                .map(|(i, (key, mut value))| {
                    let index = if fragmented {
                        (i as u64 / 2) + (i as u64 % 2) * data_count.div_ceil(2)
                    } else {
                        0
                    };
                    value[..8].copy_from_slice(&(data_start + index).to_le_bytes());
                    (key, value)
                })
                .collect();
            let mut rewrite = 514;
            let (rewritten, _) = build(
                TreeKind::Catalog,
                &records,
                &mut rewrite,
                &mut pages,
                GEOMETRY,
                1,
            );
            assert_eq!(rewritten, detached);
            let mut allocation_records = Vec::new();
            for group in 0..GEOMETRY.blocks().div_ceil(GROUP) {
                let mut bitmap = implicit_bitmap(group, GEOMETRY.blocks());
                for block in group * GROUP..((group + 1) * GROUP).min(GEOMETRY.blocks()) {
                    if (512..data_start + data_count + 8192).contains(&block) {
                        set(&mut bitmap, block, true);
                    }
                }
                allocation_records.push((group.to_be_bytes().to_vec(), bitmap));
            }
            let (allocation, _) = build(
                TreeKind::Allocation,
                &allocation_records,
                &mut next,
                &mut pages,
                GEOMETRY,
                1,
            );
            assert_eq!(next, data_start);
            let mut job_key = vec![2];
            job_key.extend_from_slice(&1u64.to_be_bytes());
            let mut job = vec![0; 32];
            job[..8].copy_from_slice(&2u64.to_le_bytes());
            job[8] = 3;
            job[9] = 3;
            job[16..24].copy_from_slice(&detached.block.to_le_bytes());
            job[24..].copy_from_slice(&1u64.to_le_bytes());
            let cleanup = TreeRef {
                block: 513,
                generation: 1,
            };
            pages.insert(
                513,
                encode_leaf(
                    cleanup,
                    &UUID,
                    TreeKind::Cleanup,
                    &[(&job_key, &job)],
                    GEOMETRY.blocks(),
                )
                .unwrap(),
            );
            let root = Root {
                allocation,
                cleanup,
                next_object: 3,
                next_job: 2,
                free_blocks: RESERVE,
                ..initial_root
            };
            pages.insert(256, root.encode(256, GEOMETRY).unwrap());
            pages.insert(
                GEOMETRY.blocks() - 257,
                root.encode(GEOMETRY.blocks() - 257, GEOMETRY).unwrap(),
            );
            pages.extend(
                workspace::encode(
                    &root,
                    &(data_start + data_count..data_start + data_count + 8192).collect::<Vec<_>>(),
                )
                .unwrap(),
            );
            return pages;
        }
    }

    #[test]
    fn refill_cursor_advances_across_full_groups_before_finding_space() {
        let (geometry, image) = scattered_retirement_image(256, 1, 7936);
        let mut device = FaultDevice::with_geometry(image, MountMode::ReadWrite, geometry);
        let mut root = selected(&device);
        for cursor in [128, 256] {
            let plan = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
                .unwrap();
            assert!(plan.writes.is_empty());
            assert_eq!(plan.root.workspace, root.workspace);
            assert_eq!(plan.root.workspace_cursor, cursor);
            verify_plan(&mut device, &plan);
            root = plan.root;
        }
        let plan = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        verify_plan(&mut device, &plan);
        assert_eq!(plan.root.free_blocks, root.free_blocks);
        device.media.borrow_mut().durable.extend(plan.writes);
        assert_eq!(
            workspace::read(&mut device, &plan.root).unwrap().len(),
            8064
        );
    }

    #[test]
    fn refill_accounts_for_new_bitmaps_and_the_partial_final_group() {
        for partial in [false, true] {
            let geometry = crate::Geometry {
                byte_len: if partial {
                    (4 * GROUP + 513) * 4096
                } else {
                    GEOMETRY.byte_len
                },
                ..GEOMETRY
            };
            let mut image = formatted_volume().into_model_image();
            let mut root = Root::decode(&image[&256].0[..], 256, GEOMETRY).unwrap();
            root.blocks = geometry.blocks();
            root.free_blocks = root.blocks - 1027;
            root.workspace_cursor = if partial { 4 } else { 2 };
            let slots: Vec<_> = (8192..8192 + 7936).collect();
            let mut bitmap = implicit_bitmap(0, root.blocks);
            for block in (512..515).chain(slots.iter().copied()) {
                set(&mut bitmap, block, true);
            }
            image.insert(
                513,
                encode_leaf(
                    root.allocation,
                    &root.uuid,
                    TreeKind::Allocation,
                    &[(&0u64.to_be_bytes(), &bitmap)],
                    root.blocks,
                )
                .unwrap(),
            );
            image.extend(workspace::encode(&root, &slots).unwrap());
            let mut device = FaultDevice::with_geometry(image, MountMode::ReadWrite, geometry);
            crate::check::validate(&mut device, &root).unwrap();
            let plan = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
                .unwrap();
            verify_plan(&mut device, &plan);
            device.media.borrow_mut().durable.extend(plan.writes);
            let next = workspace::read(&mut device, &plan.root).unwrap();
            assert_eq!(next.len(), slots.len() + 128 - 2);
            assert_eq!(plan.root.free_blocks, root.free_blocks - 2);
            assert!(next.contains(&(if partial { 4 * GROUP } else { 2 * GROUP })));
        }
    }

    // Each early generation retires one block in every group. The bulk ranges
    // come last, so oldest-first cleanup must pay for many bitmap rewrites first.
    fn scattered_retirement_image(
        groups: u64,
        rounds: u64,
        pool_size: u64,
    ) -> (crate::Geometry, BTreeMap<u64, Page>) {
        let geometry = crate::Geometry {
            byte_len: (groups + 2) * GROUP * 4096,
            ..GEOMETRY
        };
        let generation = rounds + 1;
        let initial = formatted_volume().into_model_image();
        let mut root = Root::decode(&initial[&256].0[..], 256, GEOMETRY).unwrap();
        let inode = crate::format::decode_page(
            &initial[&512].0[..],
            root.catalog,
            &UUID,
            TreeKind::Catalog,
            1,
            None,
            root.blocks,
        )
        .unwrap()
        .records[0]
            .1
            .to_vec();
        root.generation = generation;
        root.blocks = geometry.blocks();
        root.free_blocks = RESERVE;
        root.catalog.generation = generation;
        let free_start = root.blocks - 512 - RESERVE;
        let mut catalog_key = 1u64.to_be_bytes().to_vec();
        catalog_key.push(1);
        let catalog = encode_leaf(
            root.catalog,
            &UUID,
            TreeKind::Catalog,
            &[(&catalog_key, &inode)],
            root.blocks,
        )
        .unwrap();
        let allocation: Vec<_> = (0..groups + 2)
            .map(|group| {
                let mut bitmap = vec![0xff; BITMAP];
                for block in
                    (group * GROUP).max(free_start)..((group + 1) * GROUP).min(root.blocks - 512)
                {
                    set(&mut bitmap, block, block < free_start + pool_size);
                }
                (group.to_be_bytes().to_vec(), bitmap)
            })
            .collect();
        let retirement = |generation: u64, start: u64, count: u64| {
            let mut key = vec![1];
            key.extend_from_slice(&generation.to_be_bytes());
            key.extend_from_slice(&start.to_be_bytes());
            let mut value = u32::try_from(count).unwrap().to_le_bytes().to_vec();
            value.extend_from_slice(&[0; 4]);
            (key, value)
        };
        let mut metadata_end = 1024;
        for _ in 0..16 {
            let mut pages = BTreeMap::from([(512, catalog.clone())]);
            let mut next = 513;
            root.allocation = build(
                TreeKind::Allocation,
                &allocation,
                &mut next,
                &mut pages,
                geometry,
                generation,
            )
            .0;
            let mut retired = Vec::new();
            for round in 0..rounds {
                for group in 0..groups {
                    retired.push(retirement(round + 1, group * GROUP + 8192 + round, 1));
                }
            }
            let mut cursor = metadata_end;
            for group in 0..groups {
                let start = group * GROUP + 8192;
                assert!(cursor < start);
                retired.push(retirement(generation, cursor, start - cursor));
                cursor = start + rounds;
            }
            retired.push(retirement(generation, cursor, free_start - cursor));
            root.cleanup = build(
                TreeKind::Cleanup,
                &retired,
                &mut next,
                &mut pages,
                geometry,
                generation,
            )
            .0;
            if next != metadata_end {
                metadata_end = next;
                continue;
            }
            pages.insert(256, root.encode(256, geometry).unwrap());
            pages.insert(
                root.blocks - 257,
                root.encode(root.blocks - 257, geometry).unwrap(),
            );
            pages.extend(
                workspace::encode(
                    &root,
                    &(free_start..free_start + pool_size).collect::<Vec<_>>(),
                )
                .unwrap(),
            );
            return (geometry, pages);
        }
        panic!("fixture metadata accounting did not stabilize");
    }

    #[test]
    #[ignore = "extended allocation-group progress run"]
    fn scattered_retirement_recovers_after_temporary_reserve_loss() {
        for (groups, rounds) in [(64, 1), (64, 64), (256, 1)] {
            let (geometry, image) = scattered_retirement_image(groups, rounds, 8192);
            let mut volume = Volume::open(FaultDevice::with_geometry(
                image,
                MountMode::ReadWrite,
                geometry,
            ))
            .unwrap();
            let mut minimum = RESERVE;
            for step in 0..groups * rounds / 32 + 16 {
                volume.sync().unwrap();
                let free = volume.info().unwrap().free_blocks;
                minimum = minimum.min(free);
                if step % 32 == 0 {
                    eprintln!(
                        "groups={groups} rounds={rounds} step={step} free={free} minimum={minimum}"
                    );
                }
            }
            // Workspace slots fund metadata, so bitmap rewrites during scattered
            // cleanup never dip below the reserve.
            assert!(minimum >= RESERVE, "minimum free {minimum}");
            assert!(volume.info().unwrap().free_blocks > RESERVE);
            let mut checked = Volume::open(FaultDevice::with_geometry(
                volume.into_model_image(),
                MountMode::ReadOnly,
                geometry,
            ))
            .unwrap();
            checked.check().unwrap();
        }
    }

    #[test]
    #[ignore = "one TiB sparse allocation regression"]
    fn eligible_retirement_funds_metadata_with_isolated_raw_free_blocks() {
        let groups = RESERVE + 2;
        let geometry = crate::Geometry {
            byte_len: groups * GROUP * 4096,
            ..GEOMETRY
        };
        let initial = formatted_volume().into_model_image();
        let mut root = Root::decode(&initial[&256].0[..], 256, GEOMETRY).unwrap();
        root.blocks = geometry.blocks();
        root.free_blocks = RESERVE;
        let allocation: Vec<_> = (0..groups)
            .map(|group| {
                let mut bitmap = vec![0xff; BITMAP];
                if group >= 8194 {
                    set(&mut bitmap, group * GROUP + 8192, false);
                }
                (group.to_be_bytes().to_vec(), bitmap)
            })
            .collect();
        let mut metadata_end = 2 * GROUP;
        let mut image = None;
        for _ in 0..16 {
            let mut pages = BTreeMap::from([(512, initial[&512].clone())]);
            let mut next = 513;
            root.allocation = build(
                TreeKind::Allocation,
                &allocation,
                &mut next,
                &mut pages,
                geometry,
                1,
            )
            .0;
            let mut cursor = metadata_end;
            let mut retired = Vec::new();
            for end in (2..groups)
                .map(|group| group * GROUP + 8192)
                .chain([root.blocks - 512])
            {
                let mut key = vec![1];
                key.extend_from_slice(&1u64.to_be_bytes());
                key.extend_from_slice(&cursor.to_be_bytes());
                let mut value = u32::try_from(end - cursor).unwrap().to_le_bytes().to_vec();
                value.extend_from_slice(&[0; 4]);
                retired.push((key, value));
                cursor = end + 1;
            }
            root.cleanup = build(
                TreeKind::Cleanup,
                &retired,
                &mut next,
                &mut pages,
                geometry,
                1,
            )
            .0;
            assert!(
                next < 2 * GROUP,
                "metadata must fit before the first free group"
            );
            if next != metadata_end {
                metadata_end = next;
                continue;
            }
            pages.insert(256, root.encode(256, geometry).unwrap());
            pages.insert(
                root.blocks - 257,
                root.encode(root.blocks - 257, geometry).unwrap(),
            );
            pages.extend(
                workspace::encode(
                    &root,
                    &(2..8194)
                        .map(|group| group * GROUP + 8192)
                        .collect::<Vec<_>>(),
                )
                .unwrap(),
            );
            image = Some(pages);
            break;
        }
        let mut device = FaultDevice::with_geometry(
            image.expect("fixture stabilized"),
            MountMode::ReadWrite,
            geometry,
        );
        let summary = crate::check::validate(&mut device, &root).unwrap();
        eprintln!(
            "geometry={} metadata_pages={} free={}",
            geometry.byte_len, summary.metadata_pages, summary.free_blocks
        );
        let plan = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        eprintln!(
            "isolated reserve: {} writes, free {} -> {}",
            plan.writes.len(),
            root.free_blocks,
            plan.root.free_blocks
        );
        assert!(plan.root.free_blocks > root.free_blocks);
        assert!(device.media.borrow().events.is_empty());
        device.media.borrow_mut().durable.extend(plan.writes);
        crate::check::validate(&mut device, &plan.root).unwrap();
    }

    #[test]
    #[ignore = "extended scattered-retirement recovery"]
    fn transferred_retirement_reaches_bulk_ranges_without_exhausting_headroom() {
        let (geometry, image) = scattered_retirement_image(64, 128, 8192);
        let mut volume = Volume::open(FaultDevice::with_geometry(
            image,
            MountMode::ReadWrite,
            geometry,
        ))
        .unwrap();
        let mut minimum = RESERVE;
        for step in 0..288 {
            volume
                .sync()
                .unwrap_or_else(|error| panic!("step={step}: {error}"));
            let free = volume.info().unwrap().free_blocks;
            minimum = minimum.min(free);
            if step % 32 == 0 {
                eprintln!("step={step} free={free} minimum={minimum}");
            }
        }
        assert!(minimum >= 2 * MAX_PAGES as u64);
        assert!(volume.info().unwrap().free_blocks > RESERVE);
        Volume::open(FaultDevice::with_geometry(
            volume.into_model_image(),
            MountMode::ReadOnly,
            geometry,
        ))
        .unwrap()
        .check()
        .unwrap();
    }

    #[test]
    fn full_image_deletion_and_fragmented_cleanup_keep_making_progress() {
        for fragmented in [false, true] {
            let image = full_detached_image(fragmented);
            let mut device = FaultDevice::new(image, MountMode::ReadWrite);
            let initial = selected(&device);
            let foreground = Transaction::new(&mut device, &initial).unwrap().seal(false);
            assert_eq!(foreground.unwrap().root.free_blocks, initial.free_blocks);
            let mut volume = Volume::open(device).unwrap();
            let mut previous = initial.free_blocks;
            let mut minimum = previous;
            let limit = if fragmented { 2000 } else { 110 };
            for step in 0..limit {
                volume.sync().unwrap_or_else(|error| {
                    panic!("fragmented={fragmented}, step={step}: {error}")
                });
                let free = volume.info().unwrap().free_blocks;
                minimum = minimum.min(free);
                previous = free;
                if step % 50 == 0 {
                    let image = volume.into_model_image();
                    volume = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
                }
            }
            assert!(minimum >= RESERVE - 128, "minimum free {minimum}");
            assert!(
                previous > GEOMETRY.blocks() - 1024 - 64,
                "remaining allocation {}",
                GEOMETRY.blocks() - 1024 - previous
            );
        }
    }

    // Publishes a plan onto the durable image as a completed mirror would.
    fn apply(device: &mut FaultDevice, plan: Plan) -> Root {
        let mut media = device.media.borrow_mut();
        media.durable.extend(plan.writes);
        for block in [256, plan.root.blocks - 257] {
            media
                .durable
                .insert(block, plan.root.encode(block, device.geometry).unwrap());
        }
        plan.root
    }

    // Releases workspace in reviewed 128-slot batches down to `slots`.
    fn shrink_workspace(device: &mut FaultDevice, mut root: Root, slots: usize) -> Root {
        loop {
            let current = workspace::read(device, &root).unwrap();
            if current.len() == slots {
                return root;
            }
            let mut transaction = Transaction::new(device, &root).unwrap();
            transaction
                .release
                .extend(current.iter().rev().take((current.len() - slots).min(128)));
            let plan = transaction.seal(true).unwrap();
            root = apply(device, plan);
        }
    }

    // Every job owns a leaf of contiguous 16-block extents. With 89 jobs the
    // cleanup leaf holds 4,005 of 4,016 bytes, so the first partial job step
    // must split it to insert retirement.
    fn job_backlog_image(jobs: u64, extents: u64) -> BTreeMap<u64, Page> {
        let mut pages = formatted_volume().into_model_image();
        let initial = Root::decode(&pages[&256].0[..], 256, GEOMETRY).unwrap();
        let blocks = GEOMETRY.blocks();
        let data = |job: u64, extent: u64| 16_384 + (job * extents + extent) * 16;
        let mut next = 513;
        let mut job_records = Vec::new();
        for job in 0..jobs {
            let owner = job + 2;
            let records: Vec<_> = (0..extents)
                .map(|extent| {
                    let mut key = owner.to_be_bytes().to_vec();
                    key.push(3);
                    key.extend_from_slice(&(extent * 16).to_be_bytes());
                    let mut value = data(job, extent).to_le_bytes().to_vec();
                    value.extend_from_slice(&16u32.to_le_bytes());
                    value.extend_from_slice(&[0; 4]);
                    (key, value)
                })
                .collect();
            let (tree, _) = build(
                TreeKind::Catalog,
                &records,
                &mut next,
                &mut pages,
                GEOMETRY,
                1,
            );
            let mut key = vec![2];
            key.extend_from_slice(&(job + 1).to_be_bytes());
            let mut value = vec![0; 32];
            value[..8].copy_from_slice(&owner.to_le_bytes());
            value[8] = 3;
            value[9] = 3;
            value[16..24].copy_from_slice(&tree.block.to_le_bytes());
            value[24..].copy_from_slice(&tree.generation.to_le_bytes());
            job_records.push((key, value));
        }
        let (cleanup, _) = build(
            TreeKind::Cleanup,
            &job_records,
            &mut next,
            &mut pages,
            GEOMETRY,
            1,
        );
        // Four one-record allocation leaves under one root.
        let metadata_end = next + 5;
        let data_end = data(jobs, 0);
        assert!(metadata_end <= 4096 && data_end <= blocks - 512);
        let allocated = |block: u64| {
            block < metadata_end
                || crate::workspace::INITIAL.contains(&block)
                || (16_384..data_end).contains(&block)
                || block >= blocks - 512
        };
        let allocation_records: Vec<_> = (0..blocks.div_ceil(GROUP))
            .map(|group| {
                let mut bitmap = vec![0; BITMAP];
                for block in group * GROUP..((group + 1) * GROUP).min(blocks) {
                    set(&mut bitmap, block, allocated(block));
                }
                (group.to_be_bytes().to_vec(), bitmap)
            })
            .collect();
        let (allocation, _) = build(
            TreeKind::Allocation,
            &allocation_records,
            &mut next,
            &mut pages,
            GEOMETRY,
            1,
        );
        assert_eq!(next, metadata_end);
        let used = (0..blocks).filter(|block| allocated(*block)).count() as u64;
        let root = Root {
            allocation,
            cleanup,
            next_object: jobs + 2,
            next_job: jobs + 1,
            free_blocks: blocks - used + 8192,
            ..initial
        };
        pages.insert(256, root.encode(256, GEOMETRY).unwrap());
        pages.insert(blocks - 257, root.encode(blocks - 257, GEOMETRY).unwrap());
        pages.extend(
            workspace::encode(&root, &crate::workspace::INITIAL.collect::<Vec<_>>()).unwrap(),
        );
        pages
    }

    // Drains maintenance to quiescence, checking every successor's ownership,
    // the workspace floor, and per-step net metadata growth.
    fn drain(device: &mut FaultDevice, mut root: Root, limit: usize) -> (Root, usize, i64) {
        let mut pages = crate::check::validate(device, &root)
            .unwrap()
            .metadata_pages as i64;
        let mut growth = 0;
        for step in 0..limit {
            let Some(plan) = prepare_maintenance(device, &root, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
            else {
                return (root, step, growth);
            };
            verify_plan(device, &plan);
            root = apply(device, plan);
            assert!(workspace::read(device, &root).unwrap().len() >= MAX_PAGES);
            let after = crate::check::validate(device, &root)
                .unwrap()
                .metadata_pages as i64;
            growth = growth.max(after - pages);
            pages = after;
        }
        panic!("maintenance did not quiesce within {limit} steps");
    }

    #[test]
    fn full_job_leaf_backlog_drains_through_cleanup_splits() {
        let mut device = FaultDevice::new(job_backlog_image(89, 33), MountMode::ReadWrite);
        let root = selected(&device);
        let (root, steps, growth) = drain(&mut device, root, 400);
        let summary = crate::check::validate(&mut device, &root).unwrap();
        eprintln!("89-job backlog: steps={steps} max step growth={growth}");
        assert_eq!(summary.data_blocks, 0);
        assert!(growth <= MAX_STEP_GROWTH as i64, "step growth {growth}");
    }

    #[test]
    fn orphan_at_workspace_floor_refills_before_detaching() {
        let mut device = FaultDevice::new(orphan_image(16), MountMode::ReadWrite);
        let root = selected(&device);
        let root = shrink_workspace(&mut device, root, MAX_PAGES);
        let first = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        assert_eq!(first.root.next_job, root.next_job, "detached below target");
        let (root, steps, growth) = drain(&mut device, root, 64);
        let summary = crate::check::validate(&mut device, &root).unwrap();
        eprintln!("orphan at floor: steps={steps} max step growth={growth}");
        assert_eq!(summary.data_blocks, 0);
        assert!(workspace::read(&mut device, &root).unwrap().len() >= TARGET - BATCH);
    }

    #[test]
    fn metadata_replacement_stays_bounded_through_repeated_reopens() {
        let mut volume = formatted_volume();
        advance(&mut volume).unwrap();
        let image = volume.into_model_image();
        let mut device = FaultDevice::new(image, MountMode::ReadWrite);
        let mut maximum_allocated = 0;
        for _ in 0..40 {
            let before = selected(&device);
            let Some(plan) = prepare_maintenance(&mut device, &before, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
            else {
                break;
            };
            assert!(
                plan.writes
                    .iter()
                    .filter(|(block, _)| !workspace::is_bank_block(*block, before.blocks))
                    .count()
                    < 20
            );
            verify_plan(&mut device, &plan);
            let media = device.media.clone();
            let mut mounted = Volume::open(device).unwrap();
            mounted.sync().unwrap();
            let after = selected(&FaultDevice::new(
                media.borrow().durable.clone(),
                MountMode::ReadOnly,
            ));
            assert_eq!(after, plan.root);
            maximum_allocated = maximum_allocated.max(after.blocks - 1024 - after.free_blocks);
            drop(mounted);
            media.borrow_mut().crash();
            device = FaultDevice::new(media.borrow().durable.clone(), MountMode::ReadWrite);
        }
        assert!(maximum_allocated < 20);
    }

    fn cleanup_stages() -> Vec<BTreeMap<u64, Page>> {
        let mut volume = formatted_volume();
        advance(&mut volume).unwrap();
        let mut stages = vec![volume.into_model_image()];
        // Refill below TARGET - BATCH and release above TARGET + BATCH.
        for (slots, extra) in [(TARGET - 2 * BATCH, 0), (TARGET, 2 * BATCH as u64)] {
            let mut device = FaultDevice::new(orphan_image(0), MountMode::ReadWrite);
            let base = selected(&device);
            let root = shrink_workspace(&mut device, base, slots);
            if extra != 0 {
                let mut transaction = Transaction::new(&mut device, &root).unwrap();
                transaction.refill.extend(16_384..16_384 + extra);
                let plan = transaction.seal(true).unwrap();
                apply(&mut device, plan);
            }
            let stage = device.media.borrow().durable.clone();
            stages.push(stage);
        }
        for blocks in [0, 1024] {
            let mut volume =
                Volume::open(FaultDevice::new(orphan_image(blocks), MountMode::ReadWrite)).unwrap();
            for _ in 0..4 {
                let image = volume.into_model_image();
                let mut probe = FaultDevice::new(image.clone(), MountMode::ReadWrite);
                let root = selected(&probe);
                if prepare_maintenance(&mut probe, &root, &BTreeSet::new(), 0, None)
                    .map(|(plan, _)| plan)
                    .unwrap()
                    .is_some()
                {
                    stages.push(image.clone());
                }
                volume = Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).unwrap();
                volume.sync().unwrap();
            }
        }
        stages
    }

    #[test]
    fn reclamation_failure_preserves_live_blocks_and_atomic_ownership() {
        use crate::volume::model::{Failure, Tear};
        for base in cleanup_stages() {
            let mut probe = FaultDevice::new(base.clone(), MountMode::ReadWrite);
            let root = selected(&probe);
            let plan = prepare_maintenance(&mut probe, &root, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
                .unwrap();
            verify_plan(&mut probe, &plan);
            let mut old = Forest::new(&mut probe, &root).unwrap();
            let allocation = old.root(root.allocation, TreeKind::Allocation).unwrap();
            let bitmap = old
                .lookup(&allocation, TreeKind::Allocation, &0u64.to_be_bytes())
                .unwrap()
                .unwrap();
            for event in 0..plan.writes.len() + 5 {
                for (mask, tear) in [
                    (0, None),
                    (u64::MAX, None),
                    (
                        0x5555,
                        Some(Tear::Prefix {
                            write: 0,
                            bytes: 2048,
                        }),
                    ),
                    (
                        0xaaaa,
                        Some(Tear::Sectors {
                            write: 0,
                            mask: 0x55,
                        }),
                    ),
                ] {
                    let device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
                    let media = device.media.clone();
                    let mut volume = Volume::open(device).unwrap();
                    media.borrow_mut().failure = Some(Failure {
                        event,
                        whole_mask: mask,
                        tear,
                    });
                    assert!(volume.sync().is_err());
                    let output_count = media.borrow().events.len();
                    assert!(matches!(volume.sync(), Err(Error::Faulted)));
                    assert!(matches!(volume.eject(), Err(Error::Faulted)));
                    assert_eq!(media.borrow().events.len(), output_count);
                    drop(volume);
                    media.borrow_mut().crash();
                    let durable = media.borrow().durable.clone();
                    for (block, page) in &base {
                        if (512..GROUP).contains(block)
                            && bit(&bitmap, *block)
                            && !plan.writes.iter().any(|(target, _)| target == block)
                        {
                            assert_eq!(durable[block].0, page.0, "overwrote base owner {block}");
                        }
                    }
                    let readonly = FaultDevice::new(durable.clone(), MountMode::ReadOnly);
                    let reads = readonly.media.clone();
                    Volume::open(readonly).unwrap().check().unwrap();
                    assert!(reads.borrow().events.is_empty());
                    let recovered = FaultDevice::new(durable, MountMode::ReadWrite);
                    let mirrors = recovered.media.clone();
                    let mut reopened = Volume::open(recovered).unwrap();
                    let after = selected(&FaultDevice::new(
                        mirrors.borrow().durable.clone(),
                        MountMode::ReadOnly,
                    ));
                    assert!(
                        after == root || after == plan.root,
                        "recovery changed transaction contents"
                    );
                    assert_eq!(
                        Root::decode(
                            &mirrors.borrow().durable[&(root.blocks - 257)].0[..],
                            root.blocks - 257,
                            GEOMETRY
                        )
                        .unwrap(),
                        after
                    );
                    assert!(mirrors.borrow().events.iter().all(|event| matches!(event, Event::Flush) || matches!(event, Event::Write(block) if *block == 256 || *block == root.blocks - 257)));
                    reopened.sync().unwrap();
                    let mut checked = Volume::open(FaultDevice::new(
                        reopened.into_model_image(),
                        MountMode::ReadOnly,
                    ))
                    .unwrap();
                    checked.check().unwrap();
                }
            }
        }
    }

    #[test]
    fn interrupted_cleanup_mirroring_is_idempotent_before_reclamation() {
        use crate::volume::model::{Failure, Tear};
        for base in cleanup_stages() {
            let mut device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
            let old = selected(&device);
            let plan = prepare_maintenance(&mut device, &old, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
                .unwrap();
            let root_b = old.blocks - 257;
            let mut committed = base.clone();
            committed.extend(plan.writes.iter().cloned());
            for block in [256, root_b] {
                committed.insert(block, plan.root.encode(block, GEOMETRY).unwrap());
            }
            for stale in [256, root_b] {
                let mut interrupted = committed.clone();
                interrupted.insert(stale, base[&stale].clone());
                for event in 0..2 {
                    for mask in [0, u64::MAX] {
                        let device = FaultDevice::new(interrupted.clone(), MountMode::ReadWrite);
                        let media = device.media.clone();
                        media.borrow_mut().failure = Some(Failure {
                            event,
                            whole_mask: mask,
                            tear: Some(Tear::Prefix {
                                write: 0,
                                bytes: 128,
                            }),
                        });
                        assert!(Volume::open(device).is_err());
                        assert_eq!(media.borrow().events[0], Event::Write(stale));
                        assert_eq!(media.borrow().events.len(), event + 1);
                        media.borrow_mut().crash();
                        let device =
                            FaultDevice::new(media.borrow().durable.clone(), MountMode::ReadWrite);
                        let media = device.media.clone();
                        let mut reopened = Volume::open(device).unwrap();
                        assert_eq!(
                            selected(&FaultDevice::new(
                                media.borrow().durable.clone(),
                                MountMode::ReadOnly
                            )),
                            plan.root
                        );
                        assert!(
                            media.borrow().events.is_empty()
                                || media.borrow().events == [Event::Write(stale), Event::Flush]
                        );
                        reopened.sync().unwrap();
                        Volume::open(FaultDevice::new(
                            reopened.into_model_image(),
                            MountMode::ReadOnly,
                        ))
                        .unwrap()
                        .check()
                        .unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn rejected_plans_issue_no_writes() {
        let mut volume = formatted_volume();
        advance(&mut volume).unwrap();
        let base = volume.into_model_image();
        let mut device = FaultDevice::new(base, MountMode::ReadWrite);
        let mut root = selected(&device);
        root.free_blocks = 0;
        let error = match prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
        {
            Ok(_) => panic!("accepted invalid free count"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::Corrupt(_)));
        assert!(
            device
                .media
                .borrow()
                .events
                .iter()
                .all(|event| !matches!(event, Event::Write(_)))
        );
    }

    #[test]
    fn exhausted_planning_work_leaves_the_base_reusable() {
        let base = orphan_image(16);
        let mut device = FaultDevice::new(base.clone(), MountMode::ReadWrite);
        let root = selected(&device);
        let mut transaction = Transaction::new(&mut device, &root).unwrap();
        let mut marker = vec![3];
        marker.extend_from_slice(&2u64.to_be_bytes());
        for _ in 0..=crate::tree::MAX_DRAFTS {
            match transaction.forest.put(
                transaction.cleanup.clone(),
                TreeKind::Cleanup,
                &marker,
                &[],
            ) {
                Ok(cleanup) => transaction.cleanup = cleanup,
                Err(Error::LimitExceeded) => break,
                Err(error) => panic!("unexpected planning error: {error}"),
            }
        }
        assert!(matches!(transaction.seal(true), Err(Error::LimitExceeded)));
        assert!(device.media.borrow().events.is_empty());
        assert!(
            device
                .media
                .borrow()
                .durable
                .iter()
                .all(|(block, page)| page.0 == base[block].0)
        );
        let plan = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        verify_plan(&mut device, &plan);
    }

    #[test]
    fn cursor_wrap_refills_partial_group_without_stranding_remaining_free_space() {
        let geometry = crate::Geometry {
            byte_len: (286 * GROUP + 513) * 4096,
            ..GEOMETRY
        };
        let initial = formatted_volume().into_model_image();
        let mut root = Root::decode(&initial[&256].0[..], 256, GEOMETRY).unwrap();
        root.blocks = geometry.blocks();
        root.free_blocks = MAX_PAGES as u64 + 128;
        root.workspace_cursor = 158;
        let zero_start = 127 * GROUP + 1000;
        let zero_end = zero_start + 127;
        let allocation_records: Vec<_> = (0u64..286)
            .map(|group| {
                let mut bitmap = vec![0xff; BITMAP];
                if group == 127 {
                    for block in zero_start..zero_end {
                        set(&mut bitmap, block, false);
                    }
                }
                (group.to_be_bytes().to_vec(), bitmap)
            })
            .collect();
        let mut pages = BTreeMap::from([(512, initial[&512].clone())]);
        let mut next = 513;
        root.allocation = build(
            TreeKind::Allocation,
            &allocation_records,
            &mut next,
            &mut pages,
            geometry,
            1,
        )
        .0;
        root.cleanup = TreeRef {
            block: next,
            generation: 1,
        };
        next += 1;
        let metadata_end = next;
        let final_free = 286 * GROUP;
        let retirement = |start: u64, end: u64| {
            let mut key = vec![1];
            key.extend_from_slice(&1u64.to_be_bytes());
            key.extend_from_slice(&start.to_be_bytes());
            let mut value = u32::try_from(end - start).unwrap().to_le_bytes().to_vec();
            value.extend_from_slice(&[0; 4]);
            (key, value)
        };
        let retired = [
            retirement(metadata_end, 8192),
            retirement(12_288, zero_start),
            retirement(zero_end, final_free),
        ];
        let borrowed: Vec<_> = retired
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
            .collect();
        pages.insert(
            root.cleanup.block,
            encode_leaf(
                root.cleanup,
                &root.uuid,
                TreeKind::Cleanup,
                &borrowed,
                root.blocks,
            )
            .unwrap(),
        );
        let slots: Vec<_> = (8192..12_288).collect();
        pages.extend(workspace::encode(&root, &slots).unwrap());
        pages.insert(256, root.encode(256, geometry).unwrap());
        pages.insert(
            root.blocks - 257,
            root.encode(root.blocks - 257, geometry).unwrap(),
        );
        let mut device = FaultDevice::with_geometry(pages, MountMode::ReadWrite, geometry);
        crate::check::validate(&mut device, &root).unwrap();

        let cursor = prepare_maintenance(&mut device, &root, &BTreeSet::new(), 0, None)
            .map(|(plan, _)| plan)
            .unwrap()
            .unwrap();
        assert!(cursor.writes.is_empty());
        assert_eq!(cursor.root.workspace_cursor, 286);
        assert_eq!(cursor.root.free_blocks, root.free_blocks);
        let skipped_partial =
            prepare_maintenance(&mut device, &cursor.root, &BTreeSet::new(), 0, None)
                .map(|(plan, _)| plan)
                .unwrap()
                .unwrap();
        assert!(skipped_partial.writes.is_empty());
        assert_eq!(skipped_partial.root.workspace_cursor, 127);
        let refill = prepare_maintenance(
            &mut device,
            &skipped_partial.root,
            &BTreeSet::new(),
            0,
            None,
        )
        .map(|(plan, _)| plan)
        .unwrap()
        .unwrap();
        assert_ne!(refill.root.workspace, skipped_partial.root.workspace);
        verify_plan(&mut device, &refill);
    }
}
