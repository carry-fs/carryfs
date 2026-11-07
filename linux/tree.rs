// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Verified metadata pages and B+tree searches.
//!
//! Pages come from the block device's buffer cache. A page is checksummed and
//! its records validated once, the first time it is read into the cache; a
//! buffer-head flag remembers that, and later uses recheck only its identity.
//! On a writable mount, searches also see the open transaction's draft pages.

use core::cmp::Ordering;

use kernel::bindings::super_block;
use kernel::prelude::*;

use crate::corrupt;
use crate::glue::{self, BufferHead};
use crate::ondisk::format::{self, PageView, Records, TreeKind, TreeRef, BLOCK_BYTES};
use crate::volume::State;

/// Catalog record kinds (FORMAT.md section 5).
pub(crate) const INODE: u8 = 1;
pub(crate) const ENTRY: u8 = 2;
pub(crate) const EXTENT: u8 = 3;
pub(crate) const CHECKSUMS: u8 = 4;

/// The catalog key prefix of `owner`'s records of `kind`.
pub(crate) fn prefix(owner: u64, kind: u8) -> [u8; 9] {
    let mut key = [0; 9];
    key[..8].copy_from_slice(&owner.to_be_bytes());
    key[8] = kind;
    key
}

/// The catalog key of an extent or checksum-group record.
pub(crate) fn numbered_key(owner: u64, kind: u8, number: u64) -> [u8; 17] {
    let mut key = [0; 17];
    key[..9].copy_from_slice(&prefix(owner, kind));
    key[9..].copy_from_slice(&number.to_be_bytes());
    key
}

/// Little-endian `u64` at `offset` of a validated record value; zero when the
/// value is too short, which validation already excluded.
pub(crate) fn le_u64(value: &[u8], offset: usize) -> u64 {
    value
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

/// Big-endian `u64` at `offset` of a key; zero when it is too short.
pub(crate) fn be_u64(key: &[u8], offset: usize) -> u64 {
    key.get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u64::from_be_bytes)
}

/// Append `bytes` to `vec` in one copy: the kernel's `extend_from_slice`
/// clones element by element.
pub(crate) fn append(vec: &mut KVec<u8>, bytes: &[u8]) -> Result {
    vec.reserve(bytes.len(), GFP_KERNEL)?;
    let spare = vec.spare_capacity_mut();
    // SAFETY: `reserve` made room for `bytes.len()` more bytes, and
    // `MaybeUninit<u8>` has the layout of `u8`; the copy initializes them.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            spare.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        );
        vec.inc_len(bytes.len());
    }
    Ok(())
}

/// Key order. Catalog keys start with a big-endian object ID, and most keys
/// on a page differ there, so comparing it as an integer first spares most
/// comparisons a `memcmp` call.
pub(crate) fn compare(a: &[u8], b: &[u8]) -> Ordering {
    match (a.split_first_chunk::<8>(), b.split_first_chunk::<8>()) {
        (Some((x, a)), Some((y, b))) => u64::from_be_bytes(*x)
            .cmp(&u64::from_be_bytes(*y))
            .then_with(|| a.cmp(b)),
        _ => a.cmp(b),
    }
}

/// Leaves read ahead when a search reaches one that is not cached: 64 KiB,
/// as ext4 reads ahead inode-table blocks. Listings and stats in name order
/// walk neighboring leaves.
const LEAF_READAHEAD: usize = 16;

/// Records a page search bisects; catalog pages hold at most 148. Pages of
/// more, such as cleanup leaves of orphan markers, are searched linearly.
pub(crate) const SEARCH_RECORDS: usize = 160;

/// A child reference to a draft sets this bit in its block number, which no
/// real block reaches, and generation zero.
const DRAFT: u64 = 1 << 63;

/// Where a page lives: on disk, or among the open transaction's drafts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Disk(TreeRef),
    Draft(usize),
}

impl Link {
    /// The 16-byte internal-record value that refers to this page.
    pub(crate) fn value(self) -> [u8; 16] {
        let (block, generation) = match self {
            Link::Disk(reference) => (reference.block, reference.generation),
            Link::Draft(id) => (DRAFT | id as u64, 0),
        };
        let mut value = [0; 16];
        value[..8].copy_from_slice(&block.to_le_bytes());
        value[8..].copy_from_slice(&generation.to_le_bytes());
        value
    }

    /// The page an internal record's value refers to.
    pub(crate) fn of(value: &[u8]) -> Self {
        let block = le_u64(value, 0);
        if block & DRAFT != 0 {
            Link::Draft((block & !DRAFT) as usize)
        } else {
            Link::Disk(TreeRef {
                block,
                generation: le_u64(value, 8),
            })
        }
    }
}

/// A page the open transaction wrote or copied, unsealed: its header holds
/// only the level, record count, and payload length.
pub(crate) struct Draft {
    pub(crate) page: KBox<[u8; BLOCK_BYTES]>,
    /// False once the page left its tree, after a merge or root collapse.
    pub(crate) live: bool,
}

/// A metadata block held in the buffer cache.
pub(crate) struct Block(*mut BufferHead);

impl Block {
    pub(crate) fn read(sb: *mut super_block, block: u64) -> Result<Self> {
        // SAFETY: `sb` is a mounted super block whose block size is
        // `BLOCK_BYTES`; out-of-range blocks fail the read.
        let bh = unsafe { glue::carryfs_bread(sb, block) };
        if bh.is_null() {
            pr_err!("cannot read block {block}\n");
            return Err(EIO);
        }
        Ok(Self(bh))
    }

    /// The block if the buffer cache already holds it uptodate.
    fn cached(sb: *mut super_block, block: u64) -> Option<Self> {
        // SAFETY: `sb` is a mounted super block.
        let bh = unsafe { glue::carryfs_bread_cached(sb, block) };
        (!bh.is_null()).then_some(Self(bh))
    }

    pub(crate) fn data(&self) -> &[u8] {
        // SAFETY: `sb_bread` returned an uptodate buffer of `BLOCK_BYTES`
        // bytes, and the reference this `Block` holds keeps it alive.
        unsafe { core::slice::from_raw_parts(glue::carryfs_bh_data(self.0), BLOCK_BYTES) }
    }

    fn verified(&self) -> bool {
        // SAFETY: `self.0` is a referenced buffer head.
        unsafe { glue::carryfs_bh_verified(self.0) }
    }

    fn set_verified(&self) {
        // SAFETY: `self.0` is a referenced buffer head.
        unsafe { glue::carryfs_bh_set_verified(self.0) }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: `read` or `cached` took this reference.
        unsafe { glue::carryfs_brelse(self.0) }
    }
}

/// A loaded page: a cached disk block or a draft.
pub(crate) enum Page<'a> {
    Disk(Block),
    Draft(&'a [u8; BLOCK_BYTES]),
}

impl Page<'_> {
    pub(crate) fn data(&self) -> &[u8] {
        match self {
            Page::Disk(block) => block.data(),
            Page::Draft(page) => &page[..],
        }
    }
}

/// A page's place in its tree, as its parent records it.
#[derive(Clone, Copy)]
pub(crate) struct Position {
    pub(crate) link: Link,
    pub(crate) parent_generation: u64,
    pub(crate) level: Option<u8>,
}

/// One tree of the current state: the committed root, or the open
/// transaction's root and drafts.
pub(crate) struct Tree<'a> {
    pub(crate) sb: *mut super_block,
    pub(crate) uuid: [u8; 16],
    pub(crate) blocks: u64,
    pub(crate) kind: TreeKind,
    pub(crate) root: Link,
    pub(crate) generation: u64,
    pub(crate) drafts: &'a [Draft],
}

impl<'a> Tree<'a> {
    pub(crate) fn new(sb: *mut super_block, state: &'a State, kind: TreeKind) -> Self {
        let (root, drafts) = match &state.txn {
            Some(txn) => (txn.root(kind), &txn.drafts[..]),
            None => (Link::Disk(state.tree_root(kind)), &[][..]),
        };
        Self {
            sb,
            uuid: state.root.uuid,
            blocks: state.root.blocks,
            kind,
            root,
            generation: state.root.generation,
            drafts,
        }
    }

    pub(crate) fn catalog(sb: *mut super_block, state: &'a State) -> Self {
        Self::new(sb, state, TreeKind::Catalog)
    }

    pub(crate) fn root_position(&self) -> Position {
        Position {
            link: self.root,
            parent_generation: self.generation,
            level: None,
        }
    }

    /// Read the page at `at`, verifying a disk page unless the buffer cache
    /// holds it verified. `siblings` are the parent's records from this page
    /// on; when this is an uncached leaf, the leaves after it are read ahead.
    pub(crate) fn load(&self, at: Position, siblings: Option<Records<'_>>) -> Result<Page<'a>> {
        let reference = match at.link {
            Link::Draft(id) => {
                return self
                    .drafts
                    .get(id)
                    .map(|draft| Page::Draft(&*draft.page))
                    .ok_or(EIO);
            }
            Link::Disk(reference) => reference,
        };
        let block = match Block::cached(self.sb, reference.block) {
            Some(block) => block,
            None => {
                if let Some(siblings) = siblings.filter(|_| at.level == Some(0)) {
                    self.read_ahead(siblings);
                }
                Block::read(self.sb, reference.block)?
            }
        };
        if !block.verified() {
            format::decode_page(
                block.data(),
                reference,
                &self.uuid,
                self.kind,
                at.parent_generation,
                at.level,
                self.blocks,
            )
            .map_err(|error| corrupt(reference.block, error))?;
            block.set_verified();
        }
        Ok(Page::Disk(block))
    }

    pub(crate) fn view<'b>(&self, page: &'b Page<'_>, at: Position) -> Result<PageView<'b>> {
        match at.link {
            Link::Draft(_) => {
                format::draft_page_view(page.data()).map_err(|error| corrupt(0, error))
            }
            Link::Disk(reference) => format::verified_page_view(
                page.data(),
                reference,
                &self.uuid,
                self.kind,
                at.parent_generation,
                at.level,
            )
            .map_err(|error| corrupt(reference.block, error)),
        }
    }

    /// The child an internal record of the page at `at` and `level` refers to.
    pub(crate) fn child(&self, at: Position, level: u8, value: &[u8]) -> Position {
        Position {
            link: Link::of(value),
            parent_generation: match at.link {
                Link::Disk(reference) => reference.generation,
                // A draft's disk children are no newer than the committed root.
                Link::Draft(_) => self.generation,
            },
            level: Some(level - 1),
        }
    }

    /// Start reading the leaves the first `LEAF_READAHEAD` of `siblings`
    /// refer to, in one batch; cached ones and drafts are skipped.
    fn read_ahead(&self, siblings: Records<'_>) {
        let mut blocks = [0u64; LEAF_READAHEAD];
        let mut count = 0;
        for (_, value) in siblings.take(LEAF_READAHEAD) {
            if let Link::Disk(reference) = Link::of(value) {
                blocks[count] = reference.block;
                count += 1;
            }
        }
        // SAFETY: `sb` is mounted; `blocks` holds `count` block numbers.
        unsafe { glue::carryfs_readahead_blocks(self.sb, blocks.as_ptr(), count as u32) };
    }

    /// Check the tree's root page, as opening a volume does.
    pub(crate) fn check_root(&self) -> Result {
        self.load(self.root_position(), None).map(drop)
    }

    /// Calls `f` with the last record whose key is at most `key`, if any.
    pub(crate) fn floor<T>(
        &self,
        key: &[u8],
        f: impl FnOnce(&[u8], &[u8]) -> Result<T>,
    ) -> Result<Option<T>> {
        let mut offsets = [0; SEARCH_RECORDS];
        let mut at = self.root_position();
        let mut page = self.load(at, None)?;
        loop {
            let view = self.view(&page, at)?;
            // Bisect leaves, whose keys often share their owner; scan
            // internal pages, where the owner alone usually decides.
            let scratch: &mut [u16] = if view.level == 0 {
                &mut offsets
            } else {
                &mut []
            };
            // Internal keys are subtree minimums, so the last child whose
            // minimum is at most `key` holds the answer.
            let Some((_, from)) = view.floor_record(key, compare, scratch) else {
                return Ok(None);
            };
            let Some((found, value)) = from.clone().next() else {
                return Ok(None);
            };
            if view.level == 0 {
                return f(found, value).map(Some);
            }
            let child = self.child(at, view.level, value);
            // Hold the parent until the child is in hand: its records name
            // the leaves to read ahead.
            let next = self.load(child, Some(from))?;
            (page, at) = (next, child);
        }
    }

    /// Calls `f` with the value of the record whose key is exactly `key`.
    pub(crate) fn get<T>(
        &self,
        key: &[u8],
        f: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<Option<T>> {
        Ok(self
            .floor(key, |found, value| {
                if found == key {
                    f(value).map(Some)
                } else {
                    Ok(None)
                }
            })?
            .flatten())
    }

    /// Calls `f` with each record whose key is at least `start`, in key
    /// order, until `f` returns `false`.
    pub(crate) fn scan(
        &self,
        start: &[u8],
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result {
        self.scan_page(self.root_position(), None, Some(start), f)
            .map(drop)
    }

    /// `scan` below `at`; `false` once `f` stops it. Levels strictly
    /// decrease, so the recursion is at most eight deep.
    fn scan_page(
        &self,
        at: Position,
        siblings: Option<Records<'_>>,
        start: Option<&[u8]>,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<bool> {
        let page = self.load(at, siblings)?;
        let view = self.view(&page, at)?;
        if view.level == 0 {
            for (key, value) in view.records() {
                if start.is_some_and(|start| compare(key, start) == Ordering::Less) {
                    continue;
                }
                if !f(key, value)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        let first = start.map_or(0, |start| {
            view.floor_record(start, compare, &mut [])
                .map_or(0, |(index, _)| index)
        });
        let mut records = view.records();
        for index in 0.. {
            let from = records.clone();
            let Some((_, value)) = records.next() else {
                break;
            };
            if index < first {
                continue;
            }
            let child = self.child(at, view.level, value);
            let start = if index == first { start } else { None };
            if !self.scan_page(child, Some(from), start, f)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
