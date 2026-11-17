// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! The open transaction's copy-on-write B+tree edits.
//!
//! The first edit copies a committed page into a draft; later edits reuse it.
//! Commit assigns blocks and seals drafts. Subtree data-block totals in each
//! draft avoid rereading unchanged children during publication.
//!
//! A failed partial edit faults the volume because its drafts may be inconsistent.

use core::cmp::Ordering;

use kernel::bindings::super_block;
use kernel::prelude::*;

use crate::corrupt;
use crate::ondisk::format::{self, TreeKind, TreeRef, BLOCK_BYTES, PAGE_CAPACITY, PAYLOAD_OFFSET};
use crate::space::Group;
use crate::tree::{compare, le_u64, Block, Draft, Link};

/// Most bytes one record takes on a page, header included.
const MAX_RECORD: usize = 3_072;

fn get_u16(page: &[u8; BLOCK_BYTES], offset: usize) -> usize {
    usize::from(u16::from_le_bytes([page[offset], page[offset + 1]]))
}

fn set_u16(page: &mut [u8; BLOCK_BYTES], offset: usize, value: usize) {
    page[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes());
}

fn count(page: &[u8; BLOCK_BYTES]) -> usize {
    get_u16(page, 32)
}

fn payload(page: &[u8; BLOCK_BYTES]) -> usize {
    get_u16(page, 34)
}

fn level(page: &[u8; BLOCK_BYTES]) -> u8 {
    page[14]
}

/// The subtree data-block total every draft keeps in its header.
pub(crate) fn total(page: &[u8; BLOCK_BYTES]) -> u64 {
    le_u64(&page[..], 56)
}

fn set_total(page: &mut [u8; BLOCK_BYTES], value: u64) {
    page[56..64].copy_from_slice(&value.to_le_bytes());
}

/// A record's place on a page: its payload offset and lengths.
#[derive(Clone, Copy)]
struct Slot {
    offset: usize,
    key: usize,
    value: usize,
}

impl Slot {
    fn len(self) -> usize {
        4 + self.key + self.value
    }

    fn key(self, page: &[u8; BLOCK_BYTES]) -> &[u8] {
        let start = PAYLOAD_OFFSET + self.offset + 4;
        &page[start..start + self.key]
    }

    fn value(self, page: &[u8; BLOCK_BYTES]) -> &[u8] {
        let start = PAYLOAD_OFFSET + self.offset + 4 + self.key;
        &page[start..start + self.value]
    }
}

/// The record at `index`; pages are built by this writer or validated.
fn slot(page: &[u8; BLOCK_BYTES], index: usize) -> Slot {
    let mut offset = 0;
    for _ in 0..index {
        offset +=
            4 + get_u16(page, PAYLOAD_OFFSET + offset) + get_u16(page, PAYLOAD_OFFSET + offset + 2);
    }
    Slot {
        offset,
        key: get_u16(page, PAYLOAD_OFFSET + offset),
        value: get_u16(page, PAYLOAD_OFFSET + offset + 2),
    }
}

/// The index of the first record whose key is at least `key`, and whether
/// it is equal.
fn search(page: &[u8; BLOCK_BYTES], key: &[u8]) -> (usize, bool) {
    let mut offset = 0;
    for index in 0..count(page) {
        let slot = Slot {
            offset,
            key: get_u16(page, PAYLOAD_OFFSET + offset),
            value: get_u16(page, PAYLOAD_OFFSET + offset + 2),
        };
        match compare(slot.key(page), key) {
            Ordering::Less => offset += slot.len(),
            order => return (index, order == Ordering::Equal),
        }
    }
    (count(page), false)
}

/// Append a record to a page being built in key order, if it fits.
pub(crate) fn append(page: &mut [u8; BLOCK_BYTES], key: &[u8], value: &[u8]) -> bool {
    insert(page, count(page), key, value)
}

/// Insert a record before `index` if it fits.
fn insert(page: &mut [u8; BLOCK_BYTES], index: usize, key: &[u8], value: &[u8]) -> bool {
    let used = payload(page);
    let len = 4 + key.len() + value.len();
    if used + len > PAGE_CAPACITY {
        return false;
    }
    let at = PAYLOAD_OFFSET + slot(page, index).offset;
    page.copy_within(at..PAYLOAD_OFFSET + used, at + len);
    set_u16(page, at, key.len());
    set_u16(page, at + 2, value.len());
    page[at + 4..at + 4 + key.len()].copy_from_slice(key);
    page[at + 4 + key.len()..at + len].copy_from_slice(value);
    let records = count(page);
    set_u16(page, 32, records + 1);
    set_u16(page, 34, used + len);
    true
}

/// Remove the record at `index`, zeroing the freed tail.
fn remove(page: &mut [u8; BLOCK_BYTES], index: usize) {
    let used = payload(page);
    let slot = slot(page, index);
    let at = PAYLOAD_OFFSET + slot.offset;
    page.copy_within(at + slot.len()..PAYLOAD_OFFSET + used, at);
    page[PAYLOAD_OFFSET + used - slot.len()..PAYLOAD_OFFSET + used].fill(0);
    let records = count(page);
    set_u16(page, 32, records - 1);
    set_u16(page, 34, used - slot.len());
}

/// An empty page of `level` with total zero.
fn empty_page(level: u8) -> Result<KBox<[u8; BLOCK_BYTES]>> {
    let mut page = KBox::new([0; BLOCK_BYTES], GFP_KERNEL)?;
    page[14] = level;
    Ok(page)
}

/// Data blocks an extent record accounts for; others count zero.
fn extent_blocks(kind: TreeKind, key: &[u8], value: &[u8]) -> u64 {
    if kind == TreeKind::Catalog && key.get(8) == Some(&3) {
        le_u64(value, 8) & u64::from(u32::MAX)
    } else {
        0
    }
}

/// One step of an edit's path: a draft and the child it continues to.
#[derive(Clone, Copy)]
struct Step {
    id: usize,
    index: usize,
}

/// Draft pages, tree roots, and allocations of the open transaction.
pub(crate) struct Txn {
    pub(crate) drafts: KVec<Draft>,
    roots: [Link; 3],
    /// Committed metadata pages drafts replaced; commit returns them to the
    /// workspace list.
    pub(crate) superseded: KVec<u64>,
    pub(crate) generation: u64,
    pub(crate) next_object: u64,
    pub(crate) committed_allocation: TreeRef,
    pub(crate) groups: KVec<Group>,
    /// Data blocks this transaction superseded, as runs.
    pub(crate) retired: KVec<(u64, u64)>,
    pub(crate) data_allocated: u64,
    pub(crate) reclaimed: u64,
    /// Where the next data allocation starts looking.
    pub(crate) hint: u64,
    /// Edits made, so a failed change can tell whether it edited anything.
    pub(crate) edits: u64,
}

/// Where edits read committed pages from.
pub(crate) struct Disk {
    pub(crate) sb: *mut super_block,
    pub(crate) uuid: [u8; 16],
    pub(crate) blocks: u64,
    /// The committed generation: no committed page is newer.
    pub(crate) generation: u64,
}

fn tree_index(kind: TreeKind) -> usize {
    match kind {
        TreeKind::Catalog => 0,
        TreeKind::Allocation => 1,
        _ => 2,
    }
}

impl Txn {
    /// A transaction on the committed `root`, allocating data from `hint`.
    pub(crate) fn new(root: &format::Root, hint: u64) -> Result<Self> {
        Ok(Self {
            drafts: KVec::new(),
            roots: [
                Link::Disk(root.catalog),
                Link::Disk(root.allocation),
                Link::Disk(root.cleanup),
            ],
            superseded: KVec::new(),
            generation: root.generation.checked_add(1).ok_or(ENOSPC)?,
            next_object: root.next_object,
            committed_allocation: root.allocation,
            groups: KVec::new(),
            retired: KVec::new(),
            data_allocated: 0,
            reclaimed: 0,
            hint,
            edits: 0,
        })
    }

    pub(crate) fn root(&self, kind: TreeKind) -> Link {
        self.roots[tree_index(kind)]
    }

    /// Live drafts, which commit must fund from workspace slots.
    pub(crate) fn live_drafts(&self) -> usize {
        self.drafts.iter().filter(|draft| draft.live).count()
    }

    fn page(&mut self, id: usize) -> &mut [u8; BLOCK_BYTES] {
        &mut self.drafts[id].page
    }

    fn add(&mut self, page: KBox<[u8; BLOCK_BYTES]>) -> Result<usize> {
        self.drafts.push(Draft { page, live: true }, GFP_KERNEL)?;
        Ok(self.drafts.len() - 1)
    }

    /// The draft of the page `link` names, copying a committed page.
    fn own(&mut self, disk: &Disk, kind: TreeKind, link: Link, level: Option<u8>) -> Result<usize> {
        let reference = match link {
            Link::Draft(id) => return Ok(id),
            Link::Disk(reference) => reference,
        };
        let block = Block::read(disk.sb, reference.block)?;
        format::decode_page(
            block.data(),
            reference,
            &disk.uuid,
            kind,
            disk.generation,
            level,
            disk.blocks,
        )
        .map_err(|error| corrupt(reference.block, error))?;
        let mut page = KBox::new([0; BLOCK_BYTES], GFP_KERNEL)?;
        page.copy_from_slice(block.data());
        self.superseded.reserve(1, GFP_KERNEL)?;
        let id = self.add(page)?;
        self.superseded.push(reference.block, GFP_KERNEL)?;
        Ok(id)
    }

    /// Copy the path to where `key` belongs: the root's draft first, the
    /// leaf's last, each with the child the next one is.
    fn path(&mut self, disk: &Disk, kind: TreeKind, key: &[u8]) -> Result<KVec<Step>> {
        let root = self.own(disk, kind, self.root(kind), None)?;
        self.roots[tree_index(kind)] = Link::Draft(root);
        let mut path = KVec::new();
        let mut id = root;
        loop {
            let page = &self.drafts[id].page;
            if level(page) == 0 {
                path.push(Step { id, index: 0 }, GFP_KERNEL)?;
                return Ok(path);
            }
            // The last child whose minimum is at most `key`, or the first.
            let (index, equal) = search(page, key);
            let index = if equal {
                index
            } else {
                index.saturating_sub(1)
            };
            let child_level = level(page) - 1;
            let link = Link::of(slot(page, index).value(page));
            let child = self.own(disk, kind, link, Some(child_level))?;
            if link != Link::Draft(child) {
                let at = PAYLOAD_OFFSET + slot(&self.drafts[id].page, index).offset;
                let key_len = get_u16(&self.drafts[id].page, at);
                let value_at = at + 4 + key_len;
                self.page(id)[value_at..value_at + 16].copy_from_slice(&Link::Draft(child).value());
            }
            path.push(Step { id, index }, GFP_KERNEL)?;
            id = child;
        }
    }

    /// Insert `key` with `value`, replacing any record with that key.
    pub(crate) fn put(&mut self, disk: &Disk, kind: TreeKind, key: &[u8], value: &[u8]) -> Result {
        if key.is_empty() || 4 + key.len() + value.len() > MAX_RECORD {
            return Err(EINVAL);
        }
        self.edits += 1;
        let path = self.path(disk, kind, key)?;
        let leaf = path[path.len() - 1].id;
        let (index, equal) = search(&self.drafts[leaf].page, key);
        let mut delta = extent_blocks(kind, key, value) as i64;
        if equal {
            let page = &self.drafts[leaf].page;
            let old = slot(page, index);
            delta -= extent_blocks(kind, key, old.value(page)) as i64;
            remove(self.page(leaf), index);
        }
        self.adjust_totals(&path, delta);
        self.place(disk, kind, &path, path.len() - 1, index, key, value)?;
        Ok(())
    }

    /// Remove the record with `key`; whether there was one.
    pub(crate) fn delete(&mut self, disk: &Disk, kind: TreeKind, key: &[u8]) -> Result<bool> {
        self.edits += 1;
        let path = self.path(disk, kind, key)?;
        let leaf = path[path.len() - 1].id;
        let (index, equal) = search(&self.drafts[leaf].page, key);
        if !equal {
            return Ok(false);
        }
        let page = &self.drafts[leaf].page;
        let old = slot(page, index);
        let delta = -(extent_blocks(kind, key, old.value(page)) as i64);
        remove(self.page(leaf), index);
        self.adjust_totals(&path, delta);
        self.after_removal(disk, kind, &path, path.len() - 1, index)?;
        Ok(true)
    }

    /// Add `delta` to the subtree total of every page on `path`.
    fn adjust_totals(&mut self, path: &[Step], delta: i64) {
        if delta == 0 {
            return;
        }
        for step in path {
            let page = self.page(step.id);
            let total = total(page).saturating_add_signed(delta);
            set_total(page, total);
        }
    }

    /// Insert a record into the page at `depth` of `path` before `index`,
    /// splitting it, and its ancestors, as needed.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        disk: &Disk,
        kind: TreeKind,
        path: &[Step],
        depth: usize,
        index: usize,
        key: &[u8],
        value: &[u8],
    ) -> Result {
        let id = path[depth].id;
        if insert(self.page(id), index, key, value) {
            if index == 0 {
                self.minimum_changed(disk, kind, path, depth)?;
            }
            return Ok(());
        }
        // Split: the page and the new record, divided by bytes.
        let mut old = empty_page(level(&self.drafts[id].page))?;
        old.copy_from_slice(&self.drafts[id].page[..]);
        let records = count(&old) + 1;
        let new_len = 4 + key.len() + value.len();
        let total_bytes = payload(&old) + new_len;
        let record = |i: usize| -> (&[u8], &[u8]) {
            match i.cmp(&index) {
                Ordering::Less => {
                    let s = slot(&old, i);
                    (s.key(&old), s.value(&old))
                }
                Ordering::Equal => (key, value),
                Ordering::Greater => {
                    let s = slot(&old, i - 1);
                    (s.key(&old), s.value(&old))
                }
            }
        };
        let mut left_bytes = 0;
        let mut split = 0;
        while split < records - 1 {
            let (k, v) = record(split);
            if left_bytes + 4 + k.len() + v.len() > total_bytes / 2 && split > 0 {
                break;
            }
            left_bytes += 4 + k.len() + v.len();
            split += 1;
        }
        let page_level = level(&old);
        let mut left = empty_page(page_level)?;
        let mut right = empty_page(page_level)?;
        for i in 0..records {
            let (k, v) = record(i);
            let target = if i < split { &mut left } else { &mut right };
            let at = count(target);
            if !insert(target, at, k, v) {
                return Err(EINVAL);
            }
        }
        let left_total = self.page_total(disk, kind, &left, page_level)?;
        let right_total = self.page_total(disk, kind, &right, page_level)?;
        set_total(&mut left, left_total);
        set_total(&mut right, right_total);
        let right_key = {
            let s = slot(&right, 0);
            let mut k = KVec::new();
            k.extend_from_slice(s.key(&right), GFP_KERNEL)?;
            k
        };
        self.drafts[id].page.copy_from_slice(&left[..]);
        let right_id = self.add(right)?;
        let right_value = Link::Draft(right_id).value();
        if depth == 0 {
            // A new root above the two halves.
            let left_key = {
                let s = slot(&left, 0);
                let mut k = KVec::new();
                k.extend_from_slice(s.key(&left), GFP_KERNEL)?;
                k
            };
            let mut root = empty_page(page_level + 1)?;
            insert(&mut root, 0, &left_key, &Link::Draft(id).value());
            insert(&mut root, 1, &right_key, &right_value);
            set_total(&mut root, left_total + right_total);
            let root_id = self.add(root)?;
            self.roots[tree_index(kind)] = Link::Draft(root_id);
            return Ok(());
        }
        if index == 0 {
            self.minimum_changed(disk, kind, path, depth)?;
        }
        let parent = path[depth - 1];
        self.place(
            disk,
            kind,
            path,
            depth - 1,
            parent.index + 1,
            &right_key,
            &right_value,
        )
    }

    /// The data-block total of a page this writer just packed: leaves sum
    /// their extents; internal pages sum their children's totals.
    fn page_total(
        &self,
        disk: &Disk,
        kind: TreeKind,
        page: &[u8; BLOCK_BYTES],
        page_level: u8,
    ) -> Result<u64> {
        if kind != TreeKind::Catalog {
            return Ok(0);
        }
        let mut sum = 0u64;
        for i in 0..count(page) {
            let s = slot(page, i);
            sum += if page_level == 0 {
                extent_blocks(kind, s.key(page), s.value(page))
            } else {
                self.child_total(disk, Link::of(s.value(page)))?
            };
        }
        Ok(sum)
    }

    /// A child's subtree total: a draft keeps it current; a committed page
    /// recorded it, and splits of internal pages are rare enough to read it.
    fn child_total(&self, disk: &Disk, link: Link) -> Result<u64> {
        match link {
            Link::Draft(id) => Ok(total(&self.drafts[id].page)),
            Link::Disk(reference) => Ok(le_u64(Block::read(disk.sb, reference.block)?.data(), 56)),
        }
    }

    /// The first key of the page at `depth` changed: update its parent's
    /// record for it, and on up while that is the parent's first record.
    fn minimum_changed(
        &mut self,
        disk: &Disk,
        kind: TreeKind,
        path: &[Step],
        depth: usize,
    ) -> Result {
        if depth == 0 {
            return Ok(());
        }
        let id = path[depth].id;
        let mut key = KVec::new();
        {
            let page = &self.drafts[id].page;
            if count(page) == 0 {
                return Ok(());
            }
            key.extend_from_slice(slot(page, 0).key(page), GFP_KERNEL)?;
        }
        let parent = path[depth - 1];
        let value = {
            let page = &self.drafts[parent.id].page;
            let s = slot(page, parent.index);
            if s.key(page) == &key[..] {
                return Ok(());
            }
            let mut value = [0; 16];
            value.copy_from_slice(s.value(page));
            value
        };
        remove(self.page(parent.id), parent.index);
        self.place(disk, kind, path, depth - 1, parent.index, &key, &value)
    }

    /// Restore the tree's shape after removing the record at `index` of the
    /// page at `depth`.
    fn after_removal(
        &mut self,
        disk: &Disk,
        kind: TreeKind,
        path: &[Step],
        depth: usize,
        index: usize,
    ) -> Result {
        let id = path[depth].id;
        let records = count(&self.drafts[id].page);
        if depth == 0 {
            // A root keeps any number of records but one child.
            if records == 1 && level(&self.drafts[id].page) > 0 {
                let page = &self.drafts[id].page;
                let child = Link::of(slot(page, 0).value(page));
                self.drafts[id].live = false;
                self.roots[tree_index(kind)] = child;
            }
            return Ok(());
        }
        if records == 0 {
            // Empty non-root pages are invalid: drop it from its parent.
            self.drafts[id].live = false;
            let parent = path[depth - 1];
            remove(self.page(parent.id), parent.index);
            return self.after_removal(disk, kind, path, depth - 1, parent.index);
        }
        if index == 0 {
            self.minimum_changed(disk, kind, path, depth)?;
        }
        if records >= 2 || level(&self.drafts[id].page) == 0 {
            return Ok(());
        }
        self.rebalance(disk, kind, path, depth)
    }

    /// The internal page at `depth` has one child: merge it with a sibling,
    /// or borrow a child from one that is too full to merge.
    fn rebalance(&mut self, disk: &Disk, kind: TreeKind, path: &[Step], depth: usize) -> Result {
        let id = path[depth].id;
        let parent = path[depth - 1];
        let siblings = count(&self.drafts[parent.id].page);
        let sibling_index = if parent.index > 0 {
            parent.index - 1
        } else {
            parent.index + 1
        };
        if sibling_index >= siblings {
            return Ok(());
        }
        let page_level = level(&self.drafts[id].page);
        let link = {
            let page = &self.drafts[parent.id].page;
            Link::of(slot(page, sibling_index).value(page))
        };
        let sibling = self.own(disk, kind, link, Some(page_level))?;
        if link != Link::Draft(sibling) {
            let at = PAYLOAD_OFFSET + slot(&self.drafts[parent.id].page, sibling_index).offset;
            let key_len = get_u16(&self.drafts[parent.id].page, at);
            self.page(parent.id)[at + 4 + key_len..at + 4 + key_len + 16]
                .copy_from_slice(&Link::Draft(sibling).value());
        }
        let (left, right, right_index) = if sibling_index < parent.index {
            (sibling, id, parent.index)
        } else {
            (id, sibling, sibling_index)
        };
        let left_used = payload(&self.drafts[left].page);
        let right_used = payload(&self.drafts[right].page);
        if left_used + right_used <= PAGE_CAPACITY {
            // Merge right into left and drop right from the parent.
            let mut moved = empty_page(page_level)?;
            moved.copy_from_slice(&self.drafts[right].page[..]);
            for i in 0..count(&moved) {
                let s = slot(&moved, i);
                let at = count(&self.drafts[left].page);
                insert(self.page(left), at, s.key(&moved), s.value(&moved));
            }
            let sum = total(&self.drafts[left].page) + total(&moved);
            set_total(self.page(left), sum);
            self.drafts[right].live = false;
            remove(self.page(parent.id), right_index);
            return self.after_removal(disk, kind, path, depth - 1, right_index);
        }
        // Borrow the sibling's nearest child; it has at least two. Either
        // way only the right page's minimum changes.
        let (from, to, from_index, to_index) = if left == sibling {
            (left, right, count(&self.drafts[left].page) - 1, 0)
        } else {
            (right, left, 0, count(&self.drafts[left].page))
        };
        let (key, value) = {
            let page = &self.drafts[from].page;
            let s = slot(page, from_index);
            let mut key = KVec::new();
            key.extend_from_slice(s.key(page), GFP_KERNEL)?;
            let mut value = [0; 16];
            value.copy_from_slice(s.value(page));
            (key, value)
        };
        let moved_total = self.child_total(disk, Link::of(&value))?;
        remove(self.page(from), from_index);
        if !insert(self.page(to), to_index, &key, &value) {
            return Err(EINVAL);
        }
        let from_total = total(&self.drafts[from].page).saturating_sub(moved_total);
        set_total(self.page(from), from_total);
        let to_total = total(&self.drafts[to].page) + moved_total;
        set_total(self.page(to), to_total);
        let mut right_path = KVec::new();
        right_path.extend_from_slice(&path[..depth], GFP_KERNEL)?;
        right_path[depth - 1].index = right_index;
        right_path.push(
            Step {
                id: right,
                index: 0,
            },
            GFP_KERNEL,
        )?;
        self.minimum_changed(disk, kind, &right_path, depth)?;
        Ok(())
    }
}
