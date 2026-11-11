// SPDX-License-Identifier: GPL-2.0-only OR Apache-2.0

//! Publication (FORMAT.md section 3): sealing the open transaction onto
//! workspace slots and writing it with the two-root sequence, and opening a
//! volume for writing.

use kernel::bindings::super_block;
use kernel::prelude::*;

use crate::edit::{self, Txn};
use crate::glue;
use crate::ondisk::format::{self, Root, TreeKind, TreeRef, ALLOCATION_GROUP_BLOCKS, BLOCK_BYTES};
use crate::tree::{be_u64, le_u64, Block, Link};
use crate::volume::{Locked, State};
use crate::{corrupt, errno};

/// The workspace planner of FORMAT.md section 6.
const TARGET: usize = 8_192;
const BATCH: usize = 128;
const MIN_SLOTS: usize = 4_096;
const CAPACITY: usize = 12_500;
const LEAF_RUNS: usize = 250;
/// Replacement metadata one publication may write.
const MAX_PAGES: usize = 4_096;
/// Open drafts after which the next operation commits first, so that no
/// transaction outgrows `MAX_PAGES` with what commit adds.
pub(crate) const COMMIT_DRAFTS: usize = 3_072;

fn root_b(root: &Root) -> u64 {
    root.blocks - 257
}

/// Write `pages` to `blocks` and wait.
fn write(sb: *mut super_block, blocks: &[u64], pages: &[*const u8]) -> Result {
    // SAFETY: Each pointer refers to a full page the caller keeps alive.
    let status = unsafe {
        glue::carryfs_write_blocks(sb, blocks.as_ptr(), pages.as_ptr(), blocks.len() as u32)
    };
    if status != 0 {
        pr_err!("metadata write failed\n");
        return Err(EIO);
    }
    Ok(())
}

fn flush(sb: *mut super_block) -> Result {
    // SAFETY: `sb` holds its block device.
    if unsafe { glue::carryfs_flush(sb) } != 0 {
        pr_err!("cache flush failed\n");
        return Err(EIO);
    }
    Ok(())
}

/// Write `root` into its slot at `block` and flush it.
fn write_root(sb: *mut super_block, root: &Root, block: u64) -> Result {
    let mut page = KBox::new([0; BLOCK_BYTES], GFP_KERNEL)?;
    root.encode(block, root.blocks, &mut page).map_err(errno)?;
    write(sb, &[block], &[page.as_ptr()])?;
    flush(sb)
}

/// Prepare a writable mount: read the committed workspace list and finish
/// any interrupted mirroring, before any allocation.
pub(crate) fn open(sb: *mut super_block, state: &mut State, decoded: [Option<Root>; 2]) -> Result {
    state.slots = read_slots(sb, &state.root)?;
    crate::check::validate(sb, state)?;
    let root = state.root;
    for (block, existing) in [(256, decoded[0]), (root_b(&root), decoded[1])] {
        if existing != Some(root) {
            write_root(sb, &root, block)?;
        }
    }
    Ok(())
}

/// Objects left orphaned when the volume was last in use: unlinked while
/// open. A writable mount removes them.
pub(crate) fn orphans(sb: *mut super_block, state: &State) -> Result<KVec<u64>> {
    let mut found = KVec::new();
    crate::tree::Tree::new(sb, state, TreeKind::Cleanup).scan(&[3], &mut |key, _| {
        if key.first() != Some(&3) {
            return Ok(false);
        }
        found.push(be_u64(key, 1), GFP_KERNEL)?;
        Ok(true)
    })?;
    Ok(found)
}

/// Commit the open transaction, if any, from the periodic timer.
#[no_mangle]
unsafe extern "C" fn carryfs_periodic_commit(sb: *mut super_block) {
    let volume = crate::volume::of(sb);
    // Each pass may reclaim one batch; continue while more remains.
    for _ in 0..16 {
        let mut state = volume.lock();
        if state.faulted || (state.txn.is_none() && !state.reclaim_pending) {
            return;
        }
        // A failure faults the volume and was logged.
        if commit(sb, &mut state).is_err() {
            return;
        }
    }
}

/// The workspace list of `root` (FORMAT.md section 6, "Protected metadata
/// workspace").
fn read_slots(sb: *mut super_block, root: &Root) -> Result<KVec<u64>> {
    let mut slots = KVec::with_capacity(CAPACITY, GFP_KERNEL)?;
    let bank = Block::read(sb, root.workspace.block)?;
    let view = format::decode_page(
        bank.data(),
        root.workspace,
        &root.uuid,
        TreeKind::Workspace,
        root.generation,
        None,
        root.blocks,
    )
    .map_err(|error| corrupt(root.workspace.block, error))?;
    let bad = |reason| {
        corrupt(
            root.workspace.block,
            crate::ondisk::error::Error::Corrupt(reason),
        )
    };
    let mut append = |key: &[u8], value: &[u8]| -> Result {
        let first = be_u64(key, 0);
        let count = value
            .get(..4)
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, |bytes| u64::from(u32::from_le_bytes(bytes)));
        if slots.len() as u64 + count > CAPACITY as u64
            || slots.last().is_some_and(|last| last + 1 >= first)
        {
            return Err(bad("workspace runs overlap or exceed capacity"));
        }
        for block in first..first + count {
            slots.push(block, GFP_KERNEL)?;
        }
        Ok(())
    };
    if view.level == 0 {
        if view.len() > LEAF_RUNS {
            return Err(bad("workspace leaf exceeds capacity"));
        }
        for (key, value) in view.records() {
            append(key, value)?;
        }
    } else {
        if view.level != 1 || view.len() > CAPACITY.div_ceil(LEAF_RUNS) {
            return Err(bad("invalid workspace tree shape"));
        }
        for (index, (minimum, value)) in view.records().enumerate() {
            let reference = TreeRef {
                block: le_u64(value, 0),
                generation: le_u64(value, 8),
            };
            if reference.block != root.workspace.block + 1 + index as u64
                || reference.generation != root.workspace.generation
            {
                return Err(bad("workspace child leaves its bank"));
            }
            let leaf = Block::read(sb, reference.block)?;
            let leaf_view = format::decode_page(
                leaf.data(),
                reference,
                &root.uuid,
                TreeKind::Workspace,
                root.workspace.generation,
                Some(0),
                root.blocks,
            )
            .map_err(|error| corrupt(reference.block, error))?;
            if leaf_view.records().next().is_none_or(|(key, _)| key != minimum)
                || leaf_view.len() > LEAF_RUNS
                || (index + 1 < view.len() && leaf_view.len() != LEAF_RUNS)
            {
                return Err(bad("workspace child minimum or length mismatch"));
            }
            for (key, value) in leaf_view.records() {
                append(key, value)?;
            }
        }
    }
    if root.free_blocks < slots.len() as u64 {
        return Err(bad("workspace exceeds recorded free blocks"));
    }
    Ok(slots)
}

/// Publish the open transaction, if any. A failure faults the volume:
/// FORMAT.md section 3 forbids retrying a publication.
pub(crate) fn commit(sb: *mut super_block, state: &mut Locked<'_>) -> Result {
    state.quiesce();
    let result = commit_quiesced(sb, state);
    state.end_quiesce();
    result
}

fn commit_quiesced(sb: *mut super_block, state: &mut State) -> Result {
    if state.faulted {
        return Err(EIO);
    }
    let txn = match state.txn.take() {
        Some(txn) => txn,
        // A commit may run for reclamation alone.
        None if state.reclaim_pending => Txn::new(&state.root, state.hint)?,
        None => return Ok(()),
    };
    match publish(sb, state, txn) {
        Ok(()) => Ok(()),
        Err(error) => {
            pr_err!("publication failed; the volume is read-only until remounted\n");
            crate::volume::fault(sb, state);
            Err(error)
        }
    }
}

/// Post-order of the live drafts under `link`: children before parents.
fn post_order(
    txn: &Txn,
    link: Link,
    kind: TreeKind,
    order: &mut KVec<(usize, TreeKind)>,
) -> Result {
    let Link::Draft(id) = link else {
        return Ok(());
    };
    let view = format::draft_page_view(&txn.drafts[id].page[..]).map_err(errno)?;
    if view.level > 0 {
        for (_, value) in view.records() {
            post_order(txn, Link::of(value), kind, order)?;
        }
    }
    order.push((id, kind), GFP_KERNEL)?;
    Ok(())
}

fn publish(sb: *mut super_block, state: &mut State, mut txn: Txn) -> Result {
    let base = state.root;
    let generation = txn.generation;
    let disk = edit::Disk {
        sb,
        uuid: base.uuid,
        blocks: base.blocks,
        generation: base.generation,
    };

    let more = txn.reclaim(&disk, base.cleanup)?;
    state.reclaim_pending = more || !txn.retired.is_empty();
    if txn.edits == 0 && txn.data_allocated == 0 {
        // Nothing to publish: no change and nothing reclaimable.
        return Ok(());
    }

    // Workspace balance: release surplus base slots, or refill from raw free
    // blocks, setting their allocation bits.
    let surplus = state.slots.len() + txn.superseded.len();
    let mut release: KVec<u64> = KVec::new();
    let mut refill: KVec<u64> = KVec::new();
    let mut cursor = base.workspace_cursor;
    if surplus > TARGET + BATCH {
        for block in state.slots.iter().rev().take(BATCH) {
            release.push(*block, GFP_KERNEL)?;
        }
        txn.set_allocation(&disk, &release, false)?;
    } else if surplus < TARGET - BATCH {
        cursor = txn.refill(&disk, &state.slots, cursor, BATCH, &mut refill)?;
        txn.set_allocation(&disk, &refill, true)?;
    }
    txn.flush_space(&disk)?;

    // Every live draft gets a base slot that is neither released nor
    // already holding one of this transaction's data blocks.
    let mut order = KVec::new();
    for kind in [TreeKind::Catalog, TreeKind::Allocation, TreeKind::Cleanup] {
        post_order(&txn, txn.root(kind), kind, &mut order)?;
    }
    if order.len() > MAX_PAGES {
        return Err(ENOSPC);
    }
    let usable = state.slots.len() - release.len();
    if order.len() > usable {
        pr_err!(
            "transaction needs {} pages but has {usable} workspace slots\n",
            order.len()
        );
        return Err(ENOSPC);
    }
    let mut assigned = KVec::with_capacity(txn.drafts.len(), GFP_KERNEL)?;
    assigned.resize(txn.drafts.len(), 0u64, GFP_KERNEL)?;
    for (slot, (id, _)) in state.slots.iter().zip(order.iter()) {
        assigned[*id] = *slot;
    }
    let reference = |link: Link| match link {
        Link::Draft(id) => TreeRef {
            block: assigned[id],
            generation,
        },
        Link::Disk(reference) => reference,
    };

    // Seal each draft, children first, and check it as a reader would.
    for (id, kind) in order.iter().copied() {
        let page = &mut txn.drafts[id].page;
        let view_level = page[14];
        if view_level > 0 {
            let records = usize::from(u16::from_le_bytes([page[32], page[33]]));
            let mut offset = format::PAYLOAD_OFFSET;
            for _ in 0..records {
                let key = usize::from(u16::from_le_bytes([page[offset], page[offset + 1]]));
                let value_at = offset + 4 + key;
                let child = Link::of(&page[value_at..value_at + 16]);
                let sealed = reference(child);
                page[value_at..value_at + 8].copy_from_slice(&sealed.block.to_le_bytes());
                page[value_at + 8..value_at + 16].copy_from_slice(&sealed.generation.to_le_bytes());
                offset = value_at + 16;
            }
        }
        let total = if kind == TreeKind::Catalog {
            edit::total(page)
        } else {
            0
        };
        let me = TreeRef {
            block: assigned[id],
            generation,
        };
        format::seal_page(page, me, &base.uuid, kind, total);
        format::decode_page(
            &page[..],
            me,
            &base.uuid,
            kind,
            generation,
            None,
            base.blocks,
        )
        .map_err(|error| {
            pr_err!("built an invalid page: {:?}\n", error);
            errno(error)
        })?;
    }

    // The successor workspace list: the slots neither assigned nor released,
    // which are in order and about 12,500, merged with the few this
    // transaction adds. Sorting the whole list took 0.6 ms of each commit.
    let mut added = KVec::with_capacity(txn.superseded.len() + refill.len(), GFP_KERNEL)?;
    added.extend_from_slice(&txn.superseded, GFP_KERNEL)?;
    added.extend_from_slice(&refill, GFP_KERNEL)?;
    heapsort(&mut added);
    let mut successor = KVec::with_capacity(CAPACITY + MAX_PAGES, GFP_KERNEL)?;
    let mut next = 0;
    // `release` holds the last slots.
    for block in &state.slots[order.len()..usable] {
        while next < added.len() && added[next] < *block {
            successor.push(added[next], GFP_KERNEL)?;
            next += 1;
        }
        successor.push(*block, GFP_KERNEL)?;
    }
    successor.extend_from_slice(&added[next..], GFP_KERNEL)?;
    if !(MIN_SLOTS..=CAPACITY).contains(&successor.len()) {
        pr_err!("workspace list would hold {} slots\n", successor.len());
        return Err(ENOSPC);
    }
    let changed = successor[..] != state.slots[..];
    let workspace = if changed {
        TreeRef {
            block: if base.workspace.block == 32 {
                base.blocks - 96
            } else {
                32
            },
            generation,
        }
    } else {
        base.workspace
    };
    let mut bank: KVec<(u64, KBox<[u8; BLOCK_BYTES]>)> = KVec::new();
    if changed {
        encode_bank(&base, workspace, &successor, &mut bank)?;
    }

    // Drafts leave the free count as they take slots; superseded pages
    // rejoin it as slots.
    let free = (base.free_blocks + txn.reclaimed + txn.superseded.len() as u64)
        .checked_sub(txn.data_allocated + order.len() as u64)
        .ok_or(ENOSPC)?;
    let root = Root {
        label: base.label,
        uuid: base.uuid,
        generation,
        blocks: base.blocks,
        catalog: reference(txn.root(TreeKind::Catalog)),
        allocation: reference(txn.root(TreeKind::Allocation)),
        cleanup: reference(txn.root(TreeKind::Cleanup)),
        next_object: txn.next_object,
        free_blocks: free,
        next_job: base.next_job,
        workspace,
        workspace_cursor: cursor,
    };

    // Data reached the device as writeback finished; one flush then covers
    // it and the metadata below.
    let mut blocks = KVec::with_capacity(order.len() + bank.len(), GFP_KERNEL)?;
    let mut pages: KVec<*const u8> =
        KVec::with_capacity(order.len() + bank.len(), GFP_KERNEL)?;
    for (id, _) in order.iter() {
        blocks.push(assigned[*id], GFP_KERNEL)?;
        pages.push(txn.drafts[*id].page.as_ptr(), GFP_KERNEL)?;
    }
    for (block, page) in bank.iter() {
        blocks.push(*block, GFP_KERNEL)?;
        pages.push(page.as_ptr(), GFP_KERNEL)?;
    }
    write(sb, &blocks, &pages)?;
    flush(sb)?;
    write_root(sb, &root, 256)?;
    write_root(sb, &root, root_b(&root))?;

    state.root = root;
    state.slots = successor;
    state.hint = txn.hint;
    Ok(())
}

/// Sort in place. `slice::sort_unstable` has a panic path objtool in Linux
/// 7.0 does not know is noreturn.
pub(crate) fn heapsort(values: &mut [u64]) {
    fn sift(values: &mut [u64], mut root: usize, end: usize) {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                return;
            }
            if child + 1 < end && values[child] < values[child + 1] {
                child += 1;
            }
            if values[root] >= values[child] {
                return;
            }
            values.swap(root, child);
            root = child;
        }
    }
    let len = values.len();
    for root in (0..len / 2).rev() {
        sift(values, root, len);
    }
    for end in (1..len).rev() {
        values.swap(0, end);
        sift(values, 0, end);
    }
}

/// Encode `slots` into the bank at `workspace`: one leaf of runs, or a
/// level-1 root over leaves of 250 runs at the following blocks.
fn encode_bank(
    base: &Root,
    workspace: TreeRef,
    slots: &[u64],
    pages: &mut KVec<(u64, KBox<[u8; BLOCK_BYTES]>)>,
) -> Result {
    let mut runs: KVec<(u64, u32)> = KVec::new();
    for block in slots {
        match runs.last_mut() {
            Some((first, count)) if *first + u64::from(*count) == *block => *count += 1,
            _ => runs.push((*block, 1), GFP_KERNEL)?,
        }
    }
    let leaf = |reference: TreeRef, runs: &[(u64, u32)]| -> Result<KBox<[u8; BLOCK_BYTES]>> {
        let mut page = KBox::new([0; BLOCK_BYTES], GFP_KERNEL)?;
        for (first, count) in runs {
            edit::append(&mut page, &first.to_be_bytes(), &count.to_le_bytes());
        }
        format::seal_page(&mut page, reference, &base.uuid, TreeKind::Workspace, 0);
        Ok(page)
    };
    if runs.len() <= LEAF_RUNS {
        pages.push((workspace.block, leaf(workspace, &runs)?), GFP_KERNEL)?;
    } else {
        let mut root = KBox::new([0; BLOCK_BYTES], GFP_KERNEL)?;
        root[14] = 1;
        for (index, chunk) in runs.chunks(LEAF_RUNS).enumerate() {
            let reference = TreeRef {
                block: workspace.block + 1 + index as u64,
                generation: workspace.generation,
            };
            let mut value = [0u8; 16];
            value[..8].copy_from_slice(&reference.block.to_le_bytes());
            value[8..].copy_from_slice(&reference.generation.to_le_bytes());
            edit::append(&mut root, &chunk[0].0.to_be_bytes(), &value);
            pages.push((reference.block, leaf(reference, chunk)?), GFP_KERNEL)?;
        }
        format::seal_page(&mut root, workspace, &base.uuid, TreeKind::Workspace, 0);
        pages.push((workspace.block, root), GFP_KERNEL)?;
    }
    for (block, page) in pages.iter() {
        let level = if *block == workspace.block {
            None
        } else {
            Some(0)
        };
        let reference = TreeRef {
            block: *block,
            generation: workspace.generation,
        };
        format::decode_page(
            &page[..],
            reference,
            &base.uuid,
            TreeKind::Workspace,
            workspace.generation,
            level,
            base.blocks,
        )
        .map_err(errno)?;
    }
    Ok(())
}

/// Bits for blocks in `group` that are never allocatable.
pub(crate) fn implicit_bitmap(group: u64, blocks: u64, bitmap: &mut [u8; 2048]) {
    bitmap.fill(0);
    for block in format::reserved_group_blocks(group, blocks) {
        let bit = (block % ALLOCATION_GROUP_BLOCKS) as usize;
        bitmap[bit / 8] |= 1 << (bit % 8);
    }
}
