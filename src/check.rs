//! Independent read-only traversal and allocation reconciliation.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasherDefault;

use crate::format::{BLOCK_BYTES, Page, Root, TreeKind, TreeRef, checksum_slot, decode_page};
use crate::tree::{PageHasher, Record, tree_ref, u64_be, u64_le};
use crate::{BlockDevice, Error, Result};

// A 32 MiB ownership bitmap covers 1 TiB, so volumes up to that size need one
// forest pass and 32 TiB needs 32.
const WINDOW_BLOCKS: u64 = 1 << 28;
const GROUP: u64 = 16_384;
// One catalog pass and a one MiB bitmap per window of object IDs.
const LINK_WINDOW: u64 = 8_388_608;
const LINK_REASON: &str = "linked inode does not have exactly one directory entry";

/// Counts from a complete structural check. File data checksums require scrub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckSummary {
    /// Reachable metadata pages, including detached trees.
    pub metadata_pages: u64,
    /// Blocks in live and detached extents.
    pub data_blocks: u64,
    /// Inodes in the live catalog, including orphans.
    pub objects: u64,
    /// Free nonreserved blocks reconciled with ownership.
    pub free_blocks: u64,
    /// Files whose checksum-unit enumeration finished; zero for structural checks.
    pub inspected_files: u64,
    /// Backed 64 KiB units successfully verified; zero for structural checks.
    pub verified_units: u64,
    /// Backed units that could not be read or verified.
    pub unreadable_ranges: u64,
}

struct Summary {
    first: Vec<u8>,
    last: Vec<u8>,
    data: u64,
}

#[derive(Clone, Copy)]
struct Position {
    reference: TreeRef,
    kind: TreeKind,
    parent_generation: u64,
    level: Option<u8>,
}

struct Reader<'a, D> {
    device: &'a mut D,
    root: &'a Root,
    // Pages this check's point lookups already decoded, keyed by everything
    // the decode validated. The check holds the device, so bytes cannot change.
    lookups: HashMap<LookupKey, (u8, Vec<Record>), BuildHasherDefault<PageHasher>>,
    // Pages point lookups read that the walk has not reached yet: a directory
    // entry looks up its child's inode, which sits in a later leaf. The walk
    // takes the page from here instead of reading it again. Each visit still
    // decodes and verifies the bytes; only the device read is saved.
    ahead: HashMap<u64, Page>,
    ahead_order: VecDeque<u64>,
    // Directories whose parent chain this check has followed to the root
    // without a cycle, so later walks stop at them.
    rooted: HashSet<u64>,
    // Where a writable open keeps the catalog leaves this check verified.
    keep: Option<&'a mut crate::tree::Cache>,
}

type LookupKey = (u64, u64, TreeKind, u64, Option<u8>);
/// A record borrowed from the visited leaf or copied from a lookup.
type Nearby<'r> = (Cow<'r, [u8]>, Cow<'r, [u8]>);
const LOOKUP_PAGES: usize = 1024;
// Keep up to 4 MiB of page data. The walk rereads pages evicted before it
// reaches them.
const AHEAD_PAGES: usize = 1024;
// Bound the ancestor memo. Once full, uncached parent chains are still checked,
// but repeated walks can be quadratic.
const ROOTED_DIRECTORIES: usize = 1 << 20;

type Visitor<'a, D> = dyn FnMut(&mut Reader<'_, D>, u64, u8, &[(&[u8], &[u8])]) -> Result<()> + 'a;

impl<'a, D: BlockDevice> Reader<'a, D> {
    fn new(device: &'a mut D, root: &'a Root) -> Self {
        Self {
            device,
            root,
            lookups: HashMap::default(),
            ahead: HashMap::new(),
            ahead_order: VecDeque::new(),
            rooted: HashSet::new(),
            keep: None,
        }
    }

    fn position(&self, reference: TreeRef, kind: TreeKind) -> Position {
        Position {
            reference,
            kind,
            parent_generation: self.root.generation,
            level: None,
        }
    }

    fn read(&mut self, position: Position, lookup: bool) -> Result<Page> {
        let reference = position.reference;
        if reference.block < 512 || reference.block >= self.root.blocks - 512 {
            return Err(Error::Corrupt("out-of-range tree page").at_block(reference.block));
        }
        let offset = reference
            .block
            .checked_mul(BLOCK_BYTES as u64)
            .ok_or(Error::Corrupt("tree offset overflow"))?;
        if !lookup && let Some(page) = self.ahead.remove(&reference.block) {
            return Ok(page);
        }
        let mut page = Page(Box::new([0; BLOCK_BYTES]));
        self.device.read_exact_at(offset, &mut page.0[..])?;
        if lookup {
            if self.ahead_order.len() == AHEAD_PAGES
                && let Some(oldest) = self.ahead_order.pop_front()
            {
                self.ahead.remove(&oldest);
            }
            if self.ahead.insert(reference.block, page.clone()).is_none() {
                self.ahead_order.push_back(reference.block);
            }
        }
        Ok(page)
    }

    /// Hint the device about an internal page's children, which the walk
    /// reads next, in runs of adjacent blocks. Out-of-range children are left
    /// for `read` to report.
    fn prefetch_children(&mut self, records: &[(&[u8], &[u8])]) -> Result<()> {
        let mut run: Option<(u64, u64)> = None;
        for (_, value) in records {
            let block = tree_ref(value)?.block;
            if block < 512 || block >= self.root.blocks - 512 {
                continue;
            }
            run = match run {
                Some((first, count)) if first + count == block => Some((first, count + 1)),
                Some(done) => {
                    self.hint(done)?;
                    Some((block, 1))
                }
                None => Some((block, 1)),
            };
        }
        match run {
            Some(done) => self.hint(done),
            None => Ok(()),
        }
    }

    fn hint(&mut self, (first, count): (u64, u64)) -> Result<()> {
        let bytes = BLOCK_BYTES as u64;
        let len = usize::try_from(count * bytes).map_err(|_| Error::Corrupt("tree run length"))?;
        self.device.prefetch_metadata(first * bytes, len)
    }

    fn walk(
        &mut self,
        position: Position,
        visits: &mut u64,
        visitor: &mut Visitor<'_, D>,
    ) -> Result<Summary> {
        *visits = visits
            .checked_add(1)
            .ok_or(Error::Corrupt("tree visit overflow"))?;
        if *visits > self.root.blocks - 1024 {
            return Err(Error::Corrupt("tree visits exceed volume"));
        }
        let block = position.reference.block;
        let page = self.read(position, false)?;
        let decoded = decode_page(
            &page.0[..],
            position.reference,
            &self.root.uuid,
            position.kind,
            position.parent_generation,
            position.level,
            self.root.blocks,
        )
        .map_err(|error| error.at_block(block))?;
        let first = decoded
            .records
            .first()
            .map_or_else(Vec::new, |(key, _)| key.to_vec());
        let last = decoded
            .records
            .last()
            .map_or_else(Vec::new, |(key, _)| key.to_vec());
        visitor(self, block, decoded.level, &decoded.records)
            .map_err(|error| error.at_block(block))?;
        if decoded.level == 0 {
            if decoded.records.is_empty() && position.level.is_some() {
                return Err(Error::Corrupt("empty non-root leaf").at_block(block));
            }
            if let Some(cache) = self.keep.as_deref_mut() {
                cache.keep_checked_leaf(
                    position.reference,
                    position.kind,
                    &decoded.records,
                    decoded.data_blocks,
                )?;
            }
            return Ok(Summary {
                first,
                last,
                data: decoded.data_blocks,
            });
        }
        self.prefetch_children(&decoded.records)?;
        let mut previous = Vec::new();
        let mut total = 0u64;
        for (key, value) in decoded.records {
            let child = self.walk(
                Position {
                    reference: tree_ref(value)?,
                    kind: position.kind,
                    parent_generation: position.reference.generation,
                    level: Some(decoded.level - 1),
                },
                visits,
                visitor,
            )?;
            if child.first != key || (!previous.is_empty() && previous >= child.first) {
                return Err(Error::Corrupt("child minimum or range mismatch").at_block(block));
            }
            total = total
                .checked_add(child.data)
                .ok_or(Error::Corrupt("subtree total overflow"))?;
            previous = child.last;
        }
        if total != decoded.data_blocks {
            return Err(Error::Corrupt("subtree total mismatch").at_block(block));
        }
        Ok(Summary {
            first,
            last: previous,
            data: total,
        })
    }

    fn lookup(&mut self, kind: TreeKind, key: &[u8], floor: bool) -> Result<Option<Record>> {
        let reference = match kind {
            TreeKind::Catalog => self.root.catalog,
            TreeKind::Allocation => self.root.allocation,
            TreeKind::Cleanup => self.root.cleanup,
            TreeKind::Workspace => {
                return Err(Error::InvalidInput("workspace requires the bank reader"));
            }
        };
        let mut position = self.position(reference, kind);
        for _ in 0..8 {
            let (level, records) = self.lookup_page(position)?;
            let at = records.partition_point(|(candidate, _)| candidate.as_slice() <= key);
            if *level == 0 {
                return Ok(at
                    .checked_sub(1)
                    .and_then(|index| records.get(index))
                    .filter(|(candidate, _)| floor || candidate == key)
                    .cloned());
            }
            if at == 0 {
                return Ok(None);
            }
            position = Position {
                reference: tree_ref(&records[at - 1].1)?,
                kind,
                parent_generation: position.reference.generation,
                level: Some(level - 1),
            };
        }
        Err(Error::Corrupt("lookup exceeds maximum tree depth"))
    }

    fn lookup_page(&mut self, position: Position) -> Result<&(u8, Vec<Record>)> {
        let key = (
            position.reference.block,
            position.reference.generation,
            position.kind,
            position.parent_generation,
            position.level,
        );
        if !self.lookups.contains_key(&key) {
            let page = self.read(position, true)?;
            let decoded = decode_page(
                &page.0[..],
                position.reference,
                &self.root.uuid,
                position.kind,
                position.parent_generation,
                position.level,
                self.root.blocks,
            )
            .map_err(|error| error.at_block(position.reference.block))?;
            let records = decoded
                .records
                .iter()
                .map(|(key, value)| (key.to_vec(), value.to_vec()))
                .collect();
            // Drop the memo at the page limit to bound memory without tracking recency.
            if self.lookups.len() >= LOOKUP_PAGES {
                self.lookups.clear();
            }
            self.lookups.insert(key, (decoded.level, records));
        }
        self.lookups
            .get(&key)
            .ok_or(Error::InvalidInput("lookup page memo"))
    }

    fn inode(&mut self, owner: u64) -> Result<Vec<u8>> {
        self.lookup(TreeKind::Catalog, &catalog_key(owner, 1, None), false)?
            .map(|(_, value)| value)
            .ok_or(Error::Corrupt("missing inode"))
    }

    fn parent(&mut self, owner: u64) -> Result<u64> {
        let inode = self.inode(owner)?;
        if inode[0] != 2 || inode[1] != 0 {
            return Err(Error::Corrupt("linked ancestor is not a linked directory"));
        }
        u64_le(&inode[8..16])
    }

    // Follow parents from `parent` to the root or to a directory already proven
    // to reach it. A parent seen twice on one walk is a cycle. The walk's path is
    // held while it runs, so memory for one walk grows with the chain it walks.
    fn ancestry(&mut self, parent: u64) -> Result<()> {
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        let mut current = parent;
        while current != 1 && !self.rooted.contains(&current) {
            if !seen.insert(current) {
                return Err(Error::Corrupt("directory parent cycle"));
            }
            path.push(current);
            current = self.parent(current)?;
        }
        let room = ROOTED_DIRECTORIES.saturating_sub(self.rooted.len());
        self.rooted.extend(path.into_iter().take(room));
        Ok(())
    }

    fn inode_link(&mut self, owner: u64, inode: &[u8]) -> Result<()> {
        let orphan = inode[1] & 1 != 0;
        let marker = self
            .lookup(TreeKind::Cleanup, &cleanup_key(3, owner), false)?
            .is_some();
        if owner == 1 {
            if inode[0] != 2 || orphan || u64_le(&inode[8..16])? != 1 || marker {
                return Err(Error::Corrupt("invalid root directory"));
            }
            return Ok(());
        }
        if marker != orphan {
            return Err(Error::Corrupt("orphan marker mismatch"));
        }
        if orphan {
            return Ok(());
        }
        let parent = u64_le(&inode[8..16])?;
        self.ancestry(parent)
    }

    // Forward validation proves each directory entry names a linked non-root
    // inode whose parent is the entry's directory. One pass per window of
    // object IDs counts those inodes and marks entry targets; equal counts with
    // no repeated target mean every linked inode has exactly one entry. The
    // scan also finds the next occupied window, skipping gaps in issued IDs.
    fn inverse_links(&mut self, start: u64, end: u64) -> Result<Option<u64>> {
        const REASON: &str = LINK_REASON;
        let window = start..end;
        let mut targets = vec![0u8; (end - start).div_ceil(8) as usize];
        let (mut linked, mut entries) = (0u64, 0u64);
        let mut next = None;
        self.walk(
            self.position(self.root.catalog, TreeKind::Catalog),
            &mut 0,
            &mut |_, page, level, records| {
                if level != 0 {
                    return Ok(());
                }
                for (key, value) in records {
                    if key[8] == 1 {
                        let owner = u64_be(&key[..8])?;
                        if owner != 1 && value[1] & 1 == 0 {
                            if window.contains(&owner) {
                                linked += 1;
                            } else if owner >= end {
                                next = Some(next.map_or(owner, |id: u64| id.min(owner)));
                            }
                        }
                    } else if key[8] == 2 {
                        let target = u64_le(&value[..8])?;
                        if window.contains(&target) {
                            let bit = target - start;
                            let byte = &mut targets[(bit / 8) as usize];
                            if *byte & (1 << (bit % 8)) != 0 {
                                return Err(Error::Corrupt(REASON).at_block(page));
                            }
                            *byte |= 1 << (bit % 8);
                            entries += 1;
                        } else if target >= end {
                            next = Some(next.map_or(target, |id| id.min(target)));
                        }
                    }
                }
                Ok(())
            },
        )?;
        if linked == entries {
            return Ok(next);
        }
        // Error path only: report the page of the first inode without an entry.
        self.walk(
            self.position(self.root.catalog, TreeKind::Catalog),
            &mut 0,
            &mut |_, page, level, records| {
                for (key, value) in records.iter().filter(|_| level == 0) {
                    let owner = u64_be(&key[..8])?;
                    let bit = owner.wrapping_sub(start);
                    if key[8] == 1
                        && owner != 1
                        && value[1] & 1 == 0
                        && window.contains(&owner)
                        && targets[(bit / 8) as usize] & (1 << (bit % 8)) == 0
                    {
                        return Err(Error::Corrupt(REASON).at_block(page));
                    }
                }
                Ok(())
            },
        )?;
        Err(Error::Corrupt(REASON))
    }

    /// `lookup` in the catalog, answered from the leaf being visited when its
    /// key range holds `key`. The walk rejects overlapping leaf ranges, so the
    /// record can be nowhere else. A file's extents and checksum groups are
    /// neighbours, so most of their cross-checks need no descent and no copy.
    fn nearby<'r>(
        &mut self,
        leaf: &[(&'r [u8], &'r [u8])],
        key: &[u8],
        floor: bool,
    ) -> Result<Option<Nearby<'r>>> {
        if let (Some(first), Some(last)) = (leaf.first(), leaf.last())
            && first.0 <= key
            && key <= last.0
        {
            let at = leaf.partition_point(|(candidate, _)| *candidate <= key);
            return Ok(at
                .checked_sub(1)
                .and_then(|index| leaf.get(index))
                .filter(|(candidate, _)| floor || *candidate == key)
                .map(|(key, value)| (Cow::Borrowed(*key), Cow::Borrowed(*value))));
        }
        Ok(self
            .lookup(TreeKind::Catalog, key, floor)?
            .map(|(key, value)| (Cow::Owned(key), Cow::Owned(value))))
    }

    fn checksums_for_extent(
        &mut self,
        leaf: &[(&[u8], &[u8])],
        owner: u64,
        logical: u64,
        count: u64,
    ) -> Result<()> {
        let first = logical / 16;
        let last = (logical + count - 1) / 16;
        for group in first / 256..=last / 256 {
            let (_, value) = self
                .nearby(leaf, &catalog_key(owner, 4, Some(group)), false)?
                .ok_or(Error::Corrupt("backed range has no checksum group"))?;
            for unit in first.max(group * 256)..=last.min(group * 256 + 255) {
                if checksum_slot(&value, (unit % 256) as usize)?.is_none() {
                    return Err(Error::Corrupt("backed range has no checksum"));
                }
            }
        }
        Ok(())
    }

    /// `held` is the last extent found, as its first block and its end. The
    /// walk rejects overlapping extents, so the one that backed the previous
    /// unit backs every block up to its end without another lookup.
    fn backed_unit(
        &mut self,
        leaf: &[(&[u8], &[u8])],
        owner: u64,
        unit: u64,
        size: u64,
        held: &mut Option<(u64, u64)>,
    ) -> Result<()> {
        let mut block = unit
            .checked_mul(16)
            .ok_or(Error::Corrupt("checksum unit overflow"))?;
        let end = (block + 16).min(size.div_ceil(4096));
        while block < end {
            if let Some((start, next)) = *held
                && (start..next).contains(&block)
            {
                block = next.min(end);
                continue;
            }
            let (key, value) = self
                .nearby(leaf, &catalog_key(owner, 3, Some(block)), true)?
                .ok_or(Error::Corrupt("checksum refers to a hole"))?;
            if key.len() != 17 || key[8] != 3 || u64_be(&key[..8])? != owner {
                return Err(Error::Corrupt("checksum refers to a hole"));
            }
            let start = u64_be(&key[9..])?;
            let count = u64::from(u32::from_le_bytes(
                value[8..12]
                    .try_into()
                    .map_err(|_| Error::Corrupt("extent count"))?,
            ));
            let next = start
                .checked_add(count)
                .ok_or(Error::Corrupt("extent end overflow"))?;
            if next <= block {
                return Err(Error::Corrupt("partially backed checksum unit"));
            }
            *held = Some((start, next));
            block = next.min(end);
        }
        Ok(())
    }

    fn catalog(&mut self) -> Result<u64> {
        let mut state: Option<Object> = None;
        let mut count = 0;
        // While every issued ID fits one link window, this walk also does the
        // inverse-link pass, saving a second walk of the catalog.
        let fused = self.root.next_object <= LINK_WINDOW;
        let mut targets = vec![
            0u8;
            if fused {
                self.root.next_object.div_ceil(8) as usize
            } else {
                0
            }
        ];
        let (mut linked, mut entries) = (0u64, 0u64);
        self.walk(
            self.position(self.root.catalog, TreeKind::Catalog),
            &mut 0,
            &mut |reader, page, level, records| {
                if level != 0 {
                    return Ok(());
                }
                for (key, value) in records {
                    let owner = u64_be(&key[..8])?;
                    if owner >= reader.root.next_object {
                        return Err(
                            Error::Corrupt("catalog object exceeds allocated IDs").at_block(page)
                        );
                    }
                    if state.as_ref().is_none_or(|state| state.owner != owner) {
                        if let Some(state) = state.take() {
                            state.finish().map_err(|error| error.at_block(page))?;
                        }
                        if key[8] != 1 {
                            return Err(
                                Error::Corrupt("catalog object has no inode").at_block(page)
                            );
                        }
                        reader
                            .inode_link(owner, value)
                            .map_err(|error| error.at_block(page))?;
                        state = Some(Object {
                            owner,
                            inode: value.to_vec(),
                            children: 0,
                            blocks: 0,
                            end: 0,
                        });
                        count += 1;
                        linked += u64::from(fused && owner != 1 && value[1] & 1 == 0);
                    }
                    let state = state
                        .as_mut()
                        .ok_or(Error::Corrupt("missing catalog state"))?;
                    match key[8] {
                        1 => {}
                        2 => {
                            if state.inode[0] != 2 {
                                return Err(Error::Corrupt("non-directory has directory entries")
                                    .at_block(page));
                            }
                            crate::name::validate_disk_name(&value[12..], &key[9..])
                                .map_err(|error| error.at_block(page))?;
                            let target = u64_le(&value[..8])?;
                            if target == 1 || target >= reader.root.next_object {
                                return Err(
                                    Error::Corrupt("invalid directory target").at_block(page)
                                );
                            }
                            let target_inode = reader.inode(target)?;
                            if target_inode[0] != value[8]
                                || target_inode[1] & 1 != 0
                                || u64_le(&target_inode[8..16])? != owner
                            {
                                return Err(Error::Corrupt(
                                    "directory target type or parent mismatch",
                                )
                                .at_block(page));
                            }
                            if fused {
                                let byte = &mut targets[(target / 8) as usize];
                                if *byte & (1 << (target % 8)) != 0 {
                                    return Err(Error::Corrupt(LINK_REASON).at_block(page));
                                }
                                *byte |= 1 << (target % 8);
                                entries += 1;
                            }
                            state.children += 1;
                        }
                        3 => {
                            if state.inode[0] != 1 {
                                return Err(Error::Corrupt("non-file has extents").at_block(page));
                            }
                            let logical = u64_be(&key[9..])?;
                            let extent_count = u64::from(u32::from_le_bytes(
                                value[8..12]
                                    .try_into()
                                    .map_err(|_| Error::Corrupt("extent count"))?,
                            ));
                            let size = u64_le(&state.inode[16..24])?;
                            if logical < state.end
                                || logical + extent_count > size.div_ceil(4096)
                                || (logical > state.end
                                    && (!state.end.is_multiple_of(16)
                                        || !logical.is_multiple_of(16)))
                            {
                                return Err(Error::Corrupt(
                                    "extent overlap, gap inside unit, or EOF overrun",
                                )
                                .at_block(page));
                            }
                            state.end = logical + extent_count;
                            state.blocks = state
                                .blocks
                                .checked_add(extent_count)
                                .ok_or(Error::Corrupt("inode allocation overflow"))?;
                            reader.checksums_for_extent(records, owner, logical, extent_count)?;
                        }
                        4 => {
                            if state.inode[0] != 1 {
                                return Err(Error::Corrupt("non-file has checksums").at_block(page));
                            }
                            let group = u64_be(&key[9..])?;
                            let size = u64_le(&state.inode[16..24])?;
                            let present = value
                                .get(..32)
                                .ok_or(Error::Corrupt("checksum group slot"))?;
                            let mut held = None;
                            for slot in (0..256u64)
                                .filter(|slot| present[*slot as usize / 8] & (1 << (slot % 8)) != 0)
                            {
                                let unit = group
                                    .checked_mul(256)
                                    .and_then(|first| first.checked_add(slot))
                                    .ok_or(Error::Corrupt("checksum unit overflow"))?;
                                if unit.checked_mul(65_536).is_none_or(|start| start >= size) {
                                    return Err(Error::Corrupt("checksum past EOF").at_block(page));
                                }
                                reader.backed_unit(records, owner, unit, size, &mut held)?;
                            }
                        }
                        _ => return Err(Error::Corrupt("unknown catalog kind").at_block(page)),
                    }
                }
                Ok(())
            },
        )?;
        let state = state.ok_or(Error::Corrupt("catalog is empty"))?;
        state.finish()?;
        self.inode(1)?;
        if fused && linked == entries {
            return Ok(count);
        }
        // Unfused, or a mismatch whose page the second pass reports.
        let mut start = 0;
        loop {
            let end = (start + LINK_WINDOW).min(self.root.next_object);
            let Some(next) = self.inverse_links(start, end)? else {
                break;
            };
            start = next / LINK_WINDOW * LINK_WINDOW;
        }
        Ok(count)
    }
}

struct Object {
    owner: u64,
    inode: Vec<u8>,
    children: u64,
    blocks: u64,
    end: u64,
}

impl Object {
    fn finish(self) -> Result<()> {
        if self.children != u64_le(&self.inode[32..40])?
            || self.blocks != u64_le(&self.inode[24..32])?
        {
            return Err(Error::Corrupt("inode child or allocation count mismatch"));
        }
        let size = u64_le(&self.inode[16..24])?;
        if self.end != size.div_ceil(4096) && !self.end.is_multiple_of(16) {
            return Err(Error::Corrupt("backed tail is not checksum-unit aligned"));
        }
        Ok(())
    }
}

fn catalog_key(owner: u64, kind: u8, suffix: Option<u64>) -> Vec<u8> {
    let mut key = owner.to_be_bytes().to_vec();
    key.push(kind);
    if let Some(suffix) = suffix {
        key.extend_from_slice(&suffix.to_be_bytes());
    }
    key
}

fn cleanup_key(kind: u8, id: u64) -> Vec<u8> {
    let mut key = vec![kind];
    key.extend_from_slice(&id.to_be_bytes());
    key
}

struct Ownership {
    start: u64,
    end: u64,
    bits: Vec<u8>,
    count: u64,
    pages: u64,
    data: u64,
}

impl Ownership {
    fn page(&mut self, block: u64, blocks: u64) -> Result<()> {
        self.pages = self
            .pages
            .checked_add(1)
            .filter(|pages| *pages <= blocks - 1024)
            .ok_or(Error::Corrupt("forest pages exceed volume"))?;
        self.mark(block, 1, blocks, block)
    }

    fn data(&mut self, start: u64, count: u64, blocks: u64, source: u64) -> Result<()> {
        self.data = self
            .data
            .checked_add(count)
            .filter(|data| *data <= blocks - 1024)
            .ok_or(Error::Corrupt("forest extent totals exceed volume"))?;
        self.mark(start, count, blocks, source)
    }

    fn mark(&mut self, start: u64, count: u64, blocks: u64, source: u64) -> Result<()> {
        let end = start
            .checked_add(count)
            .ok_or(Error::Corrupt("ownership range overflow"))?;
        if count == 0 || start < 512 || end > blocks - 512 {
            return Err(Error::Corrupt("invalid ownership range").at_block(source));
        }
        for block in start.max(self.start)..end.min(self.end) {
            let bit = block - self.start;
            let byte = &mut self.bits[bit as usize / 8];
            let mask = 1 << (bit % 8);
            if *byte & mask != 0 {
                return Err(Error::Corrupt("block has multiple owners").at_block(source));
            }
            *byte |= mask;
            self.count += 1;
        }
        Ok(())
    }
}

pub(crate) fn validate(device: &mut impl BlockDevice, root: &Root) -> Result<CheckSummary> {
    let mut summary = validate_ownership(device, root)?;
    summary.objects = Reader::new(device, root).catalog()?;
    Ok(summary)
}

/// `validate` for a writable open, which keeps the catalog leaves it verified
/// in the volume's `cache`. A failed check leaves the cache to be dropped.
pub(crate) fn validate_keeping(
    device: &mut impl BlockDevice,
    root: &Root,
    cache: &mut crate::tree::Cache,
) -> Result<CheckSummary> {
    let mut summary = validate_ownership(device, root)?;
    let mut reader = Reader::new(device, root);
    reader.keep = Some(cache);
    summary.objects = reader.catalog()?;
    Ok(summary)
}

// Reconcile every allocated block independently of namespace semantics. The
// foreground state-machine tests run this after each mutation and run the full
// check at phase boundaries. Production inspection always calls `validate`.
pub(crate) fn validate_ownership(
    device: &mut impl BlockDevice,
    root: &Root,
) -> Result<CheckSummary> {
    ownership_in_windows(device, root, WINDOW_BLOCKS)
}

/// `validate_ownership` with `window` blocks per forest pass. Tests pass
/// windows smaller than an allocation group to cross window boundaries.
pub(crate) fn ownership_in_windows(
    device: &mut impl BlockDevice,
    root: &Root,
    window: u64,
) -> Result<CheckSummary> {
    let workspace = crate::workspace::read(device, root)?;
    let mut reader = Reader::new(device, root);
    let mut summary = CheckSummary {
        metadata_pages: 0,
        data_blocks: 0,
        objects: 0,
        free_blocks: 0,
        inspected_files: 0,
        verified_units: 0,
        unreadable_ranges: 0,
    };
    let mut total_owned = 0u64;
    // Reuse one bitmap across ownership windows to keep its allocation bounded.
    let mut bits = Vec::new();
    for start in (0..root.blocks).step_by(window as usize) {
        let end = (start + window).min(root.blocks);
        bits.clear();
        bits.resize((end - start).div_ceil(8) as usize, 0);
        let mut ownership = Ownership {
            start,
            end,
            bits: std::mem::take(&mut bits),
            count: 0,
            pages: 0,
            data: 0,
        };
        for block in &workspace {
            ownership.mark(*block, 1, root.blocks, root.workspace.block)?;
        }
        let mut visits = 0;
        for (reference, kind) in [
            (root.catalog, TreeKind::Catalog),
            (root.allocation, TreeKind::Allocation),
        ] {
            reader.walk(
                reader.position(reference, kind),
                &mut visits,
                &mut |_, block, level, records| {
                    ownership.page(block, root.blocks)?;

                    if level == 0 && kind == TreeKind::Catalog {
                        for (key, value) in records {
                            if key[8] == 3 {
                                let physical = u64_le(&value[..8])?;
                                let count = u64::from(u32::from_le_bytes(
                                    value[8..12]
                                        .try_into()
                                        .map_err(|_| Error::Corrupt("extent count"))?,
                                ));
                                ownership.data(physical, count, root.blocks, block)?;
                            }
                        }
                    }
                    Ok(())
                },
            )?;
        }
        reader.walk(
            reader.position(root.cleanup, TreeKind::Cleanup),
            &mut visits,
            &mut |reader, block, level, records| {
                ownership.page(block, root.blocks)?;

                if level != 0 {
                    return Ok(());
                }
                for (key, value) in records {
                    match key[0] {
                        1 => {
                            let first = u64_be(&key[9..17])?;
                            let count = u64::from(u32::from_le_bytes(
                                value[..4]
                                    .try_into()
                                    .map_err(|_| Error::Corrupt("retirement count"))?,
                            ));
                            ownership.mark(first, count, root.blocks, block)?;
                        }
                        2 => {
                            if u64_be(&key[1..])? >= root.next_job {
                                return Err(
                                    Error::Corrupt("job exceeds allocated IDs").at_block(block)
                                );
                            }
                            let owner = u64_le(&value[..8])?;
                            if owner >= root.next_object {
                                return Err(Error::Corrupt("job owner exceeds allocated IDs")
                                    .at_block(block));
                            }
                            let mut last_end = 0;
                            let mut job_visits = 0;
                            reader.walk(
                                Position {
                                    reference: tree_ref(&value[16..])?,
                                    kind: TreeKind::Catalog,
                                    parent_generation: root.generation,
                                    level: None,
                                },
                                &mut job_visits,
                                &mut |_, page, level, records| {
                                    ownership.page(page, root.blocks)?;

                                    if level != 0 {
                                        return Ok(());
                                    }
                                    for (key, record) in records {
                                        if u64_be(&key[..8])? != owner
                                            || !(value[8]..=value[9]).contains(&key[8])
                                        {
                                            return Err(Error::Corrupt(
                                                "detached tree owner or kind mismatch",
                                            )
                                            .at_block(page));
                                        }
                                        if key[8] == 3 {
                                            let logical = u64_be(&key[9..])?;
                                            let count = u64::from(u32::from_le_bytes(
                                                record[8..12]
                                                    .try_into()
                                                    .map_err(|_| Error::Corrupt("extent count"))?,
                                            ));
                                            if logical < last_end {
                                                return Err(Error::Corrupt(
                                                    "detached extents overlap",
                                                )
                                                .at_block(page));
                                            }
                                            last_end = logical + count;
                                            ownership.data(
                                                u64_le(&record[..8])?,
                                                count,
                                                root.blocks,
                                                page,
                                            )?;
                                        }
                                        if key[8] == 2 {
                                            crate::name::validate_disk_name(
                                                &record[12..],
                                                &key[9..],
                                            )
                                            .map_err(|error| error.at_block(page))?;
                                        }
                                    }
                                    Ok(())
                                },
                            )?;
                        }
                        3 => {
                            let owner = u64_be(&key[1..])?;
                            if owner == 1
                                || owner >= root.next_object
                                || reader.inode(owner)?[1] & 1 == 0
                            {
                                return Err(Error::Corrupt("invalid orphan marker").at_block(block));
                            }
                        }
                        _ => return Err(Error::Corrupt("unknown cleanup kind").at_block(block)),
                    }
                }
                Ok(())
            },
        )?;
        if start == 0 {
            summary.metadata_pages = ownership.pages;
            summary.data_blocks = ownership.data;
        }
        total_owned = total_owned
            .checked_add(ownership.count)
            .ok_or(Error::Corrupt("ownership total overflow"))?;
        reader.walk(
            reader.position(root.allocation, TreeKind::Allocation),
            &mut 0,
            &mut |_, page, level, records| {
                if level != 0 {
                    return Ok(());
                }
                for (key, bitmap) in records {
                    let group_start = u64_be(key)?
                        .checked_mul(GROUP)
                        .ok_or(Error::Corrupt("allocation group overflow"))?;
                    if group_start >= end || group_start + GROUP <= start {
                        continue;
                    }
                    for block in group_start.max(start).max(512)
                        ..(group_start + GROUP).min(end).min(root.blocks - 512)
                    {
                        let index = (block - start) as usize;
                        let set =
                            bitmap[(block - group_start) as usize / 8] & (1 << (block % 8)) != 0;
                        let owned = ownership.bits[index / 8] & (1 << (index % 8)) != 0;
                        if set != owned {
                            return Err(Error::Corrupt(
                                "allocation bitmap disagrees with ownership",
                            )
                            .at_block(page));
                        }
                        ownership.bits[index / 8] &= !(1 << (index % 8));
                    }
                }
                Ok(())
            },
        )?;
        if ownership.bits.iter().any(|byte| *byte != 0) {
            return Err(Error::Corrupt(
                "owned block is free in an implicit allocation group",
            ));
        }
        bits = ownership.bits;
    }
    summary.free_blocks = root
        .blocks
        .checked_sub(1024)
        .and_then(|count| count.checked_sub(total_owned))
        .and_then(|count| count.checked_add(workspace.len() as u64))
        .ok_or(Error::Corrupt("ownership exceeds volume"))?;
    if summary.free_blocks != root.free_blocks {
        return Err(Error::Corrupt("root free count disagrees with ownership"));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{crc32c, encode_leaf};
    use crate::volume::model::{FaultDevice, GEOMETRY, formatted_volume};
    use crate::{MountMode, Volume};
    use std::collections::{BTreeMap, BTreeSet};

    fn file_image(hole: bool, alias: bool, duplicate_link: bool) -> BTreeMap<u64, Page> {
        let mut image = formatted_volume().into_model_image();
        let mut root = Root::decode(&image[&256].0[..], 256, GEOMETRY).unwrap();
        let mut directory = decode_page(
            &image[&512].0[..],
            root.catalog,
            &root.uuid,
            TreeKind::Catalog,
            1,
            None,
            root.blocks,
        )
        .unwrap()
        .records[0]
            .1
            .to_vec();
        directory[32..40].copy_from_slice(&(if duplicate_link { 2u64 } else { 1 }).to_le_bytes());
        let mut file = directory.clone();
        file[0] = 1;
        file[32..40].fill(0);
        file[16..24].copy_from_slice(&65_536u64.to_le_bytes());
        file[24..32].copy_from_slice(&(if hole { 8u64 } else { 16 }).to_le_bytes());
        let mut entry = 2u64.to_le_bytes().to_vec();
        entry.extend_from_slice(&[1, 0, 1, 0, b'f']);
        let mut entry_key = catalog_key(1, 2, None);
        entry_key.push(b'f');
        let mut records = vec![
            (catalog_key(1, 1, None), directory),
            (entry_key.clone(), entry.clone()),
        ];
        if duplicate_link {
            entry_key[9] = b'g';
            entry[12] = b'g';
            records.push((entry_key, entry));
        }
        records.push((catalog_key(2, 1, None), file));
        let extent = |physical: u64| {
            let mut value = physical.to_le_bytes().to_vec();
            value.extend_from_slice(&8u32.to_le_bytes());
            value.extend_from_slice(&[0; 4]);
            value
        };
        records.push((catalog_key(2, 3, Some(0)), extent(4096)));
        if !hole {
            records.push((
                catalog_key(2, 3, Some(8)),
                extent(if alias { 4096 } else { 6144 }),
            ));
        }
        let mut checksums = vec![0; 1056];
        checksums[0] = 1;
        records.push((
            catalog_key(2, 4, Some(0)),
            crate::format::compact_group(&crate::format::expand_group(&checksums)).to_vec(),
        ));
        let borrowed: Vec<_> = records
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        image.insert(
            512,
            encode_leaf(
                root.catalog,
                &root.uuid,
                TreeKind::Catalog,
                &borrowed,
                root.blocks,
            )
            .unwrap(),
        );
        let mut bitmap = decode_page(
            &image[&513].0[..],
            root.allocation,
            &root.uuid,
            TreeKind::Allocation,
            1,
            None,
            root.blocks,
        )
        .unwrap()
        .records[0]
            .1
            .to_vec();
        for block in (4096..4104).chain(if !hole && !alias { 6144..6152 } else { 0..0 }) {
            bitmap[block / 8] |= 1 << (block % 8);
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
        root.next_object = 3;
        root.free_blocks -= if hole { 8 } else { 16 };
        image.insert(256, root.encode(256, GEOMETRY).unwrap());
        image.insert(
            root.blocks - 257,
            root.encode(root.blocks - 257, GEOMETRY).unwrap(),
        );
        image
    }

    /// Records page reads and metadata hints over a model image.
    struct Hinted {
        inner: FaultDevice,
        hinted: BTreeSet<u64>,
        unhinted_reads: Vec<u64>,
        reads: BTreeSet<u64>,
    }

    impl BlockDevice for Hinted {
        fn mode(&self) -> MountMode {
            self.inner.mode()
        }
        fn geometry(&mut self) -> Result<crate::Geometry> {
            self.inner.geometry()
        }
        fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
            let block = offset / BLOCK_BYTES as u64;
            if !self.hinted.contains(&block) {
                self.unhinted_reads.push(block);
            }
            self.reads.insert(block);
            self.inner.read_exact_at(offset, out)
        }
        fn prefetch_metadata(&mut self, offset: u64, len: usize) -> Result<()> {
            let first = offset / BLOCK_BYTES as u64;
            self.hinted
                .extend(first..first + (len / BLOCK_BYTES) as u64);
            Ok(())
        }
        fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            self.inner.write_all_at(offset, data)
        }
        fn flush_durable(&mut self) -> Result<()> {
            self.inner.flush_durable()
        }
    }

    /// The check hints every child page before reading it, and hints
    /// nothing it does not read.
    #[test]
    fn check_hints_exactly_the_child_pages_it_reads() {
        let mut volume = formatted_volume();
        for index in 0..2000 {
            let name = crate::Name::parse(&format!("f{index:04}")).unwrap();
            drop(volume.create_file(crate::ROOT, name).unwrap());
        }
        volume.sync().unwrap();
        let image = volume.into_model_image();
        let root = Root::decode(&image[&256].0[..], 256, GEOMETRY).unwrap();
        let mut device = Hinted {
            inner: FaultDevice::new(image, MountMode::ReadOnly),
            hinted: BTreeSet::new(),
            unhinted_reads: Vec::new(),
            reads: BTreeSet::new(),
        };
        let summary = validate(&mut device, &root).unwrap();
        assert!(summary.metadata_pages > 50);
        let tree_roots = [
            root.catalog.block,
            root.allocation.block,
            root.cleanup.block,
        ];
        let workspace = crate::workspace::read(&mut device.inner, &root).unwrap();
        let unhinted: Vec<_> = device
            .unhinted_reads
            .iter()
            .filter(|block| !tree_roots.contains(block) && !workspace.contains(block))
            .filter(|block| **block != root.workspace.block)
            .collect();
        assert!(unhinted.is_empty(), "read before a hint: {unhinted:?}");
        assert!(device.hinted.is_subset(&device.reads));
    }

    #[test]
    fn check_accepts_fragmented_checksum_units_without_writing_source() {
        let image = file_image(false, false, false);
        let device = FaultDevice::new(image.clone(), MountMode::ReadOnly);
        let media = device.media.clone();
        let mut volume = Volume::open(device).unwrap();
        let summary = volume.check().unwrap();
        assert_eq!(
            (summary.metadata_pages, summary.data_blocks, summary.objects),
            (3, 16, 2)
        );
        assert_eq!(media.borrow().durable, image);
        assert!(media.borrow().events.is_empty());
    }

    #[test]
    fn check_skips_empty_object_id_windows() {
        let mut image = file_image(false, false, false);
        let mut root = Root::decode(&image[&256].0[..], 256, GEOMETRY).unwrap();
        let high_id = i64::MAX as u64 - 1;
        let decoded = decode_page(
            &image[&512].0[..],
            root.catalog,
            &root.uuid,
            TreeKind::Catalog,
            root.generation,
            None,
            root.blocks,
        )
        .unwrap();
        let mut records: Vec<_> = decoded
            .records
            .iter()
            .map(|(key, value)| (key.to_vec(), value.to_vec()))
            .collect();
        for (key, value) in &mut records {
            if key[8] == 2 {
                value[..8].copy_from_slice(&high_id.to_le_bytes());
            } else if key[..8] == 2u64.to_be_bytes() {
                key[..8].copy_from_slice(&high_id.to_be_bytes());
            }
        }
        let borrowed: Vec<_> = records
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
            .collect();
        image.insert(
            512,
            encode_leaf(
                root.catalog,
                &root.uuid,
                TreeKind::Catalog,
                &borrowed,
                root.blocks,
            )
            .unwrap(),
        );
        root.next_object = i64::MAX as u64;
        image.insert(256, root.encode(256, GEOMETRY).unwrap());
        image.insert(
            root.blocks - 257,
            root.encode(root.blocks - 257, GEOMETRY).unwrap(),
        );
        let device = FaultDevice::new(image.clone(), MountMode::ReadOnly);
        let mut volume = Volume::open(device).unwrap();
        assert_eq!(volume.check().unwrap().objects, 2);
        assert!(Volume::open(FaultDevice::new(image, MountMode::ReadWrite)).is_ok());
    }

    #[test]
    fn check_rejects_partial_units_double_ownership_and_multiple_links() {
        for (hole, alias, duplicate_link) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let image = file_image(hole, alias, duplicate_link);
            let device = FaultDevice::new(image, MountMode::ReadOnly);
            let mut volume = Volume::open(device).unwrap();
            assert!(matches!(
                volume.check(),
                Err(Error::Corrupt(_) | Error::Corruption { .. })
            ));
        }
    }

    #[test]
    fn writable_open_checks_allocation_before_any_mirror_or_reclamation() {
        let mut image = file_image(false, false, false);
        let allocation = image.get_mut(&513).unwrap();
        // Leaf record: 80-byte header, 4-byte lengths, 8-byte group key.
        allocation.0[92 + 512 / 8] &= !(1 << (512 % 8));
        allocation.0[64..68].fill(0);
        let crc = crc32c(&allocation.0[..]);
        allocation.0[64..68].copy_from_slice(&crc.to_le_bytes());
        image.get_mut(&(GEOMETRY.blocks() - 257)).unwrap().0[1] ^= 1;
        let device = FaultDevice::new(image.clone(), MountMode::ReadWrite);
        let media = device.media.clone();
        assert!(matches!(
            Volume::open(device),
            Err(Error::Corruption { block: 513, .. })
        ));
        assert_eq!(media.borrow().durable, image);
        assert!(media.borrow().events.is_empty());
    }
}
