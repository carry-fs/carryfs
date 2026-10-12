//! Available metadata blocks, held outside the trees that allocate them.

use crate::format::{BLOCK_BYTES, Page, Root, TreeKind, TreeRef, decode_page, encode_node};
use crate::tree::{tree_ref, u64_be};
use crate::{BlockDevice, Error, Result};

pub(crate) const CAPACITY: usize = 12_500;
pub(crate) const INITIAL: std::ops::Range<u64> = 8192..16_384;
// Records are runs, so a leaf holds fewer of them than it holds slots. The
// bound is the fully fragmented list, which is what fits the 64-block bank.
const LEAF_RUNS: usize = 250;
pub(crate) const MAX_BANK_PAGES: usize = 1 + CAPACITY.div_ceil(LEAF_RUNS);

pub(crate) fn is_bank_root(block: u64, blocks: u64) -> bool {
    blocks >= 1024 && (block == 32 || block == blocks - 96)
}

pub(crate) fn is_bank_block(block: u64, blocks: u64) -> bool {
    blocks >= 1024 && ((32..96).contains(&block) || (blocks - 96..blocks - 32).contains(&block))
}

pub(crate) fn next_reference(root: &Root) -> Result<TreeRef> {
    Ok(TreeRef {
        block: if root.workspace.block == 32 {
            root.blocks - 96
        } else {
            32
        },
        generation: root.generation.checked_add(1).ok_or(Error::LimitExceeded)?,
    })
}

pub(crate) fn encode(root: &Root, slots: &[u64]) -> Result<Vec<(u64, Page)>> {
    if slots.len() > CAPACITY
        || !is_bank_root(root.workspace.block, root.blocks)
        || slots.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(Error::InvalidInput("invalid workspace list"));
    }
    let runs = runs(slots);
    let leaf = |reference, runs: &[Run]| {
        let records: Vec<_> = runs
            .iter()
            .map(|(first, count)| (first.as_slice(), count.as_slice()))
            .collect();
        encode_node(
            reference,
            &root.uuid,
            TreeKind::Workspace,
            &records,
            root.blocks,
            0,
            0,
        )
    };
    if runs.len() <= LEAF_RUNS {
        return Ok(vec![(root.workspace.block, leaf(root.workspace, &runs)?)]);
    }
    let mut pages = Vec::new();
    let mut children = Vec::new();
    for (index, chunk) in runs.chunks(LEAF_RUNS).enumerate() {
        let reference = TreeRef {
            block: root.workspace.block + 1 + index as u64,
            generation: root.workspace.generation,
        };
        let mut value = [0; 16];
        value[..8].copy_from_slice(&reference.block.to_le_bytes());
        value[8..].copy_from_slice(&reference.generation.to_le_bytes());
        children.push((chunk[0].0, value));
        pages.push((reference.block, leaf(reference, chunk)?));
    }
    let records: Vec<_> = children
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    pages.push((
        root.workspace.block,
        encode_node(
            root.workspace,
            &root.uuid,
            TreeKind::Workspace,
            &records,
            root.blocks,
            1,
            0,
        )?,
    ));
    Ok(pages)
}

/// A run record: the first block, big-endian, and how many consecutive slots
/// it covers. The list is sorted, so adjacent slots merge on one pass.
type Run = ([u8; 8], [u8; 4]);

fn runs(slots: &[u64]) -> Vec<Run> {
    let mut runs: Vec<(u64, u32)> = Vec::new();
    for block in slots {
        match runs.last_mut() {
            Some((first, count)) if *first + u64::from(*count) == *block => *count += 1,
            _ => runs.push((*block, 1)),
        }
    }
    runs.into_iter()
        .map(|(first, count)| (first.to_be_bytes(), count.to_le_bytes()))
        .collect()
}

pub(crate) fn read(device: &mut impl BlockDevice, root: &Root) -> Result<Vec<u64>> {
    read_with(root, |block| {
        let mut page = Page(Box::new([0; BLOCK_BYTES]));
        let offset = block
            .checked_mul(BLOCK_BYTES as u64)
            .ok_or(Error::Corrupt("workspace offset overflow"))?;
        device.read_exact_at(offset, &mut page.0[..])?;
        Ok(page)
    })
}

pub(crate) fn read_with(
    root: &Root,
    mut read: impl FnMut(u64) -> Result<Page>,
) -> Result<Vec<u64>> {
    if !is_bank_root(root.workspace.block, root.blocks) {
        return Err(Error::Corrupt("invalid workspace bank"));
    }
    let page = read(root.workspace.block)?;
    let decoded = decode_page(
        &page.0[..],
        root.workspace,
        &root.uuid,
        TreeKind::Workspace,
        root.generation,
        None,
        root.blocks,
    )
    .map_err(|error| error.at_block(root.workspace.block))?;
    let mut slots = Vec::new();
    let mut append = |records: &[(&[u8], &[u8])]| -> Result<()> {
        if records.len() > LEAF_RUNS {
            return Err(Error::Corrupt("workspace capacity exceeded"));
        }
        for (key, value) in records {
            let first = u64_be(key)?;
            let count = u32::from_le_bytes(
                (*value)
                    .try_into()
                    .map_err(|_| Error::Corrupt("workspace run length"))?,
            ) as usize;
            // Decoding validated the run itself. A gap of at least one block
            // between runs keeps the encoding of a given list unique.
            if count == 0 || count > CAPACITY - slots.len() {
                return Err(Error::Corrupt("workspace capacity exceeded"));
            }
            if slots.last().is_some_and(|last| *last + 1 >= first) {
                return Err(Error::Corrupt("workspace slots are not increasing"));
            }
            slots.extend(first..first + count as u64);
        }
        Ok(())
    };
    if decoded.level == 0 {
        append(&decoded.records)?;
    } else {
        if decoded.level != 1 || decoded.records.len() >= MAX_BANK_PAGES {
            return Err(Error::Corrupt("invalid workspace tree shape"));
        }
        for (index, (key, value)) in decoded.records.iter().enumerate() {
            let reference = tree_ref(value)?;
            if reference.block != root.workspace.block + 1 + index as u64
                || reference.generation != root.workspace.generation
            {
                return Err(Error::Corrupt("workspace child leaves selected bank"));
            }
            let leaf = read(reference.block)?;
            let child = decode_page(
                &leaf.0[..],
                reference,
                &root.uuid,
                TreeKind::Workspace,
                root.workspace.generation,
                Some(0),
                root.blocks,
            )
            .map_err(|error| error.at_block(reference.block))?;
            if child.records.first().map(|(minimum, _)| minimum) != Some(key)
                || (index + 1 < decoded.records.len() && child.records.len() != LEAF_RUNS)
            {
                return Err(Error::Corrupt("workspace child minimum or length"));
            }
            append(&child.records)?;
        }
    }
    if root.free_blocks < slots.len() as u64 {
        return Err(Error::Corrupt("workspace exceeds recorded free blocks"));
    }
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::crc32c;
    use crate::volume::model::GEOMETRY;
    use std::collections::BTreeMap;

    fn root() -> Root {
        Root::decode(
            include_bytes!("../testdata/vectors/root-a.bin"),
            256,
            GEOMETRY,
        )
        .unwrap()
    }

    fn read_pages(root: &Root, pages: &BTreeMap<u64, Page>) -> Result<Vec<u64>> {
        read_with(root, |block| {
            pages
                .get(&block)
                .cloned()
                .ok_or(Error::Corrupt("missing bank page"))
        })
    }

    fn crc(page: &mut Page) {
        page.0[64..68].fill(0);
        let crc = crc32c(&page.0[..]);
        page.0[64..68].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn bank_round_trip_covers_capacity_and_both_locations() {
        for bank in [32, GEOMETRY.blocks() - 96] {
            // Every other block: the fully fragmented list, one run per slot.
            for count in [0, 1, LEAF_RUNS, LEAF_RUNS + 1, CAPACITY] {
                let mut root = root();
                root.workspace.block = bank;
                let slots: Vec<_> = (0..count as u64).map(|index| 1024 + 2 * index).collect();
                let pages: BTreeMap<_, _> = encode(&root, &slots).unwrap().into_iter().collect();
                assert!(pages.len() <= MAX_BANK_PAGES);
                assert_eq!(read_pages(&root, &pages).unwrap(), slots);
            }
        }
        // The list the planner actually keeps is contiguous, and a contiguous
        // list costs one page instead of twenty-six.
        let root = root();
        let slots: Vec<_> = (8192..8192 + 8192).collect();
        let pages: BTreeMap<_, _> = encode(&root, &slots).unwrap().into_iter().collect();
        assert_eq!(pages.len(), 1);
        assert_eq!(read_pages(&root, &pages).unwrap(), slots);
        for slots in [
            vec![512, 512],
            vec![513, 512],
            vec![32],
            vec![root.blocks - 512],
            vec![512; CAPACITY + 1],
        ] {
            assert!(encode(&root, &slots).is_err());
        }
    }

    #[test]
    fn tiny_bank_checks_identity_and_old_generation() {
        let mut root = root();
        let pages: BTreeMap<_, _> = encode(&root, &[8192, 9000, 9001])
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            &pages[&32].0[..],
            &include_bytes!("../testdata/vectors/workspace-leaf.bin")[..]
        );
        root.generation += 1;
        assert_eq!(read_pages(&root, &pages).unwrap(), [8192, 9000, 9001]);
        root.workspace.generation += 1;
        assert!(read_pages(&root, &pages).is_err());
        root.workspace.generation -= 1;
        root.free_blocks = 1;
        assert!(read_pages(&root, &pages).is_err());
    }

    #[test]
    fn bank_reader_rejects_valid_crc_corruption_before_following_bad_references() {
        let root = root();
        // One run per slot, just over a leaf, so the bank has a level-1 root.
        let slots: Vec<_> = (0..=LEAF_RUNS as u64)
            .map(|index| 1024 + 2 * index)
            .collect();
        let pages: BTreeMap<_, _> = encode(&root, &slots).unwrap().into_iter().collect();
        for child_block in [32, 34, 95, root.blocks - 95, 512, u64::MAX] {
            let mut changed = pages.clone();
            let page = changed.get_mut(&32).unwrap();
            page.0[92..100].copy_from_slice(&child_block.to_le_bytes());
            crc(page);
            let mut reads = Vec::new();
            assert!(
                read_with(&root, |block| {
                    reads.push(block);
                    Ok(changed
                        .get(&block)
                        .cloned()
                        .unwrap_or(Page(Box::new([0; BLOCK_BYTES]))))
                })
                .is_err()
            );
            assert_eq!(reads, [32], "followed invalid child {child_block}");
        }
        for (block, offset, bytes) in [
            (33, 84, 32u64.to_be_bytes()),
            (34, 84, 1024u64.to_be_bytes()),
            (34, 24, 6u64.to_le_bytes()),
        ] {
            let mut changed = pages.clone();
            let page = changed.get_mut(&block).unwrap();
            page.0[offset..offset + 8].copy_from_slice(&bytes);
            crc(page);
            assert!(read_pages(&root, &changed).is_err());
        }
        // A run that overlaps its neighbour, and one that leaves the volume.
        for (block, count) in [(33u64, 3u32), (34, 1 << 30)] {
            let mut changed = pages.clone();
            let page = changed.get_mut(&block).unwrap();
            page.0[92..96].copy_from_slice(&count.to_le_bytes());
            crc(page);
            assert!(read_pages(&root, &changed).is_err());
        }
    }
}
