//! Bounded COW edits. Draft links have no disk address until allocation converges.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use crate::format::{
    BLOCK_BYTES, Page, Root, TreeKind, TreeRef, decode_page, encode_node, leaf_data_blocks,
    leaf_record_blocks,
};
use crate::{BlockDevice, Error, Result};

const PAYLOAD: usize = BLOCK_BYTES - 80;
pub(crate) const MAX_DRAFTS: usize = 4096;
const MAX_READS: usize = 1_048_576;
const CACHE_PAGES: usize = 4096;
pub(crate) type Record = (Vec<u8>, Vec<u8>);

/// A cached or drafted leaf's records in key order: every key and value in
/// one buffer, with a span per record. Decoding a page into a leaf, copying a
/// leaf to change one record, and dropping it each cost two allocations
/// instead of two per record: with a shared allocation per key and per value,
/// loading, copying and dropping nodes took a third to a half of the core's
/// time in a file creation, a removal and a first listing.
#[derive(Default)]
struct Leaf {
    /// Each record's key followed by its value, in no particular order.
    bytes: Vec<u8>,
    spans: Vec<Span>,
    /// Bytes of removed and replaced records still in `bytes`. A copy drops them.
    dead: usize,
}

/// Where one record lies in `Leaf::bytes`.
#[derive(Clone, Copy)]
struct Span {
    start: u32,
    key: u16,
    value: u16,
}

/// Room a copied leaf keeps for the record its edit adds, so the edit does
/// not grow the buffers the copy just allocated.
const LEAF_SLACK: usize = 256;

impl Clone for Leaf {
    fn clone(&self) -> Self {
        let mut copy = Self {
            bytes: Vec::with_capacity(self.bytes.len() - self.dead + LEAF_SLACK),
            spans: Vec::with_capacity(self.spans.len() + 1),
            dead: 0,
        };
        if self.dead == 0 {
            copy.bytes.extend_from_slice(&self.bytes);
            copy.spans.extend_from_slice(&self.spans);
            return copy;
        }
        for span in &self.spans {
            let start = span.start as usize;
            let end = start + usize::from(span.key) + usize::from(span.value);
            // A copy is no longer than its source, whose offsets fit.
            copy.spans.push(Span {
                start: copy.bytes.len() as u32,
                ..*span
            });
            copy.bytes.extend_from_slice(&self.bytes[start..end]);
        }
        copy
    }
}

impl Leaf {
    fn from_records<'a>(records: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> Result<Self> {
        let mut leaf = Self::default();
        for (key, value) in records {
            leaf.push(key, value)?;
        }
        Ok(leaf)
    }

    /// `from_records` for a decoded page, with both buffers allocated once.
    fn from_page(records: &[(&[u8], &[u8])]) -> Result<Self> {
        let bytes = records
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum();
        let mut leaf = Self {
            bytes: Vec::with_capacity(bytes),
            spans: Vec::with_capacity(records.len()),
            dead: 0,
        };
        for (key, value) in records {
            leaf.push(key, value)?;
        }
        Ok(leaf)
    }

    fn len(&self) -> usize {
        self.spans.len()
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    fn key(&self, index: usize) -> &[u8] {
        self.record(index).0
    }

    fn record(&self, index: usize) -> (&[u8], &[u8]) {
        self.at(self.spans[index])
    }

    fn at(&self, span: Span) -> (&[u8], &[u8]) {
        let start = span.start as usize;
        let value = start + usize::from(span.key);
        (
            &self.bytes[start..value],
            &self.bytes[value..value + usize::from(span.value)],
        )
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = (&[u8], &[u8])> {
        self.spans.iter().map(|span| self.at(*span))
    }

    fn slices(&self) -> Vec<(&[u8], &[u8])> {
        self.iter().collect()
    }

    fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.spans
            .binary_search_by(|span| self.at(*span).0.cmp(key))
    }

    /// The number of leading records whose keys satisfy `before`.
    fn partition_point(&self, before: impl Fn(&[u8]) -> bool) -> usize {
        self.spans.partition_point(|span| before(self.at(*span).0))
    }

    /// The bytes record `index` takes in a page.
    fn record_bytes(&self, index: usize) -> usize {
        let span = self.spans[index];
        4 + usize::from(span.key) + usize::from(span.value)
    }

    /// The bytes these records take in a page.
    fn payload(&self) -> usize {
        self.spans
            .iter()
            .map(|span| 4 + usize::from(span.key) + usize::from(span.value))
            .sum()
    }

    /// The first key as an edge's minimum, sharing `old` when it is unchanged.
    fn minimum(&self, old: Option<&Arc<[u8]>>) -> Arc<[u8]> {
        match (self.spans.first(), old) {
            (None, _) => Arc::from(&[][..]),
            (Some(first), Some(old)) if self.at(*first).0 == old.as_ref() => old.clone(),
            (Some(first), _) => Arc::from(self.at(*first).0),
        }
    }

    fn store(&mut self, key: &[u8], value: &[u8]) -> Result<Span> {
        let span = Span {
            start: u32::try_from(self.bytes.len()).map_err(|_| Error::LimitExceeded)?,
            key: u16::try_from(key.len()).map_err(|_| Error::InvalidInput("oversized record"))?,
            value: u16::try_from(value.len())
                .map_err(|_| Error::InvalidInput("oversized record"))?,
        };
        self.bytes.extend_from_slice(key);
        self.bytes.extend_from_slice(value);
        Ok(span)
    }

    fn push(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let span = self.store(key, value)?;
        self.spans.push(span);
        Ok(())
    }

    fn insert(&mut self, index: usize, key: &[u8], value: &[u8]) -> Result<()> {
        let span = self.store(key, value)?;
        self.spans.insert(index, span);
        Ok(())
    }

    fn replace_value(&mut self, index: usize, value: &[u8]) -> Result<()> {
        let old = self.spans[index];
        let start = old.start as usize;
        let key = self.bytes[start..start + usize::from(old.key)].to_vec();
        self.spans[index] = self.store(&key, value)?;
        self.dead += usize::from(old.key) + usize::from(old.value);
        Ok(())
    }

    fn remove(&mut self, index: usize) {
        self.remove_range(index..index + 1);
    }

    fn remove_range(&mut self, range: std::ops::Range<usize>) {
        self.dead += self
            .spans
            .drain(range)
            .map(|span| usize::from(span.key) + usize::from(span.value))
            .sum::<usize>();
    }

    /// Move the records from `at` on into a new leaf.
    fn split_off(&mut self, at: usize) -> Result<Self> {
        let right = Self::from_records(self.spans[at..].iter().map(|span| self.at(*span)))?;
        self.remove_range(at..self.spans.len());
        Ok(right)
    }

    fn append(&mut self, other: &Self) -> Result<()> {
        for (key, value) in other.iter() {
            self.push(key, value)?;
        }
        Ok(())
    }
}

/// Metadata a volume already read and verified, or built and published itself:
/// decoded tree nodes, with a branch's child edges already checked, and the
/// decoded workspace bank. A copy-on-write page never changes under its
/// (block, generation) reference, so entries stay valid across publications.
/// A built page enters only once its publication has committed, because an
/// unpublished plan's generation is reused by the next one, which may assign
/// the same block to another page.
/// Damage behind a cached page goes unnoticed until the entry is dropped.
/// Writable volumes and scrub retain nodes between calls. Other read-only
/// operations cache only until their current catalog read ends.
#[derive(Default)]
pub(crate) struct Cache {
    nodes: HashMap<(u64, u64, TreeKind), Node, BuildHasherDefault<PageHasher>>,
    pub workspace: Option<(TreeRef, Vec<u64>)>,
    /// The leaf the last point lookup ended in, and the root it descended from.
    finger: Option<Finger>,
}

/// Where the last lookup in a published tree ended. A listing asks for the
/// attributes of one object after another, and objects created together sit
/// in the same catalog leaf, so the next key usually lies between this leaf's
/// first and last keys and needs no descent from the root: the descents were
/// about a third of the core's time in a listing that reads no page.
#[derive(Clone, Copy)]
struct Finger {
    root: TreeRef,
    kind: TreeKind,
    leaf: TreeRef,
}

/// Hashes page references for `Cache` and for the checker's lookup memo:
/// every tree level of every read looks one up, and an ordered map spent ten
/// comparisons per level on it. Block numbers come from disk, so a crafted
/// image could make them collide; that can only slow the bounded maps, never
/// make them wrong.
#[derive(Default)]
pub(crate) struct PageHasher(u64);

impl Hasher for PageHasher {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_isize(&mut self, value: isize) {
        self.write_u64(value as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A leaf read from its page and verified as `Forest::disk_page` verifies it.
pub(crate) struct CheckedLeaf {
    reference: TreeRef,
    kind: TreeKind,
    records: Leaf,
    data_blocks: u64,
}

/// A walk over one published tree's leaves in key order, apart from any
/// forest, for a reader that runs ahead of a mounted volume on a device
/// handle of its own. Each page is decoded and verified against its
/// reference; a leaf's place under its parent is checked when a lookup
/// first reaches it, as for any cached node.
pub(crate) struct LeafWalk {
    kind: TreeKind,
    /// Pages still to read, the next one last: reference, the generation of
    /// the page that names it, and the level that page expects.
    pending: Vec<(TreeRef, u64, Option<u8>)>,
    /// Pages left before the walk gives up on a tree it cannot finish.
    budget: usize,
}

impl LeafWalk {
    pub(crate) fn new(root: &Root, kind: TreeKind, reference: TreeRef) -> Self {
        Self {
            kind,
            pending: vec![(reference, root.generation, None)],
            // Every leaf that may be kept, and the branches above them.
            budget: CACHE_PAGES,
        }
    }

    /// Read up to `limit` further leaves. An empty result ends the walk.
    pub(crate) fn next(
        &mut self,
        device: &mut impl BlockDevice,
        root: &Root,
        limit: usize,
    ) -> Result<Vec<CheckedLeaf>> {
        let mut leaves = Vec::new();
        while leaves.len() < limit
            && self.budget > 0
            && let Some((reference, parent_generation, level)) = self.pending.pop()
        {
            self.budget -= 1;
            if reference.block < 512 || reference.block >= root.blocks.saturating_sub(512) {
                return Err(Error::Corrupt("out-of-range tree page"));
            }
            let offset = reference
                .block
                .checked_mul(BLOCK_BYTES as u64)
                .ok_or(Error::Corrupt("tree page offset overflow"))?;
            let mut page = Page(Box::new([0; BLOCK_BYTES]));
            device.read_metadata_exact_at(offset, &mut page.0[..])?;
            let decoded = decode_page(
                &page.0[..],
                reference,
                &root.uuid,
                self.kind,
                parent_generation,
                level,
                root.blocks,
            )?;
            if decoded.level == 0 {
                leaves.push(CheckedLeaf {
                    reference,
                    kind: self.kind,
                    records: Leaf::from_page(&decoded.records)?,
                    data_blocks: decoded.data_blocks,
                });
                continue;
            }
            let first = self.pending.len();
            // Hint the children in runs of adjacent blocks, as `load_disk` does.
            let mut run: Option<(u64, usize)> = None;
            for (_, value) in &decoded.records {
                let child = tree_ref(value)?;
                if child.generation > reference.generation {
                    return Err(Error::Corrupt("child generation exceeds parent"));
                }
                let offset = child
                    .block
                    .checked_mul(BLOCK_BYTES as u64)
                    .ok_or(Error::Corrupt("tree page offset overflow"))?;
                match run {
                    Some((start, len))
                        if start.checked_add(len as u64) == Some(offset) && len < 64 * 1024 =>
                    {
                        run = Some((start, len + BLOCK_BYTES));
                    }
                    Some((start, len)) => {
                        device.prefetch_metadata(start, len)?;
                        run = Some((offset, BLOCK_BYTES));
                    }
                    None => run = Some((offset, BLOCK_BYTES)),
                }
                self.pending
                    .push((child, reference.generation, Some(decoded.level - 1)));
            }
            if let Some((start, len)) = run {
                device.prefetch_metadata(start, len)?;
            }
            self.pending[first..].reverse();
        }
        Ok(leaves)
    }
}

/// Pages encoded at their blocks, and the same pages as nodes to cache.
pub(crate) type Encoded = (Vec<(u64, Page)>, Vec<Built>);

/// A page a plan encoded, as `load_disk` would rebuild it from those bytes.
pub(crate) struct Built {
    reference: TreeRef,
    node: Node,
}

impl Cache {
    /// Cache the pages a committed publication wrote, so the next transaction
    /// does not read back, decode, and re-verify pages this volume just built.
    /// Rereading them was about 12 device reads per file creation through the
    /// Linux adapter.
    pub(crate) fn insert_built(&mut self, built: Vec<Built>) {
        for Built { reference, node } in built {
            self.insert(reference, node);
        }
    }

    /// Keep a leaf the writable open's check has just read and verified, while
    /// half the bound is free: the first operations after a mount then use
    /// the pages the check decoded instead of reading and verifying each one
    /// again, which was most of the core's time in a first listing.
    /// Branches are left to their first use, which checks their children.
    pub(crate) fn keep_checked_leaf(
        &mut self,
        reference: TreeRef,
        kind: TreeKind,
        records: &[(&[u8], &[u8])],
        data_blocks: u64,
    ) -> Result<()> {
        let key = (reference.block, reference.generation, kind);
        if self.has_room() && !self.nodes.contains_key(&key) {
            self.keep(CheckedLeaf {
                reference,
                kind,
                records: Leaf::from_page(records)?,
                data_blocks,
            });
        }
        Ok(())
    }

    /// Whether leaves read ahead of use may still enter: they take at most
    /// half the bound, so the first reads that follow do not empty the cache.
    pub(crate) fn has_room(&self) -> bool {
        self.nodes.len() < CACHE_PAGES / 2
    }

    /// Keep a verified leaf unless its page is already cached.
    pub(crate) fn keep(&mut self, leaf: CheckedLeaf) {
        let edge = Edge {
            link: Link::Disk(leaf.reference),
            minimum: leaf.records.minimum(None),
            level: 0,
            data_blocks: leaf.data_blocks,
        };
        self.nodes
            .entry((leaf.reference.block, leaf.reference.generation, leaf.kind))
            .or_insert(Node {
                kind: leaf.kind,
                entries: Entries::Leaf(leaf.records),
                edge,
            });
    }

    fn insert(&mut self, reference: TreeRef, node: Node) {
        // At the bound, drop all leaves and keep branches. Loading a branch reads
        // and verifies every child page, so clearing a branch made the next
        // lookup below it cost about 60 page loads: a 5,139-file catalog
        // overflowed the bound every few lookups and each `attributes` call
        // reloaded the root. Branches are a small fraction of a tree; if they
        // alone fill half the bound, clear everything.
        if self.nodes.len() >= CACHE_PAGES {
            self.nodes
                .retain(|_, node| matches!(node.entries, Entries::Branch(_)));
            if self.nodes.len() >= CACHE_PAGES / 2 {
                self.nodes.clear();
            }
        }
        self.nodes
            .insert((reference.block, reference.generation, node.kind), node);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    Disk(TreeRef),
    Draft(usize),
}

#[derive(Clone, Debug)]
pub(crate) struct Edge {
    pub link: Link,
    /// Shared, so copying a branch to change one child does not allocate a
    /// key per child: copying and dropping branch nodes was a third of a file
    /// creation's time.
    pub minimum: Arc<[u8]>,
    pub level: u8,
    pub data_blocks: u64,
}

#[derive(Clone)]
enum Entries {
    Leaf(Leaf),
    Branch(Vec<Edge>),
}

#[derive(Clone)]
struct Node {
    kind: TreeKind,
    entries: Entries,
    edge: Edge,
}

pub(crate) struct Forest<'a, D> {
    pub device: &'a mut D,
    pub base: &'a Root,
    pub generation: u64,
    nodes: Vec<Node>,
    pub retired: BTreeSet<u64>,
    /// Blocks added to `retired` by this forest, in order, so a mark records
    /// a length instead of copying the set.
    retired_log: Vec<u64>,
    reads: usize,
    drafts: usize,
    cache: Option<&'a mut Cache>,
    /// File data pages the current operation wrote, in order, and their
    /// positions by block. Reads of these blocks are served from here. An
    /// eager plan publishes them; a batch sends them once it accepts the
    /// operation (`send_data`), so a refused one never reaches the device.
    data: Vec<(u64, Page)>,
    data_index: BTreeMap<u64, usize>,
    /// Data pages this batch has already sent to the device.
    sent: usize,
    /// Drafts left after the last `compact`.
    compacted: usize,
}

/// A forest's own state between the operations of a batch: everything except
/// the borrowed device, base root, and cache. The base root does not change
/// while a batch is open, so a resumed forest continues where it stopped.
#[derive(Default)]
pub(crate) struct Drafts {
    generation: u64,
    nodes: Vec<Node>,
    retired: BTreeSet<u64>,
    reads: usize,
    drafts: usize,
    sent: usize,
    compacted: usize,
}

impl Drafts {
    /// Whether these drafts were built on `base`, the only root they resume on.
    pub fn follows(&self, base: &Root) -> bool {
        Some(self.generation) == base.generation.checked_add(1)
    }
}

/// Where a forest stood before one operation, so that a failed or refused
/// operation leaves no trace. Drafts and data pages are only appended, so
/// undoing them truncates; `retired` is the one set an edit adds to, and its
/// additions are undone from the log.
pub(crate) struct Mark {
    nodes: usize,
    retired: usize,
    reads: usize,
    drafts: usize,
    data: usize,
}

/// What a removal descent did to the node it visited.
enum Removal {
    /// The key is not in the tree, so the node is unchanged.
    Absent,
    /// The key is the leaf's only record; removing it would drop nodes.
    EmptiesLeaf,
    /// Copies that take the node's place in its parent.
    Replaced(Vec<Edge>),
}

impl<'a, D: BlockDevice> Forest<'a, D> {
    #[cfg(any(test, feature = "fuzzing"))]
    pub fn new(device: &'a mut D, base: &'a Root) -> Result<Self> {
        Self::cached(device, base, None)
    }

    pub fn cached(device: &'a mut D, base: &'a Root, cache: Option<&'a mut Cache>) -> Result<Self> {
        Ok(Self {
            device,
            base,
            cache,
            generation: base.generation.checked_add(1).ok_or(Error::LimitExceeded)?,
            nodes: Vec::new(),
            retired: BTreeSet::new(),
            retired_log: Vec::new(),
            reads: 0,
            drafts: 0,
            data: Vec::new(),
            data_index: BTreeMap::new(),
            sent: 0,
            compacted: 0,
        })
    }

    /// Continue a suspended forest against the same base root.
    /// See `Drafts::follows`.
    pub fn resume(
        device: &'a mut D,
        base: &'a Root,
        cache: Option<&'a mut Cache>,
        drafts: Drafts,
    ) -> Result<Self> {
        if !drafts.follows(base) {
            return Err(Error::InvalidInput("batch resumed against another root"));
        }
        Ok(Self {
            device,
            base,
            cache,
            generation: drafts.generation,
            nodes: drafts.nodes,
            retired: drafts.retired,
            retired_log: Vec::new(),
            reads: drafts.reads,
            drafts: drafts.drafts,
            data: Vec::new(),
            data_index: BTreeMap::new(),
            sent: drafts.sent,
            compacted: drafts.compacted,
        })
    }

    /// Release the borrows and keep the rest for the next operation.
    pub fn suspend(self) -> Drafts {
        Drafts {
            generation: self.generation,
            nodes: self.nodes,
            retired: self.retired,
            reads: self.reads,
            drafts: self.drafts,
            sent: self.sent,
            compacted: self.compacted,
        }
    }

    /// Drop drafts that no longer hang from `roots` and renumber the rest.
    ///
    /// Every edit copies its path, so an operation in a batch leaves the
    /// previous copies of re-edited nodes behind. They are never written, but
    /// counted against the draft budget they sealed a batch of small-file
    /// operations every 150 files with 4% of its drafts live. Runs between
    /// operations, when no mark can refer to the old numbering, once there are
    /// `at_least` drafts and twice as many as after the last run.
    pub fn compact(&mut self, roots: &mut [&mut Edge], at_least: usize) {
        if self.nodes.len() < at_least || self.nodes.len() < 2 * self.compacted {
            return;
        }
        let mut live = vec![false; self.nodes.len()];
        let mut pending: Vec<usize> = roots
            .iter()
            .filter_map(|root| match root.link {
                Link::Draft(id) => Some(id),
                Link::Disk(_) => None,
            })
            .collect();
        while let Some(id) = pending.pop() {
            let Some(seen) = live.get_mut(id) else {
                continue;
            };
            if std::mem::replace(seen, true) {
                continue;
            }
            if let Entries::Branch(children) = &self.nodes[id].entries {
                pending.extend(children.iter().filter_map(|child| match child.link {
                    Link::Draft(id) => Some(id),
                    Link::Disk(_) => None,
                }));
            }
        }
        // Old id to new id, keeping creation order.
        let mut renumbered = vec![usize::MAX; self.nodes.len()];
        let mut next = 0;
        for (old, keep) in live.iter().enumerate() {
            if *keep {
                renumbered[old] = next;
                next += 1;
            }
        }
        let relink = |link: &mut Link| {
            if let Link::Draft(id) = link {
                *id = renumbered[*id];
            }
        };
        let nodes = std::mem::take(&mut self.nodes);
        for (old, mut node) in nodes.into_iter().enumerate() {
            if !live[old] {
                continue;
            }
            relink(&mut node.edge.link);
            if let Entries::Branch(children) = &mut node.entries {
                for child in children {
                    relink(&mut child.link);
                }
            }
            self.nodes.push(node);
        }
        for root in roots {
            relink(&mut root.link);
        }
        self.drafts = self.nodes.len();
        self.compacted = self.nodes.len();
    }

    pub fn mark(&self) -> Mark {
        Mark {
            nodes: self.nodes.len(),
            retired: self.retired_log.len(),
            reads: self.reads,
            drafts: self.drafts,
            data: self.data.len(),
        }
    }

    /// Undo everything since `mark`. Nothing reachable from the transaction's
    /// roots at `mark` is touched, because edits only ever append drafts.
    pub fn rewind(&mut self, mark: Mark) {
        self.nodes.truncate(mark.nodes);
        for block in self.retired_log.drain(mark.retired..) {
            self.retired.remove(&block);
        }
        self.reads = mark.reads;
        self.drafts = mark.drafts;
        for (block, _) in self.data.drain(mark.data..) {
            self.data_index.remove(&block);
        }
    }

    /// Page loads counted against this transaction. A read-only operation on
    /// an open batch runs against its own budget and then restores this.
    pub fn reads(&self) -> usize {
        self.reads
    }

    pub fn set_reads(&mut self, reads: usize) {
        self.reads = reads;
    }

    /// Queue a data page for `block`, once.
    pub fn write_data(&mut self, block: u64, page: Page) -> Result<()> {
        if self.data_index.contains_key(&block) {
            return Err(Error::InvalidInput("data block written twice in a plan"));
        }
        self.data_index.insert(block, self.data.len());
        self.data.push((block, page));
        Ok(())
    }

    /// Data pages held for the current plan plus those the batch sent.
    pub fn data_pages(&self) -> usize {
        self.data.len() + self.sent
    }

    /// Data pages held for the current plan.
    pub fn held_pages(&self) -> usize {
        self.data.len()
    }

    /// Data pages the batch sent before this plan was sealed.
    pub fn sent_pages(&self) -> usize {
        self.sent
    }

    /// Send the held data pages to the device. They go to blocks this batch
    /// allocated and no durable root references, and the batch keeps those
    /// blocks allocated until it is sealed, so none is written twice before
    /// the flush that precedes the plan's roots. The device returns them to
    /// later reads.
    pub fn send_data(&mut self) -> Result<()> {
        self.data_index.clear();
        for (block, page) in std::mem::take(&mut self.data) {
            let offset = block
                .checked_mul(BLOCK_BYTES as u64)
                .ok_or(Error::InvalidInput("data block offset overflow"))?;
            self.device.write_page(offset, page.0)?;
            self.sent += 1;
        }
        Ok(())
    }

    pub fn take_data(&mut self) -> Vec<(u64, Page)> {
        self.data_index.clear();
        std::mem::take(&mut self.data)
    }

    /// Read file data at `offset`, seeing pages this transaction queued but has
    /// not written yet. A range the queued pages cover entirely skips the
    /// device, whose older bytes would all be overwritten. `run_end` is the
    /// device offset where the extent holding the range ends, and `next_run`
    /// where the file's next extent starts, when the caller looked it up.
    pub fn read_data(
        &mut self,
        offset: u64,
        out: &mut [u8],
        run_end: u64,
        next_run: Option<u64>,
    ) -> Result<()> {
        let block_bytes = BLOCK_BYTES as u64;
        let first = offset / block_bytes;
        let end = offset
            .saturating_add(out.len() as u64)
            .div_ceil(block_bytes);
        let queued = self.data_index.range(first..end).count() as u64;
        if queued < end - first {
            self.device
                .read_data_exact_at(offset, out, run_end, next_run)?;
        }
        if queued == 0 {
            return Ok(());
        }
        for (&block, &at) in self.data_index.range(first..end) {
            let page = &self.data[at].1.0;
            let low = (block * block_bytes).max(offset);
            let high = ((block + 1) * block_bytes).min(offset + out.len() as u64);
            out[(low - offset) as usize..(high - offset) as usize].copy_from_slice(
                &page[(low - block * block_bytes) as usize..(high - block * block_bytes) as usize],
            );
        }
        Ok(())
    }

    fn count_read(&mut self) -> Result<()> {
        self.reads = self.reads.checked_add(1).ok_or(Error::LimitExceeded)?;
        if self.reads > MAX_READS {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }

    fn disk_page(&mut self, reference: TreeRef, kind: TreeKind) -> Result<(Edge, Leaf)> {
        self.count_read()?;
        let mut page = Page(Box::new([0; BLOCK_BYTES]));
        let offset = reference
            .block
            .checked_mul(BLOCK_BYTES as u64)
            .ok_or(Error::Corrupt("tree page offset overflow"))?;
        self.device
            .read_metadata_exact_at(offset, &mut page.0[..])?;
        let decoded = decode_page(
            &page.0[..],
            reference,
            &self.base.uuid,
            kind,
            self.base.generation,
            None,
            self.base.blocks,
        )?;
        let records = Leaf::from_page(&decoded.records)?;
        Ok((
            Edge {
                link: Link::Disk(reference),
                minimum: records.minimum(None),
                level: decoded.level,
                data_blocks: decoded.data_blocks,
            },
            records,
        ))
    }

    fn cached_node(&self, reference: TreeRef, kind: TreeKind) -> Option<&Node> {
        self.cache
            .as_ref()?
            .nodes
            .get(&(reference.block, reference.generation, kind))
    }

    fn remember(&mut self, reference: TreeRef, node: Node) {
        if let Some(cache) = self.cache.as_deref_mut() {
            cache.insert(reference, node);
        }
    }

    /// The page's own edge. A leaf read for it is cached whole, since it is decoded anyway.
    fn summary(&mut self, reference: TreeRef, kind: TreeKind) -> Result<Edge> {
        if let Some(edge) = self
            .cached_node(reference, kind)
            .map(|node| node.edge.clone())
        {
            self.count_read()?;
            return Ok(edge);
        }
        let (edge, records) = self.disk_page(reference, kind)?;
        if edge.level == 0 {
            let node = Node {
                kind,
                entries: Entries::Leaf(records),
                edge: edge.clone(),
            };
            self.remember(reference, node);
        }
        Ok(edge)
    }

    pub fn root(&mut self, reference: TreeRef, kind: TreeKind) -> Result<Edge> {
        self.summary(reference, kind)
    }

    /// Lend the node behind `edge` to `read`, with the same read counting and
    /// edge checks as `load` but without copying it. Reads walk many nodes to
    /// use one entry of each; copying whole branches for that was a quarter of
    /// the time of a file creation.
    fn read_node<T>(
        &mut self,
        edge: &Edge,
        kind: TreeKind,
        read: impl FnOnce(&Node) -> T,
    ) -> Result<T> {
        match edge.link {
            Link::Draft(id) => self
                .nodes
                .get(id)
                .filter(|node| node.kind == kind)
                .map(read)
                .ok_or(Error::InvalidInput("unknown draft tree node")),
            Link::Disk(reference) => {
                if self.cached_node(reference, kind).is_none() {
                    let node = self.load_disk(reference, edge, kind)?;
                    return Ok(read(&node));
                }
                self.count_read()?;
                let node = self
                    .cached_node(reference, kind)
                    .ok_or(Error::InvalidInput("cached tree node vanished"))?;
                // A cached node was verified against the edge that first reached it.
                if node.edge.minimum != edge.minimum
                    || node.edge.level != edge.level
                    || node.edge.data_blocks != edge.data_blocks
                {
                    return Err(Error::Corrupt("tree edge does not match its page"));
                }
                Ok(read(node))
            }
        }
    }

    fn load(&mut self, edge: &Edge, kind: TreeKind) -> Result<Node> {
        match edge.link {
            Link::Draft(id) => self
                .nodes
                .get(id)
                .filter(|node| node.kind == kind)
                .cloned()
                .ok_or(Error::InvalidInput("unknown draft tree node")),
            Link::Disk(reference) => {
                let node = match self.cached_node(reference, kind).cloned() {
                    Some(node) => {
                        self.count_read()?;
                        node
                    }
                    None => self.load_disk(reference, edge, kind)?,
                };
                // A cached node was verified against the edge that first reached it.
                if node.edge.minimum != edge.minimum
                    || node.edge.level != edge.level
                    || node.edge.data_blocks != edge.data_blocks
                {
                    return Err(Error::Corrupt("tree edge does not match its page"));
                }
                Ok(node)
            }
        }
    }

    fn load_disk(&mut self, reference: TreeRef, edge: &Edge, kind: TreeKind) -> Result<Node> {
        let (actual, records) = self.disk_page(reference, kind)?;
        if actual.minimum != edge.minimum
            || actual.level != edge.level
            || actual.data_blocks != edge.data_blocks
        {
            return Err(Error::Corrupt("tree edge does not match its page"));
        }
        let entries = if edge.level == 0 {
            Entries::Leaf(records)
        } else {
            // Every child summary is checked below. Start only those exact
            // reads, avoiding guessed windows over intervening file data.
            if self.cache.is_some() && self.device.mode() == crate::MountMode::ReadOnly {
                let mut run: Option<(u64, usize)> = None;
                for (_, value) in records.iter() {
                    let child = tree_ref(value)?;
                    if child.generation > reference.generation {
                        return Err(Error::Corrupt("child generation exceeds parent"));
                    }
                    if self.cached_node(child, kind).is_some() {
                        continue;
                    }
                    let offset = child
                        .block
                        .checked_mul(BLOCK_BYTES as u64)
                        .ok_or(Error::Corrupt("tree page offset overflow"))?;
                    match run {
                        Some((start, len))
                            if start.checked_add(len as u64) == Some(offset) && len < 64 * 1024 =>
                        {
                            run = Some((start, len + BLOCK_BYTES));
                        }
                        Some((start, len)) => {
                            self.device.prefetch_metadata(start, len)?;
                            run = Some((offset, BLOCK_BYTES));
                        }
                        None => run = Some((offset, BLOCK_BYTES)),
                    }
                }
                if let Some((start, len)) = run {
                    self.device.prefetch_metadata(start, len)?;
                }
            }
            let mut children = Vec::with_capacity(records.len());
            let mut total = 0u64;
            for (key, value) in records.iter() {
                let child_ref = tree_ref(value)?;
                if child_ref.generation > reference.generation {
                    return Err(Error::Corrupt("child generation exceeds parent"));
                }
                let child = self.summary(child_ref, kind)?;
                if child.minimum.as_ref() != key
                    || child.minimum.is_empty()
                    || child.level + 1 != edge.level
                {
                    return Err(Error::Corrupt("child minimum or level mismatch"));
                }
                total = total
                    .checked_add(child.data_blocks)
                    .ok_or(Error::Corrupt("subtree total overflow"))?;
                children.push(child);
            }
            if total != edge.data_blocks {
                return Err(Error::Corrupt("subtree total mismatch"));
            }
            Entries::Branch(children)
        };
        let node = Node {
            kind,
            entries,
            edge: actual,
        };
        if self.cache.is_some() {
            self.remember(reference, node.clone());
        }
        Ok(node)
    }

    pub fn retire_root(&mut self, edge: &Edge) {
        if let Link::Disk(reference) = edge.link
            && self.retired.insert(reference.block)
        {
            self.retired_log.push(reference.block);
        }
    }

    fn leaf(&mut self, kind: TreeKind, records: Leaf) -> Result<Edge> {
        // The real address is assigned at seal; this validates input and totals.
        let total = leaf_data_blocks(
            TreeRef {
                block: 512,
                generation: self.generation,
            },
            &self.base.uuid,
            kind,
            &records.slices(),
            self.base.blocks,
        )?;
        let minimum = records.minimum(None);
        self.draft(kind, Entries::Leaf(records), minimum, 0, total)
    }

    fn branch(&mut self, kind: TreeKind, children: Vec<Edge>) -> Result<Edge> {
        if children.len() == 1 {
            return Ok(children[0].clone());
        }
        let first = children
            .first()
            .ok_or(Error::InvalidInput("empty internal node"))?;
        let level = first
            .level
            .checked_add(1)
            .filter(|level| *level <= 7)
            .ok_or(Error::LimitExceeded)?;
        let mut total = 0u64;
        let mut last: Option<&[u8]> = None;
        for child in &children {
            if child.minimum.is_empty()
                || child.level + 1 != level
                || last.is_some_and(|key| key >= child.minimum.as_ref())
            {
                return Err(Error::InvalidInput("unordered or uneven internal children"));
            }
            total = total
                .checked_add(child.data_blocks)
                .filter(|total| *total <= self.base.blocks)
                .ok_or(Error::Corrupt("subtree total exceeds volume"))?;
            last = Some(&child.minimum);
        }
        let minimum = first.minimum.clone();
        self.draft(kind, Entries::Branch(children), minimum, level, total)
    }

    fn draft(
        &mut self,
        kind: TreeKind,
        entries: Entries,
        minimum: Arc<[u8]>,
        level: u8,
        data_blocks: u64,
    ) -> Result<Edge> {
        self.drafts = self.drafts.saturating_add(1);
        if self.drafts > MAX_DRAFTS {
            return Err(Error::LimitExceeded);
        }
        let edge = Edge {
            link: Link::Draft(self.nodes.len()),
            minimum,
            level,
            data_blocks,
        };
        self.nodes.push(Node {
            kind,
            entries,
            edge: edge.clone(),
        });
        Ok(edge)
    }

    fn pack_leaves(&mut self, kind: TreeKind, records: Leaf, append: bool) -> Result<Vec<Edge>> {
        let bytes = |index: &usize| records.record_bytes(*index);
        let all: Vec<usize> = (0..records.len()).collect();
        if all.iter().any(|index| bytes(index) > PAYLOAD) {
            return Err(Error::InvalidInput("oversized tree record"));
        }
        let mut chunks = pack(all, bytes, 1, append);
        // Most edits do not split their page, and keep the leaf as it is.
        if chunks.len() == 1 {
            return if records.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![self.leaf(kind, records)?])
            };
        }
        chunks.retain(|chunk| !chunk.is_empty());
        chunks
            .into_iter()
            .map(|chunk| {
                let chunk =
                    Leaf::from_records(chunk.into_iter().map(|index| records.record(index)))?;
                self.leaf(kind, chunk)
            })
            .collect()
    }

    fn pack_branches(
        &mut self,
        kind: TreeKind,
        children: Vec<Edge>,
        append: bool,
    ) -> Result<Vec<Edge>> {
        let bytes = |child: &Edge| 20 + child.minimum.len();
        // Every three children fit, so an overflowed chunk can give up two.
        if children.iter().any(|child| bytes(child) > PAYLOAD / 3) {
            return Err(Error::InvalidInput("oversized child key"));
        }
        pack(children, bytes, 2, append)
            .into_iter()
            .map(|chunk| self.branch(kind, chunk))
            .collect()
    }

    pub fn lookup(&mut self, root: &Edge, kind: TreeKind, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(found) = self.lookup_at_finger(root, kind, key)? {
            return Ok(found);
        }
        let mut edge = root.clone();
        loop {
            // A leaf answers with what it holds; a branch names the child to
            // read next, or has no child that could hold the key.
            let step = self.read_node(&edge, kind, |node| match &node.entries {
                Entries::Leaf(records) => Err(Some(
                    records
                        .search(key)
                        .ok()
                        .map(|index| records.record(index).1.to_vec()),
                )),
                Entries::Branch(children) => {
                    let index = children.partition_point(|child| child.minimum.as_ref() <= key);
                    match index.checked_sub(1) {
                        Some(index) => Ok(children[index].clone()),
                        None => Err(None),
                    }
                }
            })?;
            match step {
                Ok(child) => edge = child,
                Err(None) => return Ok(None),
                Err(Some(found)) => {
                    if let (Link::Disk(root), Link::Disk(leaf), Some(cache)) =
                        (root.link, edge.link, self.cache.as_deref_mut())
                    {
                        cache.finger = Some(Finger { root, kind, leaf });
                    }
                    return Ok(found);
                }
            }
        }
    }

    /// Answer a lookup from the leaf the last one ended in, when `root` is
    /// the published root that lookup descended from and `key` lies between
    /// the leaf's first and last keys. Leaves of one tree hold disjoint key
    /// ranges, so the key is in that leaf or nowhere. `None` means the
    /// descent has to run.
    fn lookup_at_finger(
        &mut self,
        root: &Edge,
        kind: TreeKind,
        key: &[u8],
    ) -> Result<Option<Option<Vec<u8>>>> {
        let (Link::Disk(root), Some(cache)) = (root.link, self.cache.as_deref()) else {
            return Ok(None);
        };
        let Some(finger) = cache
            .finger
            .filter(|finger| finger.root == root && finger.kind == kind)
        else {
            return Ok(None);
        };
        let Some(Entries::Leaf(records)) = cache
            .nodes
            .get(&(finger.leaf.block, finger.leaf.generation, kind))
            .map(|node| &node.entries)
        else {
            return Ok(None);
        };
        let Some(last) = records.len().checked_sub(1) else {
            return Ok(None);
        };
        if key < records.key(0) || key > records.key(last) {
            return Ok(None);
        }
        let found = records
            .search(key)
            .ok()
            .map(|index| records.record(index).1.to_vec());
        self.count_read()?;
        Ok(Some(found))
    }

    /// The record with the greatest key less than or equal to `key`.
    pub fn floor(&mut self, root: &Edge, kind: TreeKind, key: &[u8]) -> Result<Option<Record>> {
        let mut edge = root.clone();
        loop {
            let step = self.read_node(&edge, kind, |node| match &node.entries {
                Entries::Leaf(records) => {
                    let index = records.partition_point(|candidate| candidate <= key);
                    Err(index.checked_sub(1).map(|index| {
                        let (key, value) = records.record(index);
                        (key.to_vec(), value.to_vec())
                    }))
                }
                // Each child's minimum is at most `key` and the next child's
                // exceeds it, so the floor lies in this child.
                Entries::Branch(children) => {
                    let index = children.partition_point(|child| child.minimum.as_ref() <= key);
                    match index.checked_sub(1) {
                        Some(index) => Ok(children[index].clone()),
                        None => Err(None),
                    }
                }
            })?;
            match step {
                Ok(child) => edge = child,
                Err(found) => return Ok(found),
            }
        }
    }

    /// Return a bounded ordered page. Start is inclusive.
    pub fn scan(
        &mut self,
        root: &Edge,
        kind: TreeKind,
        start: &[u8],
        limit: usize,
    ) -> Result<Vec<Record>> {
        self.scan_before(root, kind, start, None, limit)
    }

    /// `scan`, stopping before the first key at or past `end`, so a caller
    /// looking for one object's records does not copy its neighbours'.
    pub fn scan_before(
        &mut self,
        root: &Edge,
        kind: TreeKind,
        start: &[u8],
        end: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let past = |key: &[u8]| end.is_some_and(|end| key >= end);
        if limit > 64 {
            return Err(Error::LimitExceeded);
        }
        let mut out = Vec::new();
        let mut pending = vec![root.clone()];
        let mut ended = false;
        while let Some(edge) = pending.pop() {
            if out.len() == limit || ended {
                break;
            }
            let overlap = self.read_node(&edge, kind, |node| match &node.entries {
                Entries::Leaf(records) => {
                    // A leaf's keys ascend, so skip to the first one at or
                    // past `start` instead of comparing each.
                    let first = records.partition_point(|key| key < start);
                    for (key, value) in records.iter().skip(first) {
                        if past(key) {
                            ended = true;
                            break;
                        }
                        if out
                            .last()
                            .is_some_and(|last: &Record| last.0.as_slice() >= key)
                        {
                            return true;
                        }
                        out.push((key.to_vec(), value.to_vec()));
                        if out.len() == limit {
                            break;
                        }
                    }
                    false
                }
                Entries::Branch(children) => {
                    let first = children
                        .partition_point(|child| child.minimum.as_ref() <= start)
                        .saturating_sub(1);
                    let last = children.partition_point(|child| !past(&child.minimum));
                    // Each child after the first holds its minimum, a key past
                    // `start`, so one child per record still wanted is enough.
                    let wanted = first + 1 + (limit - out.len());
                    pending.extend(
                        children[first.min(last)..last.min(wanted)]
                            .iter()
                            .rev()
                            .cloned(),
                    );
                    false
                }
            })?;
            if overlap {
                return Err(Error::Corrupt("overlapping child key ranges"));
            }
        }
        Ok(out)
    }

    pub fn split(
        &mut self,
        root: Edge,
        kind: TreeKind,
        key: &[u8],
    ) -> Result<(Option<Edge>, Option<Edge>)> {
        if root.minimum.as_ref() >= key {
            return Ok((None, Some(root)));
        }
        let node = self.load(&root, kind)?;
        match node.entries {
            Entries::Leaf(mut records) => {
                let at = records.partition_point(|candidate| candidate < key);
                if at == records.len() {
                    return Ok((Some(root), None));
                }
                let right = records.split_off(at)?;
                self.retire_root(&root);
                Ok((
                    Some(self.leaf(kind, records)?),
                    Some(self.leaf(kind, right)?),
                ))
            }
            Entries::Branch(mut children) => {
                let at = children.partition_point(|child| child.minimum.as_ref() < key) - 1;
                let right_children = children.split_off(at + 1);
                let boundary = children
                    .pop()
                    .ok_or(Error::Corrupt("missing split boundary"))?;
                let (left, right) = self.split(boundary, kind, key)?;
                self.retire_root(&root);
                let left_base = if children.is_empty() {
                    None
                } else {
                    Some(self.branch(kind, children)?)
                };
                let right_base = if right_children.is_empty() {
                    None
                } else {
                    Some(self.branch(kind, right_children)?)
                };
                Ok((
                    self.join(left_base, left, kind)?,
                    self.join(right, right_base, kind)?,
                ))
            }
        }
    }

    pub fn join(
        &mut self,
        left: Option<Edge>,
        right: Option<Edge>,
        kind: TreeKind,
    ) -> Result<Option<Edge>> {
        let (left, right) = match (left, right) {
            (None, right) => return Ok(right),
            (left, None) => return Ok(left),
            (Some(left), Some(right)) => (left, right),
        };
        if left.minimum >= right.minimum {
            return Err(Error::InvalidInput("join ranges are not ordered"));
        }
        let mut last = left.clone();
        loop {
            match self.load(&last, kind)?.entries {
                Entries::Leaf(records) => {
                    if records
                        .len()
                        .checked_sub(1)
                        .is_some_and(|last| records.key(last) >= right.minimum.as_ref())
                    {
                        return Err(Error::InvalidInput("join ranges overlap"));
                    }
                    break;
                }
                Entries::Branch(children) => {
                    last = children
                        .last()
                        .ok_or(Error::Corrupt("empty branch"))?
                        .clone()
                }
            }
        }
        let roots = self.join_nodes(left, right, kind)?;
        Ok(Some(if roots.len() == 1 {
            roots[0].clone()
        } else {
            self.branch(kind, roots)?
        }))
    }

    fn join_nodes(&mut self, left: Edge, right: Edge, kind: TreeKind) -> Result<Vec<Edge>> {
        if left.level == right.level {
            let a = self.load(&left, kind)?.entries;
            let b = self.load(&right, kind)?.entries;
            self.retire_root(&left);
            self.retire_root(&right);
            match (a, b) {
                (Entries::Leaf(mut a), Entries::Leaf(b)) => {
                    a.append(&b)?;
                    self.pack_leaves(kind, a, false)
                }
                (Entries::Branch(mut a), Entries::Branch(b)) => {
                    a.extend(b);
                    self.pack_branches(kind, a, false)
                }
                _ => Err(Error::Corrupt("equal tree levels have different kinds")),
            }
        } else if left.level > right.level {
            let Entries::Branch(mut children) = self.load(&left, kind)?.entries else {
                return Err(Error::Corrupt("nonzero level is a leaf"));
            };
            let boundary = children.pop().ok_or(Error::Corrupt("empty branch"))?;
            children.extend(self.join_nodes(boundary, right, kind)?);
            self.retire_root(&left);
            self.pack_branches(kind, children, false)
        } else {
            let Entries::Branch(mut children) = self.load(&right, kind)?.entries else {
                return Err(Error::Corrupt("nonzero level is a leaf"));
            };
            let boundary = children.remove(0);
            let mut joined = self.join_nodes(left, boundary, kind)?;
            joined.extend(children);
            self.retire_root(&right);
            self.pack_branches(kind, joined, false)
        }
    }

    // Copy the affected path. Fixed-width replacement preserves its topology.
    fn put_nodes(
        &mut self,
        root: &Edge,
        kind: TreeKind,
        key: &[u8],
        value: &[u8],
    ) -> Result<Vec<Edge>> {
        let node = self.load(root, kind)?;
        let replacements = match node.entries {
            Entries::Leaf(mut records) => {
                // The other records were checked when their page was decoded
                // or when they entered a draft, and `seal` checks every page
                // it encodes, so only the new record needs checking here.
                let mut total = node.edge.data_blocks;
                let new = leaf_record_blocks(kind, key, value, self.base.blocks, self.generation)?;
                let appended = records
                    .len()
                    .checked_sub(1)
                    .is_none_or(|last| records.key(last) < key);
                match records.search(key) {
                    Ok(index) => {
                        // Only catalog extent records carry data blocks.
                        let (old_key, old_value) = records.record(index);
                        let old = if kind == TreeKind::Catalog && old_key.get(8) == Some(&3) {
                            leaf_record_blocks(
                                kind,
                                old_key,
                                old_value,
                                self.base.blocks,
                                self.generation,
                            )?
                        } else {
                            0
                        };
                        total = total
                            .checked_sub(old)
                            .ok_or(Error::Corrupt("leaf data-block total underflow"))?;
                        records.replace_value(index, value)?;
                    }
                    Err(index) => records.insert(index, key, value)?,
                }
                total = total
                    .checked_add(new)
                    .filter(|total| *total <= self.base.blocks)
                    .ok_or(Error::InvalidInput(
                        "leaf data-block total exceeds geometry",
                    ))?;
                if records.payload() <= PAYLOAD {
                    let minimum = records.minimum(Some(&node.edge.minimum));
                    vec![self.draft(kind, Entries::Leaf(records), minimum, 0, total)?]
                } else {
                    self.pack_leaves(kind, records, appended)?
                }
            }
            Entries::Branch(children) => {
                let index = children
                    .partition_point(|child| child.minimum.as_ref() <= key)
                    .saturating_sub(1);
                let replacements = self.put_nodes(&children[index], kind, key, value)?;
                self.replace_child(kind, &node.edge, children, index, replacements)?
            }
        };
        self.retire_root(root);
        Ok(replacements)
    }

    /// A branch with `children[index]` replaced by `replacements`. One child
    /// replaced in a branch that still fits its page needs only the new
    /// child's level, order and total checked; anything else is repacked
    /// and fully checked.
    fn replace_child(
        &mut self,
        kind: TreeKind,
        edge: &Edge,
        mut children: Vec<Edge>,
        index: usize,
        mut replacements: Vec<Edge>,
    ) -> Result<Vec<Edge>> {
        let bytes: usize = children.iter().map(|child| 20 + child.minimum.len()).sum();
        let grown = replacements
            .first()
            .map_or(0, |child| child.minimum.len())
            .saturating_sub(children[index].minimum.len());
        if let ([child], true) = (
            replacements.as_slice(),
            children.len() >= 2 && bytes + grown <= PAYLOAD,
        ) && child.level + 1 == edge.level
            && !child.minimum.is_empty()
            && index
                .checked_sub(1)
                .is_none_or(|previous| children[previous].minimum < child.minimum)
            && children
                .get(index + 1)
                .is_none_or(|next| child.minimum < next.minimum)
        {
            let total = edge
                .data_blocks
                .checked_sub(children[index].data_blocks)
                .and_then(|total| total.checked_add(child.data_blocks))
                .filter(|total| *total <= self.base.blocks)
                .ok_or(Error::Corrupt("subtree total exceeds volume"))?;
            children[index] = replacements.remove(0);
            let minimum = children[0].minimum.clone();
            return Ok(vec![self.draft(
                kind,
                Entries::Branch(children),
                minimum,
                edge.level,
                total,
            )?]);
        }
        // A split of the last child is an append at this branch's end too.
        let append = index + 1 == children.len();
        children.splice(index..index + 1, replacements);
        self.pack_branches(kind, children, append)
    }

    /// A leaf-parent branch without `children[index]`, a leaf whose last
    /// records are going. The branch keeps at least two children, so every
    /// leaf stays at the same depth; with fewer the caller rebuilds the
    /// interval instead. That rebuild is a split and a join of the whole
    /// tree; taking it for every emptied leaf was 71% of a small-file removal.
    fn drop_leaf(
        &mut self,
        kind: TreeKind,
        edge: &Edge,
        mut children: Vec<Edge>,
        index: usize,
    ) -> Result<Edge> {
        let child = children.remove(index);
        self.retire_root(&child);
        let total = edge
            .data_blocks
            .checked_sub(child.data_blocks)
            .ok_or(Error::Corrupt("subtree total underflow"))?;
        let minimum = children[0].minimum.clone();
        self.draft(kind, Entries::Branch(children), minimum, 1, total)
    }

    /// Data blocks a leaf record accounts for; only catalog extents have any.
    fn record_blocks(&self, kind: TreeKind, key: &[u8], value: &[u8]) -> Result<u64> {
        if kind == TreeKind::Catalog && key.get(8) == Some(&3) {
            leaf_record_blocks(kind, key, value, self.base.blocks, self.generation)
        } else {
            Ok(0)
        }
    }

    pub fn put(&mut self, root: Edge, kind: TreeKind, key: &[u8], value: &[u8]) -> Result<Edge> {
        if key.len() > 1024 || value.len() > 3072 {
            return Err(Error::InvalidInput("oversized record"));
        }
        let mut roots = self.put_nodes(&root, kind, key, value)?;
        if roots.len() == 1 {
            return roots
                .pop()
                .ok_or(Error::InvalidInput("insert produced empty tree"));
        }
        self.branch(kind, roots)
    }

    // Copy the affected path, as `put_nodes` does. Emptying the leaf would
    // remove nodes, which can leave a branch with a single child that
    // `Forest::branch` collapses, making leaf depths uneven. Nothing is
    // retired or drafted before the recursion returns, so those removals
    // report back and the caller rebuilds by interval instead.
    fn remove_nodes(&mut self, root: &Edge, kind: TreeKind, key: &[u8]) -> Result<Removal> {
        let node = self.load(root, kind)?;
        let replacements = match node.entries {
            Entries::Leaf(mut records) => {
                let Ok(index) = records.search(key) else {
                    return Ok(Removal::Absent);
                };
                if records.len() == 1 {
                    return Ok(Removal::EmptiesLeaf);
                }
                // The others were checked when they entered this leaf.
                let (old_key, old_value) = records.record(index);
                let total = node
                    .edge
                    .data_blocks
                    .checked_sub(self.record_blocks(kind, old_key, old_value)?)
                    .ok_or(Error::Corrupt("leaf data-block total underflow"))?;
                records.remove(index);
                let minimum = records.minimum(Some(&node.edge.minimum));
                vec![self.draft(kind, Entries::Leaf(records), minimum, 0, total)?]
            }
            Entries::Branch(children) => {
                let index = children
                    .partition_point(|child| child.minimum.as_ref() <= key)
                    .saturating_sub(1);
                match self.remove_nodes(&children[index], kind, key)? {
                    Removal::Replaced(replacements) => {
                        self.replace_child(kind, &node.edge, children, index, replacements)?
                    }
                    Removal::EmptiesLeaf if node.edge.level == 1 && children.len() > 2 => {
                        vec![self.drop_leaf(kind, &node.edge, children, index)?]
                    }
                    other => return Ok(other),
                }
            }
        };
        self.retire_root(root);
        Ok(Removal::Replaced(replacements))
    }

    /// Remove every record in `[low, high)` with one path copy, when they sit
    /// in one leaf that keeps another record. Returns `None`, having changed
    /// nothing, when the range holds nothing, spans leaves, or would empty
    /// its leaf; the caller then removes the records one at a time.
    pub fn remove_run(
        &mut self,
        root: &Edge,
        kind: TreeKind,
        low: &[u8],
        high: &[u8],
    ) -> Result<Option<Edge>> {
        Ok(match self.run_nodes(root, kind, low, high)? {
            // The whole tree would go; the caller handles that.
            Some(roots) if roots.is_empty() => None,
            Some(mut roots) if roots.len() == 1 => roots.pop(),
            Some(roots) => Some(self.branch(kind, roots)?),
            None => None,
        })
    }

    fn run_nodes(
        &mut self,
        root: &Edge,
        kind: TreeKind,
        low: &[u8],
        high: &[u8],
    ) -> Result<Option<Vec<Edge>>> {
        let node = self.load(root, kind)?;
        let replacements = match node.entries {
            Entries::Leaf(mut records) => {
                let first = records.partition_point(|key| key < low);
                let last = records.partition_point(|key| key < high);
                if first == last {
                    return Ok(None);
                }
                if last - first == records.len() {
                    // The parent drops this leaf, or the caller falls back.
                    return Ok(Some(Vec::new()));
                }
                let mut total = node.edge.data_blocks;
                for index in first..last {
                    let (key, value) = records.record(index);
                    total = total
                        .checked_sub(self.record_blocks(kind, key, value)?)
                        .ok_or(Error::Corrupt("leaf data-block total underflow"))?;
                }
                records.remove_range(first..last);
                let minimum = records.minimum(Some(&node.edge.minimum));
                vec![self.draft(kind, Entries::Leaf(records), minimum, 0, total)?]
            }
            Entries::Branch(children) => {
                let index = children
                    .partition_point(|child| child.minimum.as_ref() <= low)
                    .saturating_sub(1);
                if children
                    .get(index + 1)
                    .is_some_and(|next| next.minimum.as_ref() < high)
                {
                    return Ok(None);
                }
                match self.run_nodes(&children[index], kind, low, high)? {
                    None => return Ok(None),
                    Some(emptied) if emptied.is_empty() => {
                        if node.edge.level != 1 || children.len() <= 2 {
                            return Ok(None);
                        }
                        vec![self.drop_leaf(kind, &node.edge, children, index)?]
                    }
                    Some(replacements) => {
                        self.replace_child(kind, &node.edge, children, index, replacements)?
                    }
                }
            }
        };
        self.retire_root(root);
        Ok(Some(replacements))
    }

    pub fn remove(&mut self, root: Edge, kind: TreeKind, key: &[u8]) -> Result<Edge> {
        match self.remove_nodes(&root, kind, key)? {
            Removal::Absent => Ok(root),
            Removal::EmptiesLeaf => self.remove_interval(root, kind, key),
            Removal::Replaced(mut roots) if roots.len() == 1 => roots
                .pop()
                .ok_or(Error::Corrupt("removal produced no replacement")),
            Removal::Replaced(roots) => self.branch(kind, roots),
        }
    }

    fn remove_interval(&mut self, root: Edge, kind: TreeKind, key: &[u8]) -> Result<Edge> {
        let (left, right) = self.split(root, kind, key)?;
        let mut after = key.to_vec();
        after.push(0);
        let (old, right) = self.split(
            right.ok_or(Error::Corrupt("missing remove interval"))?,
            kind,
            &after,
        )?;
        if let Some(old) = old {
            self.retire_root(&old);
        }
        match self.join(left, right, kind)? {
            Some(root) => Ok(root),
            None => self.leaf(kind, Leaf::default()),
        }
    }

    pub fn detach(&mut self, root: Edge, low: &[u8], high: &[u8]) -> Result<(Edge, Option<Edge>)> {
        if low >= high {
            return Err(Error::InvalidInput("empty detach interval"));
        }
        let (left, rest) = self.split(root, TreeKind::Catalog, low)?;
        let (detached, right) = if let Some(rest) = rest {
            self.split(rest, TreeKind::Catalog, high)?
        } else {
            (None, None)
        };
        let kept = match self.join(left, right, TreeKind::Catalog)? {
            Some(root) => root,
            None => self.leaf(TreeKind::Catalog, Leaf::default())?,
        };
        Ok((kept, detached))
    }

    /// Draft pages this transaction has built, including discarded ones. The
    /// maintenance planner fills a plan up to a fraction of `MAX_DRAFTS`.
    pub fn drafts(&self) -> usize {
        self.drafts
    }

    pub fn draft_ids(&self, roots: &[Edge]) -> Result<BTreeSet<usize>> {
        if self.drafts > MAX_DRAFTS || self.reads > MAX_READS {
            return Err(Error::LimitExceeded);
        }
        let mut pending: Vec<_> = roots.iter().map(|root| root.link).collect();
        let mut ids = BTreeSet::new();
        let mut disk_blocks = BTreeSet::new();
        while let Some(link) = pending.pop() {
            match link {
                Link::Disk(reference) => {
                    if !disk_blocks.insert(reference.block) {
                        return Err(Error::InvalidInput("disk subtree has multiple owners"));
                    }
                    if self.retired.contains(&reference.block) {
                        return Err(Error::InvalidInput("retired page remains reachable"));
                    }
                }
                Link::Draft(id) => {
                    if !ids.insert(id) {
                        return Err(Error::InvalidInput("draft node has multiple owners"));
                    }
                    let node = self
                        .nodes
                        .get(id)
                        .ok_or(Error::InvalidInput("unknown draft node"))?;
                    if let Entries::Branch(children) = &node.entries {
                        pending.extend(children.iter().map(|child| child.link));
                    }
                }
            }
        }
        Ok(ids)
    }

    /// Encode each assigned draft at its block, and return with each page the
    /// node `load_disk` would rebuild from it, for the volume to cache once the
    /// publication commits.
    pub fn encode(&self, assignments: &BTreeMap<usize, u64>) -> Result<Encoded> {
        let mut pages = Vec::with_capacity(assignments.len());
        let mut built = Vec::with_capacity(assignments.len());
        for (id, block) in assignments {
            let node = self
                .nodes
                .get(*id)
                .ok_or(Error::InvalidInput("unknown draft node"))?;
            let reference = TreeRef {
                block: *block,
                generation: self.generation,
            };
            // A branch's records: each child's minimum and its reference.
            let mut references = Vec::new();
            let entries = match &node.entries {
                Entries::Leaf(records) => Entries::Leaf(records.clone()),
                Entries::Branch(children) => {
                    let mut resolved = Vec::with_capacity(children.len());
                    for child in children {
                        let child_ref = self.resolve(child, assignments)?;
                        let mut bytes = [0; 16];
                        bytes[..8].copy_from_slice(&child_ref.block.to_le_bytes());
                        bytes[8..].copy_from_slice(&child_ref.generation.to_le_bytes());
                        references.push(bytes);
                        resolved.push(Edge {
                            link: Link::Disk(child_ref),
                            ..child.clone()
                        });
                    }
                    Entries::Branch(resolved)
                }
            };
            let slices: Vec<(&[u8], &[u8])> = match &entries {
                Entries::Leaf(records) => records.slices(),
                Entries::Branch(children) => children
                    .iter()
                    .zip(&references)
                    .map(|(child, reference)| (child.minimum.as_ref(), &reference[..]))
                    .collect(),
            };
            let page = encode_node(
                reference,
                &self.base.uuid,
                node.kind,
                &slices,
                self.base.blocks,
                node.edge.level,
                node.edge.data_blocks,
            )?;
            // The edge as `disk_page` rebuilds it: the minimum comes from the
            // page's first record, not from the parent.
            let edge = Edge {
                link: Link::Disk(reference),
                minimum: slices
                    .first()
                    .map_or_else(|| Arc::from(&[][..]), |(key, _)| Arc::from(*key)),
                level: node.edge.level,
                data_blocks: node.edge.data_blocks,
            };
            drop(slices);
            let node = Node {
                kind: node.kind,
                entries,
                edge,
            };
            #[cfg(debug_assertions)]
            self.assert_rebuilds(&page, reference, &node);
            pages.push((*block, page));
            built.push(Built { reference, node });
        }
        Ok((pages, built))
    }

    /// Test and debug builds prove that a built node is the one `load_disk`
    /// would rebuild from its page, including the checks a branch load makes
    /// on its children. Release builds trust the encoder, which is the saving.
    #[cfg(debug_assertions)]
    fn assert_rebuilds(&self, page: &Page, reference: TreeRef, node: &Node) {
        let decoded = decode_page(
            &page.0[..],
            reference,
            &self.base.uuid,
            node.kind,
            self.generation,
            None,
            self.base.blocks,
        )
        .expect("a built page decodes");
        let minimum = decoded.records.first().map_or(&[][..], |(key, _)| *key);
        assert_eq!(minimum, node.edge.minimum.as_ref(), "built minimum");
        assert_eq!(decoded.level, node.edge.level, "built level");
        assert_eq!(
            decoded.data_blocks, node.edge.data_blocks,
            "built data blocks"
        );
        match &node.entries {
            Entries::Leaf(records) => {
                assert_eq!(decoded.records.len(), records.len(), "built leaf length");
                for ((key, value), (k, v)) in decoded.records.iter().zip(records.iter()) {
                    assert_eq!((*key, *value), (k, v), "built leaf");
                }
            }
            Entries::Branch(children) => {
                assert_eq!(decoded.records.len(), children.len(), "built branch length");
                let mut total = 0u64;
                for ((key, value), child) in decoded.records.iter().zip(children) {
                    assert_eq!(*key, child.minimum.as_ref(), "built child minimum");
                    let reference = tree_ref(value).expect("built child reference");
                    assert_eq!(Link::Disk(reference), child.link, "built child link");
                    assert!(
                        !child.minimum.is_empty() && child.level + 1 == node.edge.level,
                        "built child level"
                    );
                    total += child.data_blocks;
                }
                assert_eq!(total, node.edge.data_blocks, "built subtree total");
            }
        }
    }

    pub fn resolve(&self, edge: &Edge, assignments: &BTreeMap<usize, u64>) -> Result<TreeRef> {
        match edge.link {
            Link::Disk(reference) => Ok(reference),
            Link::Draft(id) => Ok(TreeRef {
                block: *assignments
                    .get(&id)
                    .ok_or(Error::InvalidInput("unassigned draft page"))?,
                generation: self.generation,
            }),
        }
    }
}

pub(crate) fn tree_ref(bytes: &[u8]) -> Result<TreeRef> {
    if bytes.len() != 16 {
        return Err(Error::Corrupt("tree reference length"));
    }
    Ok(TreeRef {
        block: u64_le(&bytes[..8])?,
        generation: u64_le(&bytes[8..])?,
    })
}

pub(crate) fn u64_le(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| Error::Corrupt("u64 field length"))?,
    ))
}

/// Packs entries greedily into chunks of at most `PAYLOAD` bytes, keeping at
/// least `minimum` entries in each chunk that follows an overflow.
///
/// Greedy packing alone leaves a nearly empty final chunk when an entry lands
/// at the front of a full page, so repeated front insertion would spill a new
/// page per insertion. Evening out the last two chunks makes it split once.
/// An `append` (a new last key) keeps earlier chunks full instead: an even
/// split would leave the left page half full forever, since later appends
/// land to its right.
fn pack<T>(
    entries: Vec<T>,
    size: impl Fn(&T) -> usize,
    minimum: usize,
    append: bool,
) -> Vec<Vec<T>> {
    // Most edits do not split their page: return the entries as they are
    // rather than rebuilding them one by one into a growing chunk.
    if entries.iter().map(&size).sum::<usize>() <= PAYLOAD {
        return vec![entries];
    }
    let mut chunks: Vec<(Vec<T>, usize)> = vec![(Vec::new(), 0)];
    for entry in entries {
        let bytes = size(&entry);
        if chunks
            .last()
            .is_some_and(|(_, used)| used + bytes > PAYLOAD)
        {
            chunks.push((Vec::new(), 0));
        }
        if let Some((chunk, used)) = chunks.last_mut() {
            *used += bytes;
            chunk.push(entry);
        }
    }
    if let [.., previous, last] = chunks.as_mut_slice() {
        while previous.0.len() > minimum {
            let Some(bytes) = previous.0.last().map(&size) else {
                break;
            };
            if last.0.len() >= minimum && (append || last.1 + bytes > previous.1 - bytes) {
                break;
            }
            if let Some(entry) = previous.0.pop() {
                previous.1 -= bytes;
                last.1 += bytes;
                last.0.insert(0, entry);
            }
        }
    }
    chunks.into_iter().map(|(chunk, _)| chunk).collect()
}

pub(crate) fn u64_be(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| Error::Corrupt("u64 field length"))?,
    ))
}

#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz(data: &[u8]) {
    use crate::MountMode;
    use crate::volume::model::{FaultDevice, GEOMETRY, formatted_volume};
    let image = formatted_volume().into_model_image();
    let mut base = Root::decode(&image[&256].0[..], 256, GEOMETRY).expect("fixture root");
    let mut device = FaultDevice::new(image, MountMode::ReadWrite);
    let mut expected = BTreeMap::new();
    let mut next_block = 1024;
    for batch in data.chunks(32).take(2) {
        let mut forest = Forest::new(&mut device, &base).expect("edit forest");
        let mut root = forest
            .root(base.cleanup, TreeKind::Cleanup)
            .expect("tree root");
        if base.generation == 1 && data.first().is_some_and(|byte| byte & 2 != 0) {
            let records: Vec<_> = (1..=400u64)
                .map(|id| {
                    let mut key = vec![3];
                    key.extend_from_slice(&id.to_be_bytes());
                    (key, Vec::new())
                })
                .collect();
            expected.extend(records.iter().cloned());
            let records =
                Leaf::from_records(records.iter().map(|(k, v)| (k.as_slice(), v.as_slice())))
                    .expect("seed records");
            let leaves = forest
                .pack_leaves(TreeKind::Cleanup, records, false)
                .expect("seed leaves");
            root = forest
                .branch(TreeKind::Cleanup, leaves)
                .expect("seed branch");
        }
        for action in batch.as_chunks::<2>().0 {
            let mut key = vec![3];
            key.extend_from_slice(&(u64::from(action[1]) + 1).to_be_bytes());
            if action[0] & 1 == 0 {
                root = forest
                    .put(root, TreeKind::Cleanup, &key, &[])
                    .expect("insert");
                expected.insert(key, vec![]);
            } else {
                root = forest
                    .remove(root, TreeKind::Cleanup, &key)
                    .expect("remove");
                expected.remove(&key);
            }
        }
        let mut actual = BTreeMap::new();
        let mut cursor = Vec::new();
        loop {
            let records = forest
                .scan(&root, TreeKind::Cleanup, &cursor, 64)
                .expect("scan");
            if records.is_empty() {
                break;
            }
            cursor = records.last().expect("nonempty page").0.clone();
            cursor.push(0);
            actual.extend(records);
        }
        assert_eq!(actual, expected);
        let cut = [3, 0, 0, 0, 0, 0, 0, 0, 128];
        let (left, right) = forest.split(root, TreeKind::Cleanup, &cut).expect("split");
        let root = forest
            .join(left, right, TreeKind::Cleanup)
            .expect("join")
            .expect("root");
        let ids = forest
            .draft_ids(std::slice::from_ref(&root))
            .expect("ownership");
        let assignments = ids
            .iter()
            .map(|id| {
                next_block += 1;
                (*id, next_block)
            })
            .collect();
        let reference = forest.resolve(&root, &assignments).expect("assigned root");
        let (writes, _) = forest.encode(&assignments).expect("encoded edits");
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.cleanup = reference;
        base.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MountMode;
    use crate::format::encode_leaf;
    use crate::volume::model::{FaultDevice, GEOMETRY, UUID};

    fn initial() -> (FaultDevice, Root) {
        let reference = |block| TreeRef {
            block,
            generation: 1,
        };
        let root = Root {
            label: None,
            uuid: UUID,
            generation: 1,
            blocks: GEOMETRY.blocks(),
            catalog: reference(512),
            allocation: reference(513),
            cleanup: reference(514),
            next_object: 2,
            free_blocks: GEOMETRY.blocks() - 1027,
            next_job: 1,
            workspace: TreeRef {
                block: 32,
                generation: 1,
            },
            workspace_cursor: 0,
        };
        let leaf = encode_leaf(root.cleanup, &UUID, TreeKind::Cleanup, &[], root.blocks).unwrap();
        (
            FaultDevice::new(BTreeMap::from([(514, leaf)]), MountMode::ReadWrite),
            root,
        )
    }

    fn key(id: u64) -> Vec<u8> {
        let mut key = vec![3];
        key.extend_from_slice(&id.to_be_bytes());
        key
    }

    /// A leaf of one record.
    fn one(key: &[u8], value: &[u8]) -> Leaf {
        Leaf::from_records([(key, value)]).unwrap()
    }

    #[test]
    fn leaf_edits_and_compaction_preserve_records() {
        let mut leaf = Leaf::from_records([
            (&b"a"[..], &b"one"[..]),
            (&b"c"[..], &b"three"[..]),
            (&b"d"[..], &b"four"[..]),
        ])
        .unwrap();
        leaf.insert(1, b"b", b"two").unwrap();
        leaf.replace_value(2, b"replacement").unwrap();
        leaf.remove(0);
        assert!(leaf.dead > 0);
        let copy = leaf.clone();
        assert_eq!(copy.dead, 0);
        assert_eq!(copy.bytes.len(), leaf.bytes.len() - leaf.dead);
        assert_eq!(copy.slices(), leaf.slices());
        let right = leaf.split_off(1).unwrap();
        leaf.append(&right).unwrap();
        assert_eq!(leaf.slices(), copy.slices());
        assert_eq!(leaf.record(0), (&b"b"[..], &b"two"[..]));
        assert_eq!(leaf.record(1), (&b"c"[..], &b"replacement"[..]));
        leaf.remove_range(0..leaf.len());
        let empty = leaf.clone();
        assert!(empty.is_empty());
        assert!(empty.bytes.is_empty());
    }

    #[test]
    fn appends_keep_the_left_page_full_and_inserts_split_evenly() {
        // 402 records of 10 bytes: one more than a 4,016-byte payload holds.
        let entries: Vec<u32> = (0..=PAYLOAD as u32 / 10).collect();
        let sizes = |chunks: &[Vec<u32>]| chunks.iter().map(Vec::len).collect::<Vec<_>>();
        assert_eq!(sizes(&pack(entries.clone(), |_| 10, 1, true)), [401, 1]);
        assert_eq!(sizes(&pack(entries.clone(), |_| 10, 1, false)), [201, 201]);
        // A branch keeps its minimum child count in the new page.
        assert_eq!(sizes(&pack(entries, |_| 10, 2, true)), [400, 2]);
    }

    #[test]
    fn a_full_cache_drops_leaves_and_keeps_branches() {
        let node = |level: u8| Node {
            kind: TreeKind::Catalog,
            entries: if level == 0 {
                Entries::Leaf(Leaf::default())
            } else {
                Entries::Branch(Vec::new())
            },
            edge: Edge {
                link: Link::Draft(0),
                minimum: Arc::from(&b"k"[..]),
                level,
                data_blocks: 0,
            },
        };
        let at = |block: u64| TreeRef {
            block,
            generation: 1,
        };
        let mut cache = Cache::default();
        cache.insert(at(1), node(1));
        for block in 2..=CACHE_PAGES as u64 + 1 {
            cache.insert(at(block), node(0));
        }
        // The overflow dropped every leaf but kept the branch.
        assert!(cache.nodes.contains_key(&(1, 1, TreeKind::Catalog)));
        assert_eq!(cache.nodes.len(), 2);
        // Branches alone past half the bound still clear the whole cache.
        let mut cache = Cache::default();
        for block in 1..=CACHE_PAGES as u64 {
            cache.insert(at(block), node(1));
        }
        cache.insert(at(0), node(0));
        assert_eq!(cache.nodes.len(), 1);
    }

    #[test]
    fn discarded_drafts_still_count_toward_transaction_work() {
        let (mut device, base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        for _ in 0..MAX_DRAFTS {
            forest
                .draft(
                    TreeKind::Cleanup,
                    Entries::Leaf(Leaf::default()),
                    Arc::from(&[][..]),
                    0,
                    0,
                )
                .unwrap();
        }
        assert!(matches!(
            forest.leaf(TreeKind::Cleanup, Leaf::default()),
            Err(Error::LimitExceeded)
        ));
        assert!(device.media.borrow().events.is_empty());
    }

    fn materialize(
        forest: &Forest<'_, FaultDevice>,
        edge: &Edge,
        first: u64,
    ) -> (TreeRef, BTreeMap<u64, Page>) {
        let ids = forest.draft_ids(std::slice::from_ref(edge)).unwrap();
        let assigned = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, first + index as u64))
            .collect();
        (
            forest.resolve(edge, &assigned).unwrap(),
            forest.encode(&assigned).unwrap().0.into_iter().collect(),
        )
    }

    // Counts live pages per level reachable from `edge`.
    fn pages_per_level(forest: &mut Forest<'_, FaultDevice>, edge: &Edge) -> Vec<usize> {
        let mut counts = vec![0; usize::from(edge.level) + 1];
        let mut pending = vec![edge.clone()];
        while let Some(edge) = pending.pop() {
            counts[usize::from(edge.level)] += 1;
            if let Entries::Branch(children) =
                forest.load(&edge, TreeKind::Cleanup).unwrap().entries
            {
                pending.extend(children);
            }
        }
        counts
    }

    /// Removing every record of some leaves, one at a time and as runs,
    /// drops those leaves from their parents and leaves a tree that reads
    /// back, page by page, with exactly the remaining keys.
    #[test]
    fn emptied_leaves_leave_their_parents() {
        let (mut device, mut base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edge = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
        for id in 1..=900u64 {
            edge = forest.put(edge, TreeKind::Cleanup, &key(id), &[]).unwrap();
        }
        for id in 1..=300u64 {
            edge = forest.remove(edge, TreeKind::Cleanup, &key(id)).unwrap();
        }
        let mut id = 300u64;
        while id < 600 {
            if let Some(next) = forest
                .remove_run(&edge, TreeKind::Cleanup, &key(id + 1), &key(id + 51))
                .unwrap()
            {
                edge = next;
            } else {
                for gone in id + 1..id + 51 {
                    edge = forest.remove(edge, TreeKind::Cleanup, &key(gone)).unwrap();
                }
            }
            id += 50;
        }
        let (reference, writes) = materialize(&forest, &edge, 1024);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.cleanup = reference;
        base.generation += 1;
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let edge = forest.root(reference, TreeKind::Cleanup).unwrap();
        let mut seen = Vec::new();
        let mut start = Vec::new();
        loop {
            let page = forest.scan(&edge, TreeKind::Cleanup, &start, 64).unwrap();
            let Some((last, _)) = page.last() else { break };
            start = last.clone();
            start.push(0);
            seen.extend(page.into_iter().map(|(key, _)| key));
        }
        assert_eq!(seen, (601..=900u64).map(key).collect::<Vec<_>>());
    }

    /// `remove_run` drops a range inside one leaf with one copy, and declines,
    /// changing nothing, a range that spans leaves or would empty a leaf.
    #[test]
    fn a_run_inside_one_leaf_is_removed_in_one_copy() {
        let (mut device, base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edge = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
        for id in 1..=1000u64 {
            edge = forest.put(edge, TreeKind::Cleanup, &key(id), &[]).unwrap();
        }
        assert!(edge.level > 0);
        let drafts = forest.drafts();
        let removed = forest
            .remove_run(&edge, TreeKind::Cleanup, &key(10), &key(13))
            .unwrap()
            .expect("a run inside one leaf");
        assert_eq!(forest.drafts() - drafts, usize::from(edge.level) + 1);
        for id in 1..=1000u64 {
            let found = forest
                .lookup(&removed, TreeKind::Cleanup, &key(id))
                .unwrap();
            assert_eq!(found.is_some(), !(10..13).contains(&id), "key {id}");
        }
        // Spanning every leaf, and a whole one-leaf tree, both decline.
        assert!(
            forest
                .remove_run(&removed, TreeKind::Cleanup, &key(1), &key(1001))
                .unwrap()
                .is_none()
        );
        let single = forest.leaf(TreeKind::Cleanup, one(&key(5), &[])).unwrap();
        assert!(
            forest
                .remove_run(&single, TreeKind::Cleanup, &key(1), &key(9))
                .unwrap()
                .is_none()
        );
    }

    /// A bounded scan stops before `end`, across leaves and branches.
    #[test]
    fn a_bounded_scan_stops_before_its_end() {
        let (mut device, base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edge = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
        for id in 1..=1000u64 {
            edge = forest.put(edge, TreeKind::Cleanup, &key(id), &[]).unwrap();
        }
        assert!(edge.level > 0);
        for (start, end, limit) in [
            (10, 20, 64),
            (300, 700, 64),
            (5, 6, 64),
            (990, 2000, 64),
            (1, 60, 16),
        ] {
            let keys: Vec<_> = forest
                .scan_before(
                    &edge,
                    TreeKind::Cleanup,
                    &key(start),
                    Some(&key(end)),
                    limit,
                )
                .unwrap()
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            let expected: Vec<_> = (start..end.min(1001)).take(limit).map(key).collect();
            assert_eq!(keys, expected, "{start}..{end}");
        }
    }

    /// A scan reads only as many children as it wants records. A start between
    /// two keys, including the last key of one leaf and the first of the next,
    /// still returns the records that follow it.
    #[test]
    fn a_short_scan_from_a_gap_returns_the_following_records() {
        let (mut device, base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edge = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
        for id in 1..=1000u64 {
            edge = forest
                .put(edge, TreeKind::Cleanup, &key(2 * id), &[])
                .unwrap();
        }
        assert!(edge.level > 0);
        for gap in (1..=2001u64).step_by(2) {
            for limit in 1..=3 {
                let keys: Vec<_> = forest
                    .scan(&edge, TreeKind::Cleanup, &key(gap), limit)
                    .unwrap()
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect();
                let expected: Vec<_> = (gap + 1..=2000).step_by(2).take(limit).map(key).collect();
                assert_eq!(keys, expected, "from {gap}, limit {limit}");
            }
        }
    }

    /// Compaction keeps exactly the drafts the roots reach, renumbered, so
    /// every lookup answers as before and the ownership walk still passes.
    #[test]
    fn compaction_drops_superseded_drafts_and_keeps_the_trees() {
        let (mut device, base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut first = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
        let mut second = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
        for id in 1..=600u64 {
            first = forest.put(first, TreeKind::Cleanup, &key(id), &[]).unwrap();
            if id % 3 == 0 {
                second = forest
                    .put(second, TreeKind::Cleanup, &key(id), &[])
                    .unwrap();
            }
        }
        let before = forest.drafts();
        let reachable = forest
            .draft_ids(&[first.clone(), second.clone()])
            .unwrap()
            .len();
        assert!(
            reachable * 4 < before,
            "{reachable} of {before} drafts live"
        );
        forest.compact(&mut [&mut first, &mut second], 0);
        assert_eq!(forest.drafts(), reachable);
        assert_eq!(
            forest
                .draft_ids(&[first.clone(), second.clone()])
                .unwrap()
                .len(),
            reachable
        );
        for id in 1..=600u64 {
            assert_eq!(
                forest.lookup(&first, TreeKind::Cleanup, &key(id)).unwrap(),
                Some(Vec::new())
            );
            let expected = (id % 3 == 0).then(Vec::new);
            assert_eq!(
                forest.lookup(&second, TreeKind::Cleanup, &key(id)).unwrap(),
                expected
            );
        }
        // Too few drafts since the last run: nothing moves.
        let drafts = forest.drafts();
        forest.compact(&mut [&mut first, &mut second], 0);
        assert_eq!(forest.drafts(), drafts);
    }

    #[test]
    fn front_insertion_into_full_pages_splits_once() {
        // 13-byte records: 308 fill a 4,016-byte leaf, and 138 fill a branch.
        let full_leaf = |first: u64| {
            let keys: Vec<_> = (first..first + 308).map(key).collect();
            Leaf::from_records(keys.iter().map(|key| (key.as_slice(), &[][..]))).unwrap()
        };
        for (leaves, inserts) in [(1u64, 32u64), (138, 616)] {
            let (mut device, mut base) = initial();
            let mut forest = Forest::new(&mut device, &base).unwrap();
            let children = (0..leaves)
                .map(|leaf| {
                    let edge = forest
                        .leaf(TreeKind::Cleanup, full_leaf(10_000 + leaf * 1000))
                        .unwrap();
                    assert_eq!(pages_per_level(&mut forest, &edge), [1]);
                    edge
                })
                .collect::<Vec<_>>();
            let root = if leaves == 1 {
                children[0].clone()
            } else {
                forest.branch(TreeKind::Cleanup, children).unwrap()
            };
            let (reference, writes) = materialize(&forest, &root, 1024);
            drop(forest);
            device.media.borrow_mut().durable.extend(writes);
            base.cleanup = reference;
            base.generation += 1;

            let mut forest = Forest::new(&mut device, &base).unwrap();
            let mut edge = forest.root(reference, TreeKind::Cleanup).unwrap();
            for id in (1..=inserts).rev() {
                edge = forest.put(edge, TreeKind::Cleanup, &key(id), &[]).unwrap();
            }
            let counts = pages_per_level(&mut forest, &edge);
            eprintln!("leaves={leaves} inserts={inserts} pages per level={counts:?}");
            let records = forest.scan(&edge, TreeKind::Cleanup, &[], 64).unwrap();
            assert_eq!(records.first().unwrap().0, key(1));
            assert!(records.windows(2).all(|pair| pair[0].0 < pair[1].0));
            // Balanced splits add the minimum pages, not one per insertion.
            // Unbalanced packing measured [2, 1] as 33 leaves, and the
            // branch case as three branches where two fit.
            let expected: &[usize] = if leaves == 1 { &[2, 1] } else { &[142, 2, 1] };
            assert_eq!(counts, expected);
            let first = 1024 + 4096;
            let (reference, writes) = materialize(&forest, &edge, first);
            drop(forest);
            device.media.borrow_mut().durable.extend(writes);
            base.cleanup = reference;
            base.generation += 1;
            let mut forest = Forest::new(&mut device, &base).unwrap();
            let reopened = forest.root(reference, TreeKind::Cleanup).unwrap();
            assert_eq!(pages_per_level(&mut forest, &reopened), counts);
        }
    }

    #[test]
    fn ordered_mutations_split_join_and_reopen_match_map() {
        let (mut device, mut base) = initial();
        let mut expected = BTreeMap::new();
        let mut first = 1024;
        let mut random = 42u64;
        for batch in 0..24 {
            let mut forest = Forest::new(&mut device, &base).unwrap();
            let mut edge = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
            for _ in 0..48 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let k = key(1 + (random >> 32) % 1100);
                if batch > 8 && random & 3 == 0 {
                    edge = forest.remove(edge, TreeKind::Cleanup, &k).unwrap();
                    expected.remove(&k);
                } else {
                    edge = forest.put(edge, TreeKind::Cleanup, &k, &[]).unwrap();
                    expected.insert(k, Vec::new());
                }
            }
            let mut actual = BTreeMap::new();
            let mut cursor = Vec::new();
            loop {
                let records = forest.scan(&edge, TreeKind::Cleanup, &cursor, 64).unwrap();
                if records.is_empty() {
                    break;
                }
                cursor = records.last().unwrap().0.clone();
                cursor.push(0);
                actual.extend(records);
            }
            assert_eq!(actual, expected);
            for probe in [0, 1, 2, 549, 550, 551, 1099, 1100, 1200] {
                let mut probe = key(probe);
                for between in [false, true] {
                    if between {
                        probe.push(0);
                    }
                    let floor = forest.floor(&edge, TreeKind::Cleanup, &probe).unwrap();
                    let oracle = expected
                        .range(..=probe.clone())
                        .next_back()
                        .map(|(k, v)| (k.clone(), v.clone()));
                    assert_eq!(floor, oracle, "{probe:?}");
                }
            }
            let cut = key(550);
            let (left, right) = forest.split(edge, TreeKind::Cleanup, &cut).unwrap();
            let roots: Vec<_> = [left.clone(), right.clone()]
                .into_iter()
                .flatten()
                .collect();
            forest.draft_ids(&roots).unwrap();
            let joined = forest
                .join(left, right, TreeKind::Cleanup)
                .unwrap()
                .unwrap();
            for k in expected.keys() {
                assert_eq!(
                    forest.lookup(&joined, TreeKind::Cleanup, k).unwrap(),
                    Some(vec![])
                );
            }
            let (reference, writes) = materialize(&forest, &joined, first);
            first += writes.len() as u64;
            drop(forest);
            device.media.borrow_mut().durable.extend(writes);
            base.cleanup = reference;
            base.generation += 1;
        }
        assert!(expected.len() > 300);
    }

    #[test]
    fn inserting_a_bitmap_bounds_growth_when_the_root_splits() {
        let (mut device, mut base) = initial();
        base.blocks = 287 * 16_384;
        device.geometry.byte_len = base.blocks * 4096;
        let bitmap = |group| {
            let mut value = vec![0; 2048];
            if group == 0 {
                value[..64].fill(255);
            }
            value[1024] = 1;
            value
        };
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let leaves = (0..143u64)
            .map(|id| {
                let group = id * 2;
                forest
                    .leaf(
                        TreeKind::Allocation,
                        one(&group.to_be_bytes(), &bitmap(group)),
                    )
                    .unwrap()
            })
            .collect();
        let root = forest.branch(TreeKind::Allocation, leaves).unwrap();
        assert_eq!(root.level, 1);
        let (reference, writes) = materialize(&forest, &root, 1024);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.allocation = reference;
        base.generation += 1;
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let root = forest.root(reference, TreeKind::Allocation).unwrap();
        let key = 143u64.to_be_bytes();
        let new = forest
            .put(root.clone(), TreeKind::Allocation, &key, &bitmap(143))
            .unwrap();
        assert_eq!(new.level, 2);
        assert_eq!(
            forest.draft_ids(std::slice::from_ref(&new)).unwrap().len(),
            5
        );
        assert_eq!(forest.retired.len(), 2);
        assert_eq!(
            forest.lookup(&root, TreeKind::Allocation, &key).unwrap(),
            None
        );
        assert_eq!(
            forest.lookup(&new, TreeKind::Allocation, &key).unwrap(),
            Some(bitmap(143))
        );
        materialize(&forest, &new, 2048);
        assert!(device.media.borrow().events.is_empty());
    }

    #[test]
    fn fixed_width_replacement_updates_totals_and_validates_the_record() {
        let (mut device, mut base) = initial();
        let mut key = 2u64.to_be_bytes().to_vec();
        key.push(3);
        key.extend_from_slice(&0u64.to_be_bytes());
        let mut value = 8192u64.to_le_bytes().to_vec();
        value.extend_from_slice(&1u32.to_le_bytes());
        value.extend_from_slice(&[0; 4]);
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let edge = forest.leaf(TreeKind::Catalog, one(&key, &value)).unwrap();
        let (reference, writes) = materialize(&forest, &edge, 1024);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.catalog = reference;
        base.generation += 1;
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let edge = forest.root(reference, TreeKind::Catalog).unwrap();
        for (key, value) in [(vec![0], value.clone()), (key.clone(), vec![])] {
            assert!(
                forest
                    .put(edge.clone(), TreeKind::Catalog, &key, &value)
                    .is_err()
            );
            assert_eq!(forest.drafts, 0);
            assert!(forest.retired.is_empty());
        }
        value[8..12].copy_from_slice(&2u32.to_le_bytes());
        let edge = forest.put(edge, TreeKind::Catalog, &key, &value).unwrap();
        assert_eq!(edge.data_blocks, 2);
        assert_eq!(forest.drafts, 1);
        assert_eq!(forest.retired, BTreeSet::from([reference.block]));
        let (reference, writes) = materialize(&forest, &edge, 2048);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.catalog = reference;
        base.generation += 1;
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let edge = forest.root(reference, TreeKind::Catalog).unwrap();
        assert_eq!(edge.data_blocks, 2);
        assert_eq!(
            forest.lookup(&edge, TreeKind::Catalog, &key).unwrap(),
            Some(value.clone())
        );
        value[8..12].fill(0);
        assert!(matches!(
            forest.put(edge, TreeKind::Catalog, &key, &value),
            Err(Error::InvalidInput(_))
        ));
        assert!(device.media.borrow().events.is_empty());
    }

    #[test]
    fn maximum_height_edits_preserve_levels_and_reject_cycles() {
        let (mut device, mut base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edges: Vec<_> = (1..=128u64)
            .map(|id| forest.leaf(TreeKind::Cleanup, one(&key(id), &[])).unwrap())
            .collect();
        for _ in 0..7 {
            edges = edges
                .chunks(2)
                .map(|children| forest.branch(TreeKind::Cleanup, children.to_vec()).unwrap())
                .collect();
        }
        let root = edges.pop().unwrap();
        assert_eq!(root.level, 7);
        assert!(matches!(
            forest.branch(TreeKind::Cleanup, vec![root.clone(), root.clone()]),
            Err(Error::LimitExceeded)
        ));
        let (reference, writes) = materialize(&forest, &root, 1024);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.cleanup = reference;
        base.generation += 1;
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let root = forest.root(reference, TreeKind::Cleanup).unwrap();
        let root = forest.put(root, TreeKind::Cleanup, &key(64), &[]).unwrap();
        assert_eq!(
            forest.draft_ids(std::slice::from_ref(&root)).unwrap().len(),
            8
        );
        assert_eq!(forest.retired.len(), 8);
        assert_eq!(forest.drafts, 8);
        let root = forest.remove(root, TreeKind::Cleanup, &key(63)).unwrap();
        let root = forest.put(root, TreeKind::Cleanup, &key(129), &[]).unwrap();
        let (left, right) = forest.split(root, TreeKind::Cleanup, &key(64)).unwrap();
        let root = forest
            .join(left, right, TreeKind::Cleanup)
            .unwrap()
            .unwrap();
        for id in 1..=129 {
            assert_eq!(
                forest
                    .lookup(&root, TreeKind::Cleanup, &key(id))
                    .unwrap()
                    .is_some(),
                id != 63
            );
        }
        forest.draft_ids(&[root]).unwrap();
        drop(forest);
        let mut media = device.media.borrow_mut();
        let page = media.durable.get_mut(&reference.block).unwrap();
        let value_offset = 80 + 4 + 9;
        page.0[value_offset..value_offset + 8].copy_from_slice(&reference.block.to_le_bytes());
        page.0[value_offset + 8..value_offset + 16]
            .copy_from_slice(&reference.generation.to_le_bytes());
        page.0[64..68].fill(0);
        let crc = crate::format::crc32c(&page.0[..]);
        page.0[64..68].copy_from_slice(&crc.to_le_bytes());
        drop(media);
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let root = forest.root(reference, TreeKind::Cleanup).unwrap();
        assert!(matches!(
            forest.lookup(&root, TreeKind::Cleanup, &key(1)),
            Err(Error::Corrupt(_))
        ));
    }

    /// Small enough to interpret under Miri, and still covers both removal
    /// paths: a leaf that keeps records, and the last record of a leaf.
    #[test]
    fn removals_from_a_prebuilt_tree_take_both_paths() {
        let (mut device, base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let leaves: Vec<_> = (0..4u64)
            .map(|leaf| {
                let keys: Vec<_> = (0..3u64).map(|slot| key(1 + leaf * 3 + slot)).collect();
                let records =
                    Leaf::from_records(keys.iter().map(|key| (key.as_slice(), &[][..]))).unwrap();
                forest.leaf(TreeKind::Cleanup, records).unwrap()
            })
            .collect();
        let pairs: Vec<_> = leaves
            .chunks(2)
            .map(|children| forest.branch(TreeKind::Cleanup, children.to_vec()).unwrap())
            .collect();
        let mut edge = forest.branch(TreeKind::Cleanup, pairs).unwrap();
        assert_eq!(edge.level, 2);
        for id in 1..=12u64 {
            edge = forest.remove(edge, TreeKind::Cleanup, &key(id)).unwrap();
            assert_eq!(
                forest.lookup(&edge, TreeKind::Cleanup, &key(id)).unwrap(),
                None
            );
            for later in id + 1..=12 {
                assert_eq!(
                    forest
                        .lookup(&edge, TreeKind::Cleanup, &key(later))
                        .unwrap(),
                    Some(vec![]),
                    "removing {id} lost {later}"
                );
            }
        }
        assert_eq!(edge.level, 0);
        assert!(
            forest
                .scan(&edge, TreeKind::Cleanup, &[], 8)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn removing_every_record_keeps_levels_even_and_ends_empty() {
        let (mut device, base) = initial();
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edge = forest.root(base.cleanup, TreeKind::Cleanup).unwrap();
        // Enough records for several leaves under a branch, so removals take
        // the copied-path case and, on a leaf's last record, the interval case.
        let ids: Vec<u64> = (1..=900).collect();
        for id in &ids {
            edge = forest.put(edge, TreeKind::Cleanup, &key(*id), &[]).unwrap();
        }
        assert!(edge.level > 0, "expected a branching tree");
        // Removing from the middle outwards empties interior leaves first.
        let order: Vec<u64> = ids
            .iter()
            .rev()
            .step_by(2)
            .chain(ids.iter().step_by(2))
            .copied()
            .collect();
        for (done, id) in order.iter().enumerate() {
            edge = forest.remove(edge, TreeKind::Cleanup, &key(*id)).unwrap();
            assert_eq!(
                forest.lookup(&edge, TreeKind::Cleanup, &key(*id)).unwrap(),
                None
            );
            if done % 97 == 0 {
                for other in order[done + 1..].iter() {
                    assert_eq!(
                        forest
                            .lookup(&edge, TreeKind::Cleanup, &key(*other))
                            .unwrap(),
                        Some(vec![]),
                        "removing {id} lost {other}"
                    );
                }
            }
        }
        assert_eq!(edge.level, 0);
        assert!(
            forest
                .scan(&edge, TreeKind::Cleanup, &[], 64)
                .unwrap()
                .is_empty()
        );
        // A removal that misses replaces nothing.
        let same = forest
            .remove(edge.clone(), TreeKind::Cleanup, &key(7))
            .unwrap();
        assert_eq!(same.level, edge.level);
    }

    /// Lookups that start at the last leaf answer as a descent does, for
    /// present and absent keys in every leaf, and a later root is searched
    /// from its own top.
    #[test]
    fn a_lookup_from_the_last_leaf_matches_a_descent() {
        let (mut device, mut base) = initial();
        let extent = |logical: u64| {
            let mut key = 2u64.to_be_bytes().to_vec();
            key.push(3);
            key.extend_from_slice(&logical.to_be_bytes());
            let mut value = (8192 + logical).to_le_bytes().to_vec();
            value.extend_from_slice(&1u32.to_le_bytes());
            value.extend_from_slice(&[0; 4]);
            (key, value)
        };
        // Even extents only, in enough leaves for two levels of branches.
        let mut records = Leaf::default();
        for logical in (0..4800u64).step_by(2) {
            let (key, value) = extent(logical);
            records.push(&key, &value).unwrap();
        }
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edges = forest
            .pack_leaves(TreeKind::Catalog, records, false)
            .unwrap();
        while edges.len() > 1 {
            edges = forest
                .pack_branches(TreeKind::Catalog, edges, false)
                .unwrap();
        }
        let (reference, writes) = materialize(&forest, &edges[0], 1024);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.catalog = reference;
        base.generation += 1;

        let mut cache = Cache::default();
        // Ascending, then scattered, so lookups both stay in a leaf and leave it.
        let order = (0..4800u64).chain((0..4800).map(|step| step * 2_477 % 4800));
        for logical in order {
            let (key, value) = extent(logical);
            let mut cached = Forest::cached(&mut device, &base, Some(&mut cache)).unwrap();
            let root = cached.root(base.catalog, TreeKind::Catalog).unwrap();
            let found = cached.lookup(&root, TreeKind::Catalog, &key).unwrap();
            drop(cached);
            let mut plain = Forest::new(&mut device, &base).unwrap();
            let root = plain.root(base.catalog, TreeKind::Catalog).unwrap();
            assert_eq!(found, plain.lookup(&root, TreeKind::Catalog, &key).unwrap());
            assert_eq!(
                found,
                (logical % 2 == 0).then_some(value),
                "extent {logical}"
            );
        }
        assert!(cache.finger.is_some());

        // Publish an odd extent next to the last key looked up. The finger
        // still names a leaf of the old root, which does not hold it.
        let last = cache.finger.unwrap().leaf;
        let Entries::Leaf(leaf) =
            &cache.nodes[&(last.block, last.generation, TreeKind::Catalog)].entries
        else {
            panic!("the finger names a leaf");
        };
        let neighbour = u64_be(&leaf.key(0)[9..]).unwrap() + 1;
        let (key, value) = extent(neighbour);
        let mut forest = Forest::cached(&mut device, &base, Some(&mut cache)).unwrap();
        let root = forest.root(base.catalog, TreeKind::Catalog).unwrap();
        assert_eq!(forest.lookup(&root, TreeKind::Catalog, &key).unwrap(), None);
        let edge = forest.put(root, TreeKind::Catalog, &key, &value).unwrap();
        let (reference, writes) = materialize(&forest, &edge, 4096);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.catalog = reference;
        base.generation += 1;
        let mut forest = Forest::cached(&mut device, &base, Some(&mut cache)).unwrap();
        let root = forest.root(base.catalog, TreeKind::Catalog).unwrap();
        assert_eq!(
            forest.lookup(&root, TreeKind::Catalog, &key).unwrap(),
            Some(value)
        );
    }

    #[test]
    fn detach_copies_boundaries_and_keeps_disjoint_ownership() {
        let (mut device, mut base) = initial();
        let mut records = Leaf::default();
        for logical in 0..2400u64 {
            let mut key = 2u64.to_be_bytes().to_vec();
            key.push(3);
            key.extend_from_slice(&logical.to_be_bytes());
            let mut value = (8192 + logical).to_le_bytes().to_vec();
            value.extend_from_slice(&1u32.to_le_bytes());
            value.extend_from_slice(&[0; 4]);
            records.push(&key, &value).unwrap();
        }
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let mut edges = forest
            .pack_leaves(TreeKind::Catalog, records, false)
            .unwrap();
        while edges.len() > 1 {
            edges = forest
                .pack_branches(TreeKind::Catalog, edges, false)
                .unwrap();
        }
        let edge = edges.pop().unwrap();
        let (reference, writes) = materialize(&forest, &edge, 1024);
        drop(forest);
        device.media.borrow_mut().durable.extend(writes);
        base.catalog = reference;
        base.generation += 1;
        let mut forest = Forest::new(&mut device, &base).unwrap();
        let edge = forest.root(base.catalog, TreeKind::Catalog).unwrap();
        let bound = |logical: u64| {
            let mut key = 2u64.to_be_bytes().to_vec();
            key.push(3);
            key.extend_from_slice(&logical.to_be_bytes());
            key
        };
        let (kept, detached) = forest.detach(edge, &bound(123), &bound(2123)).unwrap();
        let detached = detached.unwrap();
        assert_eq!(detached.data_blocks, 2000);
        assert_eq!(kept.data_blocks, 400);
        let ids = forest.draft_ids(&[kept.clone(), detached.clone()]).unwrap();
        assert!(ids.len() < 16, "{} replacement boundary pages", ids.len());
        assert!(forest.retired.len() < 16);
        assert!(
            forest
                .lookup(&kept, TreeKind::Catalog, &bound(2122))
                .unwrap()
                .is_none()
        );
        assert!(
            forest
                .lookup(&detached, TreeKind::Catalog, &bound(2122))
                .unwrap()
                .is_some()
        );
    }
}
