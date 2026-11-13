// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Read-only mount validation, before root repair or allocation.

use crate::ondisk::format::{self, checksum_slot, TreeKind, TreeRef};
use crate::tree::{self, be_u64, le_u64, Link, Tree};
use crate::volume::State;
use kernel::bindings::super_block;
use kernel::prelude::*;

// One bitmap covers 256 GiB of blocks or 67 million object IDs. Allocate
// individual pages so validation does not require contiguous large allocations.
const WINDOW: u64 = 1 << 26;
struct Bits {
    start: u64,
    end: u64,
    pages: KVec<KBox<[u8; 4096]>>,
    count: u64,
    metadata: u64,
    data: u64,
}
impl Bits {
    fn new(start: u64, end: u64) -> Result<Self> {
        let mut pages = KVec::new();
        for _ in 0..(end - start).div_ceil(32768) {
            pages.push(KBox::new([0; 4096], GFP_KERNEL)?, GFP_KERNEL)?;
        }
        Ok(Self {
            start,
            end,
            pages,
            count: 0,
            metadata: 0,
            data: 0,
        })
    }
    fn holds(&self, id: u64) -> bool {
        let bit = (id - self.start) as usize;
        self.pages[bit / 32768][bit % 32768 / 8] & (1 << (bit % 8)) != 0
    }
    fn mark(&mut self, id: u64) -> Result {
        if id < self.start || id >= self.end {
            return Ok(());
        }
        if self.holds(id) {
            return Err(bad("duplicate ownership or directory link"));
        }
        let bit = (id - self.start) as usize;
        self.pages[bit / 32768][bit % 32768 / 8] |= 1 << (bit % 8);
        self.count += 1;
        Ok(())
    }
    fn range(&mut self, first: u64, count: u64, blocks: u64) -> Result {
        let end = first
            .checked_add(count)
            .ok_or_else(|| bad("ownership overflow"))?;
        if first < 512 || end > blocks - 512 || count == 0 {
            return Err(bad("ownership outside allocatable blocks"));
        }
        for block in first.max(self.start)..end.min(self.end) {
            self.mark(block)?;
        }
        Ok(())
    }
    fn page(&mut self, block: u64, blocks: u64) -> Result {
        self.metadata = self.metadata.checked_add(1)
            .filter(|&count| count <= blocks - 1024)
            .ok_or_else(|| bad("forest pages exceed volume"))?;
        self.range(block, 1, blocks)
    }
    fn extent(&mut self, first: u64, count: u64, blocks: u64) -> Result {
        self.data = self.data.checked_add(count)
            .filter(|&count| count <= blocks - 1024)
            .ok_or_else(|| bad("forest extent totals exceed volume"))?;
        self.range(first, count, blocks)
    }
}
fn bad(reason: &'static str) -> Error {
    crate::corrupt(0, crate::ondisk::error::Error::Corrupt(reason))
}
fn reference(value: &[u8]) -> TreeRef {
    TreeRef {
        block: le_u64(value, 0),
        generation: le_u64(value, 8),
    }
}
fn detached<'a>(sb: *mut super_block, state: &'a State, reference: TreeRef) -> Tree<'a> {
    Tree {
        sb,
        uuid: state.root.uuid,
        blocks: state.root.blocks,
        kind: TreeKind::Catalog,
        root: Link::Disk(reference),
        generation: state.root.generation,
        drafts: &[],
    }
}
fn record(sb: *mut super_block, state: &State, object: u64) -> Result<KVec<u8>> {
    crate::namei::read_record(sb, state, object).map_err(|error| {
        if error == ENOENT {
            bad("missing inode record")
        } else {
            error
        }
    })
}

pub(crate) fn validate(sb: *mut super_block, state: &State) -> Result {
    ownership(sb, state)?;
    catalog(sb, state)?;
    links(sb, state)
}

fn ownership(sb: *mut super_block, state: &State) -> Result {
    let root = &state.root;
    let mut total = 0;
    let mut start = 0;
    while start < root.blocks {
        let end = (start + WINDOW).min(root.blocks);
        let mut bits = Bits::new(start, end)?;
        for &slot in state.slots.iter() {
            bits.range(slot, 1, root.blocks)?;
        }
        let mut visits = 0;
        for kind in [TreeKind::Catalog, TreeKind::Allocation, TreeKind::Cleanup] {
            let tree = Tree::new(sb, state, kind);
            tree.walk(tree.root_position(), &mut visits, &mut |at, view| {
                let Link::Disk(me) = at.link else {
                    return Err(EIO);
                };
                bits.page(me.block, root.blocks)?;
                if view.level != 0 {
                    return Ok(());
                }
                for (key, value) in view.records() {
                    if kind == TreeKind::Catalog && key.get(8) == Some(&tree::EXTENT) {
                        bits.extent(
                            le_u64(value, 0),
                            le_u64(value, 8) & u64::from(u32::MAX),
                            root.blocks,
                        )?;
                    } else if kind == TreeKind::Cleanup {
                        match key[0] {
                            1 => bits.range(
                                be_u64(key, 9),
                                le_u64(value, 0) & u64::from(u32::MAX),
                                root.blocks,
                            )?,
                            2 => {
                                let owner = le_u64(value, 0);
                                if be_u64(key, 1) >= root.next_job || owner >= root.next_object {
                                    return Err(bad("cleanup job exceeds allocated IDs"));
                                }
                                let job = detached(sb, state, reference(&value[16..]));
                                let mut last_end = 0;
                                job.walk(job.root_position(), &mut 0, &mut |at, page| {
                                    let Link::Disk(me) = at.link else {
                                        return Err(EIO);
                                    };
                                    bits.page(me.block, root.blocks)?;
                                    if page.level == 0 {
                                        for (key, entry) in page.records() {
                                            if be_u64(key, 0) != owner
                                                || !(value[8]..=value[9]).contains(&key[8])
                                            {
                                                return Err(bad(
                                                    "detached tree owner or kind mismatch",
                                                ));
                                            }
                                            if key[8] == tree::EXTENT {
                                                let logical = be_u64(key, 9);
                                                let count = le_u64(entry, 8) & u64::from(u32::MAX);
                                                if logical < last_end {
                                                    return Err(bad("detached extents overlap"));
                                                }
                                                last_end = logical + count;
                                                bits.extent(le_u64(entry, 0), count, root.blocks)?;
                                            } else if key[8] == tree::ENTRY {
                                                crate::names::validate_disk(
                                                    &entry[12..],
                                                    &key[9..],
                                                )?;
                                            }
                                        }
                                    }
                                    Ok(())
                                })?;
                            }
                            3 => {
                                let owner = be_u64(key, 1);
                                if owner <= 1
                                    || owner >= root.next_object
                                    || record(sb, state, owner)?[1] & 1 == 0
                                {
                                    return Err(bad("invalid orphan marker"));
                                }
                            }
                            _ => return Err(bad("unknown cleanup record")),
                        }
                    }
                }
                Ok(())
            })?;
        }
        total += bits.count;
        let allocation = Tree::new(sb, state, TreeKind::Allocation);
        // Every owned block needs an explicit allocation bit; implicit groups
        // contain only reserved blocks, which ownership never marks.
        allocation.scan(&[], &mut |key, value| {
            let first = be_u64(key, 0) * format::ALLOCATION_GROUP_BLOCKS;
            for block in first.max(start).max(512)
                ..(first + format::ALLOCATION_GROUP_BLOCKS)
                    .min(end)
                    .min(root.blocks - 512)
            {
                let set = value[((block - first) / 8) as usize] & (1 << (block % 8)) != 0;
                if set != bits.holds(block) {
                    return Err(bad("allocation bitmap disagrees with ownership"));
                }
                if set {
                    let bit = (block - start) as usize;
                    bits.pages[bit / 32768][bit % 32768 / 8] &= !(1 << (bit % 8));
                }
            }
            Ok(true)
        })?;
        if bits
            .pages
            .iter()
            .any(|page| page.iter().any(|&byte| byte != 0))
        {
            return Err(bad("owned block is free in an implicit group"));
        }
        start = end;
    }
    let free = root
        .blocks
        .checked_sub(1024)
        .and_then(|n| n.checked_sub(total))
        .and_then(|n| n.checked_add(state.slots.len() as u64))
        .ok_or_else(|| bad("ownership exceeds volume"))?;
    if free != root.free_blocks {
        return Err(bad("root free count disagrees with ownership"));
    }
    Ok(())
}

struct Object {
    id: u64,
    inode: KVec<u8>,
    children: u64,
    blocks: u64,
    end: u64,
}
impl Object {
    fn finish(&self) -> Result {
        if self.children != le_u64(&self.inode, 32) || self.blocks != le_u64(&self.inode, 24) {
            return Err(bad("inode child or allocation count mismatch"));
        }
        if self.end != le_u64(&self.inode, 16).div_ceil(4096) && self.end % 16 != 0 {
            return Err(bad("backed tail is not unit aligned"));
        }
        Ok(())
    }
}
fn catalog(sb: *mut super_block, state: &State) -> Result {
    let catalog = Tree::catalog(sb, state);
    let mut object: Option<Object> = None;
    // The last parent whose chain reached the root: its other children need
    // no second walk. A directory's objects mostly follow one another.
    let mut rooted = 1;
    catalog.scan(&[], &mut |key, value| {
        let owner = be_u64(key, 0);
        if owner >= state.root.next_object {
            return Err(bad("object exceeds allocated IDs"));
        }
        if object.as_ref().is_none_or(|current| current.id != owner) {
            if let Some(old) = &object {
                old.finish()?;
            }
            if key[8] != tree::INODE {
                return Err(bad("object has no inode"));
            }
            // The scan is on the inode record, whose key is the bare
            // prefix: no lookup.
            if key.len() != 9 {
                return Err(bad("missing inode record"));
            }
            let mut inode = KVec::new();
            tree::append(&mut inode, value)?;
            if format::validate_inode_features(
                key,
                &inode,
                state.root.features,
                format::Validation::Disk,
            )
            .is_err()
            {
                return Err(bad("inode mode or attributes without the root feature"));
            }
            let mut orphan = [3; 9];
            orphan[1..].copy_from_slice(&owner.to_be_bytes());
            let marker = Tree::new(sb, state, TreeKind::Cleanup)
                .get(&orphan, |_| Ok(()))?
                .is_some();
            if marker != (inode[1] & 1 != 0) {
                return Err(bad("orphan marker mismatch"));
            }
            if owner == 1 {
                if inode[0] != 2 || marker || le_u64(&inode, 8) != 1 {
                    return Err(bad("invalid root directory"));
                }
            } else if !marker {
                let mut seen = KVec::new();
                let direct = le_u64(&inode, 8);
                let mut parent = direct;
                while parent != 1 && parent != rooted {
                    if seen.contains(&parent) {
                        return Err(bad("directory parent cycle"));
                    }
                    seen.push(parent, GFP_KERNEL)?;
                    let ancestor = record(sb, state, parent)?;
                    if ancestor[0] != 2 || ancestor[1] & 1 != 0 {
                        return Err(bad("invalid parent directory"));
                    }
                    parent = le_u64(&ancestor, 8);
                }
                rooted = direct;
            }
            object = Some(Object {
                id: owner,
                inode,
                children: 0,
                blocks: 0,
                end: 0,
            });
        }
        let current = object.as_mut().ok_or(EIO)?;
        let size = le_u64(&current.inode, 16);
        match key[8] {
            tree::INODE => (),
            tree::ENTRY => {
                if current.inode[0] != 2 {
                    return Err(bad("non-directory has entries"));
                }
                crate::names::validate_disk(&value[12..], &key[9..])?;
                let target = le_u64(value, 0);
                if target <= 1 || target >= state.root.next_object {
                    return Err(bad("invalid entry target"));
                }
                let inode = record(sb, state, target)?;
                if inode[0] != value[8] || inode[1] & 1 != 0 || le_u64(&inode, 8) != owner {
                    return Err(bad("directory target type or parent mismatch"));
                }
                current.children += 1;
            }
            tree::EXTENT => {
                if current.inode[0] != 1 {
                    return Err(bad("non-file has extents"));
                }
                let logical = be_u64(key, 9);
                let count = le_u64(value, 8) & u64::from(u32::MAX);
                if logical < current.end
                    || logical + count > size.div_ceil(4096)
                    || (logical > current.end && (current.end % 16 != 0 || logical % 16 != 0))
                {
                    return Err(bad("extent overlap, gap inside unit, or EOF overrun"));
                }
                current.end = logical + count;
                current.blocks += count;
                // One lookup for the extent's units in each checksum group.
                let last = (logical + count).div_ceil(16);
                let mut unit = logical / 16;
                while unit < last {
                    let next = ((unit / 256 + 1) * 256).min(last);
                    let key = tree::numbered_key(owner, tree::CHECKSUMS, unit / 256);
                    let present = catalog.get(&key, |group| {
                        for unit in unit..next {
                            if checksum_slot(group, (unit % 256) as usize)
                                .map_err(crate::errno)?
                                .is_none()
                            {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    })?;
                    if present != Some(true) {
                        return Err(bad("backed range has no checksum"));
                    }
                    unit = next;
                }
            }
            tree::CHECKSUMS => {
                if current.inode[0] != 1 {
                    return Err(bad("non-file has checksums"));
                }
                // The last extent found, as its first block and its end:
                // it backs every block up to its end without another lookup.
                let mut held = (0, 0);
                for slot in 0..256 {
                    if checksum_slot(value, slot).map_err(crate::errno)?.is_none() {
                        continue;
                    }
                    let unit = be_u64(key, 9) * 256 + slot as u64;
                    let first = unit * 16;
                    let end = (first + 16).min(size.div_ceil(4096));
                    if first >= end {
                        return Err(bad("checksum past EOF"));
                    }
                    let mut block = first;
                    while block < end {
                        if !(held.0..held.1).contains(&block) {
                            held = catalog
                                .floor(
                                    &tree::numbered_key(owner, tree::EXTENT, block),
                                    |key, extent| {
                                        let start = be_u64(key, 9);
                                        let count = le_u64(extent, 8) & u64::from(u32::MAX);
                                        Ok((key.starts_with(&tree::prefix(owner, tree::EXTENT))
                                            && start <= block
                                            && start + count > block)
                                            .then_some((start, start + count)))
                                    },
                                )?
                                .flatten()
                                .ok_or_else(|| bad("checksum refers to a hole"))?;
                        }
                        block = held.1;
                    }
                }
            }
            _ => return Err(bad("unknown catalog kind")),
        }
        Ok(true)
    })?;
    object
        .as_ref()
        .ok_or_else(|| bad("catalog is empty"))?
        .finish()?;
    if record(sb, state, 1)?[0] != 2 {
        return Err(bad("root is not a directory"));
    }
    Ok(())
}

fn links(sb: *mut super_block, state: &State) -> Result {
    let mut start = 0;
    while start < state.root.next_object {
        let end = (start + WINDOW).min(state.root.next_object);
        let mut bits = Bits::new(start, end)?;
        let mut linked = 0;
        let mut next = state.root.next_object;
        Tree::catalog(sb, state).scan(&[], &mut |key, value| {
            let owner = be_u64(key, 0);
            if key[8] == tree::INODE {
                if owner >= end {
                    next = next.min(owner);
                }
                if owner > 1 && owner >= start && owner < end && value[1] & 1 == 0 {
                    linked += 1;
                }
            } else if key[8] == tree::ENTRY {
                bits.mark(le_u64(value, 0))?;
            }
            Ok(true)
        })?;
        if bits.count != linked {
            return Err(bad("linked inode does not have exactly one entry"));
        }
        if next == state.root.next_object {
            break;
        }
        start = next / WINDOW * WINDOW;
    }
    Ok(())
}
