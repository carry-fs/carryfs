//! Regular-file objects and verified data access. Data lives in 64 KiB units
//! of 4 KiB blocks, mapped by extent records and covered by packed checksum
//! groups exactly as FORMAT.md section 5 specifies.

use crate::allocation::{GROUP, Transaction};
use crate::format::{
    BLOCK_BYTES, CHECKSUM_GROUP_BYTES, Page, TreeKind, UNIT_BYTES, all_zero, checksum_slot,
    data_unit_crc, set_checksum_slot,
};
use crate::name::Name;
use crate::tree::{Edge, Forest, u64_be, u64_le};
use crate::{BlockDevice, Error, Result};

use super::namespace::{Inode, inode_key};
use super::{Access, Handle, ObjectId, ObjectKind, Timestamp, Volume};

/// Largest prefix one `read` or `write` call transfers.
pub(super) const MAX_TRANSFER_BYTES: usize = 1 << 20;
const BLOCKS_PER_UNIT: u64 = (UNIT_BYTES / BLOCK_BYTES) as u64;
const UNITS_PER_GROUP: u64 = 256;

pub(super) fn extent_key(object: u64, logical_block: u64) -> Vec<u8> {
    let mut key = object.to_be_bytes().to_vec();
    key.push(3);
    key.extend_from_slice(&logical_block.to_be_bytes());
    key
}

pub(super) fn group_key(object: u64, group: u64) -> Vec<u8> {
    let mut key = object.to_be_bytes().to_vec();
    key.push(4);
    key.extend_from_slice(&group.to_be_bytes());
    key
}

/// A decoded extent: logical start block, physical start block, block count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Extent {
    pub(super) logical: u64,
    pub(super) physical: u64,
    pub(super) count: u64,
}

impl Extent {
    /// Decode `record` if it is an extent of `object`.
    fn of(object: u64, (key, value): &(Vec<u8>, Vec<u8>)) -> Result<Option<Self>> {
        if key.len() != 17 || key[8] != 3 || u64_be(&key[..8])? != object {
            return Ok(None);
        }
        let count = u64::from(u32::from_le_bytes(
            value
                .get(8..12)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(Error::Corrupt("extent value length"))?,
        ));
        Ok(Some(Self {
            logical: u64_be(&key[9..])?,
            physical: u64_le(
                value
                    .get(..8)
                    .ok_or(Error::Corrupt("extent value length"))?,
            )?,
            count,
        }))
    }

    fn end(&self) -> Result<u64> {
        self.logical
            .checked_add(self.count)
            .ok_or(Error::Corrupt("extent end overflow"))
    }

    /// The device offset where the extent's contiguous run ends: a hint for
    /// the device, so an extent no device could hold saturates and the read
    /// of its blocks decides.
    fn device_end(&self) -> u64 {
        self.physical
            .saturating_add(self.count)
            .saturating_mul(BLOCK_BYTES as u64)
    }

    /// The extent of `object` covering `logical`, if any.
    pub(super) fn covering<D: BlockDevice>(
        forest: &mut Forest<'_, D>,
        catalog: &Edge,
        object: u64,
        logical: u64,
    ) -> Result<Option<Self>> {
        let Some(record) =
            forest.floor(catalog, TreeKind::Catalog, &extent_key(object, logical))?
        else {
            return Ok(None);
        };
        match Self::of(object, &record)? {
            Some(extent) if extent.end()? > logical => Ok(Some(extent)),
            _ => Ok(None),
        }
    }
}

fn encode_extent(physical: u64, count: u64) -> Result<Vec<u8>> {
    let mut value = physical.to_le_bytes().to_vec();
    value.extend_from_slice(
        &u32::try_from(count)
            .map_err(|_| Error::LimitExceeded)?
            .to_le_bytes(),
    );
    value.extend_from_slice(&[0; 4]);
    Ok(value)
}

/// The extent covering `logical` and those following it that start before
/// block `until`, for a caller that walks a file in order. One descent and one
/// ranged scan then serve many units.
fn extents_from<D: BlockDevice>(
    forest: &mut Forest<'_, D>,
    catalog: &Edge,
    object: u64,
    logical: u64,
    until: u64,
) -> Result<Vec<Extent>> {
    const AHEAD: usize = 64;
    let mut extents: Vec<_> = Extent::covering(forest, catalog, object, logical)?
        .into_iter()
        .collect();
    let next = match extents.last() {
        Some(extent) => extent.end()?,
        None => logical,
    };
    // Extent keys end below the object's other records, so even an `until` of
    // `u64::MAX` copies no record of the files after a small one.
    for record in forest.scan_before(
        catalog,
        TreeKind::Catalog,
        &extent_key(object, next),
        Some(&extent_key(object, until)),
        AHEAD,
    )? {
        match Extent::of(object, &record)? {
            Some(extent) => extents.push(extent),
            None => break,
        }
    }
    Ok(extents)
}

/// Extents of `object` overlapping the unit whose first block is `first`.
fn unit_extents<D: BlockDevice>(
    forest: &mut Forest<'_, D>,
    catalog: &Edge,
    object: u64,
    first: u64,
) -> Result<Vec<Extent>> {
    let mut extents: Vec<_> = Extent::covering(forest, catalog, object, first)?
        .into_iter()
        .collect();
    let limit = BLOCKS_PER_UNIT as usize;
    for record in forest.scan_before(
        catalog,
        TreeKind::Catalog,
        &extent_key(object, first + 1),
        Some(&extent_key(object, first + BLOCKS_PER_UNIT)),
        limit,
    )? {
        match Extent::of(object, &record)? {
            Some(extent) if extent.logical < first + BLOCKS_PER_UNIT => extents.push(extent),
            _ => break,
        }
    }
    Ok(extents)
}

fn put<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    key: &[u8],
    value: &[u8],
) -> Result<()> {
    let catalog = transaction.catalog.clone();
    transaction.catalog = transaction
        .forest
        .put(catalog, TreeKind::Catalog, key, value)?;
    Ok(())
}

fn remove<D: BlockDevice>(transaction: &mut Transaction<'_, D>, key: &[u8]) -> Result<()> {
    let catalog = transaction.catalog.clone();
    transaction.catalog = transaction.forest.remove(catalog, TreeKind::Catalog, key)?;
    Ok(())
}

/// One accepted write: the byte range and where the file ends before and after.
struct Change<'a> {
    object: u64,
    offset: u64,
    bytes: &'a [u8],
    old_size: u64,
    new_size: u64,
}

impl Change<'_> {
    fn end(&self) -> u64 {
        self.offset + self.bytes.len() as u64
    }

    /// Units this write must rewrite: those it touches, plus the old partial
    /// tail when EOF grows past it (its valid length and checksum change).
    fn units(&self) -> Vec<u64> {
        let unit = UNIT_BYTES as u64;
        let mut units: Vec<u64> = (self.offset / unit..=(self.end() - 1) / unit).collect();
        let tail = self.old_size / unit;
        if self.new_size > self.old_size
            && !self.old_size.is_multiple_of(unit)
            && !units.contains(&tail)
        {
            units.push(tail);
            units.sort_unstable();
        }
        units
    }
}

/// A unit's mapping is rebuilt as at most this many runs. A unit that would
/// need more is replaced whole instead, which also defragments it.
const MAX_UNIT_RUNS: usize = 4;

/// Which of the unit's blocks keep their existing storage. A block qualifies
/// only when this call read and verified the old unit, the block lies entirely
/// inside the valid length of both the old and the new contents, the change
/// does not touch it, and it is mapped. Keeping them makes an aligned 4 KiB
/// overwrite write one block instead of sixteen. Blocks at or past `count` are
/// never kept.
fn retained_blocks(
    change: &Change<'_>,
    start: u64,
    new_valid: usize,
    old_valid: usize,
    verified: bool,
    old_physical: &[Option<u64>],
    count: u64,
) -> [bool; BLOCKS_PER_UNIT as usize] {
    let mut keep = [false; BLOCKS_PER_UNIT as usize];
    if !verified {
        return keep;
    }
    // Bytes the change rewrites, relative to the unit. A change is one
    // contiguous range, so untouched blocks form a prefix and a suffix.
    let dirty = if change.offset < start + new_valid as u64 && change.end() > start {
        let from = (change.offset.max(start) - start) as usize;
        let to = (change.end().min(start + new_valid as u64) - start) as usize;
        from..to
    } else {
        0..0
    };
    // A block spanning either valid length changes with the length itself.
    let settled = old_valid.min(new_valid);
    for (index, kept) in keep[..count as usize].iter_mut().enumerate() {
        let (block_start, block_end) = (index * BLOCK_BYTES, (index + 1) * BLOCK_BYTES);
        *kept = block_end <= settled
            && (dirty.start >= block_end || dirty.end <= block_start)
            && old_physical[index].is_some();
    }
    if runs(&keep, old_physical, count) > MAX_UNIT_RUNS {
        keep.fill(false);
    }
    keep
}

/// Contiguous runs the unit's mapping would need. Retained blocks keep their
/// own addresses; the rest come from one fresh allocation, so counting a
/// boundary wherever a retained block meets a replaced one bounds the result.
fn runs(keep: &[bool], old_physical: &[Option<u64>], count: u64) -> usize {
    let mut total = 0;
    for index in 0..count as usize {
        let continues = index > 0
            && match (keep[index - 1], keep[index]) {
                (true, true) => match (old_physical[index - 1], old_physical[index]) {
                    (Some(previous), Some(current)) => current == previous + 1,
                    _ => false,
                },
                (false, false) => true,
                _ => false,
            };
        if !continues {
            total += 1;
        }
    }
    total
}

/// Copy-on-write one unit, returning its new checksum, or None for an unchanged
/// hole. The unit's new mapping is appended to `mapped` instead of recorded.
fn rewrite_unit<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    change: &Change<'_>,
    inode: &mut Inode,
    unit: u64,
    hint: &mut u64,
    mapped: &mut Vec<Extent>,
) -> Result<Option<u32>> {
    let start = unit * UNIT_BYTES as u64;
    let new_valid = (change.new_size - start).min(UNIT_BYTES as u64) as usize;
    let old_valid = change.old_size.saturating_sub(start).min(UNIT_BYTES as u64) as usize;
    let touches = change.offset < start + UNIT_BYTES as u64 && change.end() > start;
    // Replacing every old valid byte needs no read, so damaged contents with a
    // valid mapping can still be overwritten (FORMAT.md section 5).
    let replaced = touches && change.offset <= start && change.end() >= start + old_valid as u64;
    let catalog = transaction.catalog.clone();
    // Only bytes this call verified may stay where they are.
    let mut verified = false;
    let mut assembled = Vec::new();
    // A change covering the whole new unit already holds its bytes, so the
    // sequential case copies nothing here.
    let data: &[u8] = if replaced && change.end() >= start + new_valid as u64 {
        let from = (start - change.offset) as usize;
        &change.bytes[from..from + new_valid]
    } else {
        assembled.resize(new_valid, 0);
        if old_valid > 0 && !replaced {
            match read_unit(
                &mut transaction.forest,
                &catalog,
                change.object,
                change.old_size,
                unit,
            )? {
                Some(old) => {
                    let keep = old_valid.min(new_valid);
                    assembled[..keep].copy_from_slice(&old[..keep]);
                    verified = true;
                }
                None if !touches => return Ok(None),
                None => {}
            }
        }
        if touches {
            let from = change.offset.max(start);
            let to = change.end().min(start + new_valid as u64);
            assembled[(from - start) as usize..(to - start) as usize].copy_from_slice(
                &change.bytes[(from - change.offset) as usize..(to - change.offset) as usize],
            );
        }
        &assembled
    };

    let first = unit * BLOCKS_PER_UNIT;
    let unit_end = first + BLOCKS_PER_UNIT;
    let blocks = BLOCKS_PER_UNIT as usize;
    let mut old_physical = [None; BLOCKS_PER_UNIT as usize];
    // A unit wholly past the old EOF cannot have mapped blocks.
    let old_extents = if old_valid == 0 {
        Vec::new()
    } else {
        unit_extents(&mut transaction.forest, &catalog, change.object, first)?
    };
    for extent in old_extents {
        let extent_end = extent.end()?;
        remove(transaction, &extent_key(change.object, extent.logical))?;
        if extent.logical < first {
            let value = encode_extent(extent.physical, first - extent.logical)?;
            put(
                transaction,
                &extent_key(change.object, extent.logical),
                &value,
            )?;
        }
        if extent_end > unit_end {
            let value = encode_extent(
                extent.physical + (unit_end - extent.logical),
                extent_end - unit_end,
            )?;
            put(transaction, &extent_key(change.object, unit_end), &value)?;
        }
        for logical in extent.logical.max(first)..extent_end.min(unit_end) {
            old_physical[(logical - first) as usize] =
                Some(extent.physical + (logical - extent.logical));
        }
    }

    let count = new_valid.div_ceil(BLOCK_BYTES) as u64;
    let keep = retained_blocks(
        change,
        start,
        new_valid,
        old_valid,
        verified,
        &old_physical,
        count,
    );
    let mut physical = [0u64; BLOCKS_PER_UNIT as usize];
    let mut allocated = 0;
    let wanted = count - keep.iter().filter(|kept| **kept).count() as u64;
    let mut runs = transaction.allocate_data(wanted, *hint)?.into_iter();
    let mut run = (0u64, 0u64);
    for index in 0..count as usize {
        if keep[index] {
            physical[index] =
                old_physical[index].ok_or(Error::Corrupt("retained block is unmapped"))?;
            continue;
        }
        if run.1 == 0 {
            run = runs.next().ok_or(Error::Corrupt("allocation is short"))?;
        }
        let at = index * BLOCK_BYTES;
        let take = (new_valid - at).min(BLOCK_BYTES);
        transaction.write_data(run.0, Page::from_prefix(&data[at..at + take]))?;
        physical[index] = run.0;
        *hint = run.0 / GROUP;
        run = (run.0 + 1, run.1 - 1);
        allocated += 1;
    }

    // The unit's new mapping, one run per contiguous stretch; the caller
    // merges and records them.
    for (index, &block) in physical[..count as usize].iter().enumerate() {
        match mapped.last_mut() {
            Some(run)
                if run.end()? == first + index as u64
                    && run.physical.checked_add(run.count) == Some(block) =>
            {
                run.count += 1;
            }
            _ => mapped.push(Extent {
                logical: first + index as u64,
                physical: block,
                count: 1,
            }),
        }
    }

    // Retire the old blocks this unit no longer references, in runs.
    let mut retired = 0;
    let mut index = 0;
    while index < blocks {
        let superseded = |at: usize| old_physical[at].filter(|_| !keep[at]);
        let Some(start_block) = superseded(index) else {
            index += 1;
            continue;
        };
        let mut end = index + 1;
        while end < blocks && superseded(end) == Some(start_block + (end - index) as u64) {
            end += 1;
        }
        transaction.retire_data(start_block, (end - index) as u64);
        retired += (end - index) as u64;
        index = end;
    }

    inode.allocated_blocks = inode
        .allocated_blocks
        .checked_sub(retired)
        .and_then(|blocks| blocks.checked_add(allocated))
        .ok_or(Error::Corrupt(
            "inode allocation count disagrees with extents",
        ))?;

    data_unit_crc(&transaction.forest.base.uuid, change.object, unit, data).map(Some)
}

/// Apply each checksum group once. Units are distinct and sorted, so reading
/// an old unit never needs a checksum produced earlier in this operation.
/// New mappings are recorded last, coalesced with each other and with the
/// adjacent records they continue, so a sequential file keeps one extent per
/// physical run instead of one per unit.
fn rewrite_units<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    change: &Change<'_>,
    inode: &mut Inode,
    units: &[u64],
    hint: &mut u64,
) -> Result<()> {
    // A unit's lookups never see another unit's pending runs: runs cover only
    // their own unit, and splits of shared old extents are recorded at once.
    let mut mapped = Vec::new();
    for units in units.chunk_by(|a, b| a / UNITS_PER_GROUP == b / UNITS_PER_GROUP) {
        let key = group_key(change.object, units[0] / UNITS_PER_GROUP);
        let mut group = [0; CHECKSUM_GROUP_BYTES];
        if let Some(existing) =
            transaction
                .forest
                .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
        {
            group = crate::format::expand_group(&existing);
        }
        let mut changed = false;
        for &unit in units {
            if let Some(crc) = rewrite_unit(transaction, change, inode, unit, hint, &mut mapped)? {
                set_checksum_slot(&mut group, (unit % UNITS_PER_GROUP) as usize, Some(crc));
                changed = true;
            }
        }
        if changed {
            put(transaction, &key, crate::format::compact_group(&group))?;
        }
    }
    // Neighbors come from disk, so their arithmetic is checked.
    let joins = |left: &Extent, right: &Extent| {
        left.logical.checked_add(left.count) == Some(right.logical)
            && left.physical.checked_add(left.count) == Some(right.physical)
            && left.count + right.count <= u64::from(u32::MAX)
    };
    let mut merged: Vec<Extent> = Vec::with_capacity(mapped.len());
    for run in mapped {
        match merged.last_mut() {
            Some(last) if joins(last, &run) => last.count += run.count,
            _ => merged.push(run),
        }
    }
    for mut run in merged {
        let catalog = transaction.catalog.clone();
        if let Some(next) = transaction.forest.lookup(
            &catalog,
            TreeKind::Catalog,
            &extent_key(change.object, run.end()?),
        )? {
            let next = Extent::of(
                change.object,
                &(extent_key(change.object, run.end()?), next),
            )?
            .ok_or(Error::Corrupt("extent record is malformed"))?;
            if joins(&run, &next) {
                remove(transaction, &extent_key(change.object, next.logical))?;
                run.count += next.count;
            }
        }
        if let Some(previous) = run.logical.checked_sub(1)
            && let Some(record) = transaction.forest.floor(
                &catalog,
                TreeKind::Catalog,
                &extent_key(change.object, previous),
            )?
            && let Some(previous) = Extent::of(change.object, &record)?
            && joins(&previous, &run)
        {
            run = Extent {
                count: previous.count + run.count,
                ..previous
            };
        }
        put(
            transaction,
            &extent_key(change.object, run.logical),
            &encode_extent(run.physical, run.count)?,
        )?;
    }
    Ok(())
}

/// Detach every extent and checksum slot of `object` past `size`. The
/// retained partial unit, if any, has already been rewritten.
fn shrink<D: BlockDevice>(
    transaction: &mut Transaction<'_, D>,
    object: u64,
    inode: &mut Inode,
    size: u64,
) -> Result<()> {
    let first_discarded = size.div_ceil(UNIT_BYTES as u64);
    let cut = first_discarded * BLOCKS_PER_UNIT;
    let catalog = transaction.catalog.clone();
    if let Some(extent) = Extent::covering(&mut transaction.forest, &catalog, object, cut)?
        && extent.logical < cut
    {
        let end = extent.end()?;
        put(
            transaction,
            &extent_key(object, extent.logical),
            &encode_extent(extent.physical, cut - extent.logical)?,
        )?;
        put(
            transaction,
            &extent_key(object, cut),
            &encode_extent(extent.physical + (cut - extent.logical), end - cut)?,
        )?;
    }
    let mut kind_end = object.to_be_bytes().to_vec();
    kind_end.push(4);
    let detached = transaction.detach_job(object, &extent_key(object, cut), &kind_end, (3, 3))?;
    inode.allocated_blocks = inode
        .allocated_blocks
        .checked_sub(detached)
        .ok_or(Error::Corrupt(
            "inode allocation count disagrees with extents",
        ))?;

    let mut whole_groups = first_discarded.div_ceil(UNITS_PER_GROUP);
    let partial = first_discarded / UNITS_PER_GROUP;
    let key = group_key(object, partial);
    if !first_discarded.is_multiple_of(UNITS_PER_GROUP)
        && let Some(existing) =
            transaction
                .forest
                .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
    {
        let mut group = crate::format::expand_group(&existing);
        for slot in (first_discarded % UNITS_PER_GROUP) as usize..UNITS_PER_GROUP as usize {
            set_checksum_slot(&mut group, slot, None);
        }
        if group[..32].iter().all(|byte| *byte == 0) {
            remove(transaction, &key)?;
        } else {
            put(transaction, &key, crate::format::compact_group(&group))?;
        }
    } else {
        whole_groups = partial;
    }
    let mut object_end = object.to_be_bytes().to_vec();
    object_end.push(5);
    transaction.detach_job(
        object,
        &group_key(object, whole_groups),
        &object_end,
        (4, 4),
    )?;
    Ok(())
}

/// Distinguishes damaged/unreadable payload from metadata that cannot be trusted.
pub(super) enum UnitError {
    Metadata(Error),
    Data(Error),
}

impl From<Error> for UnitError {
    fn from(error: Error) -> Self {
        Self::Metadata(error)
    }
}

impl UnitError {
    pub(super) fn into_error(self) -> Error {
        match self {
            Self::Metadata(error) | Self::Data(error) => error,
        }
    }
}

/// Verified valid bytes of one data unit, or `None` for a hole. `size` is the
/// file size the unit's checksum was computed against.
pub(super) fn read_unit<D: BlockDevice>(
    forest: &mut Forest<'_, D>,
    catalog: &Edge,
    object: u64,
    size: u64,
    unit: u64,
) -> Result<Option<Vec<u8>>> {
    inspect_unit(forest, catalog, object, size, unit, None, None).map_err(UnitError::into_error)
}

/// A contiguous read ending within this many blocks of its extent's end
/// names the extent after it, so a device that reads ahead has the file's
/// next run requested before the stream reaches the gap: 16 MiB.
const NEXT_RUN_BLOCKS: u64 = 4096;

/// Read an aligned request covered by one extent, verifying the original
/// checksum units. Fragmented and sparse requests use `inspect_unit` instead.
/// `size` is the file size.
fn read_contiguous<D: BlockDevice>(
    forest: &mut Forest<'_, D>,
    catalog: &Edge,
    object: u64,
    size: u64,
    offset: u64,
    out: &mut [u8],
) -> Result<bool> {
    if !offset.is_multiple_of(UNIT_BYTES as u64)
        || !out.len().is_multiple_of(UNIT_BYTES)
        || out.len() < 2 * UNIT_BYTES
    {
        return Ok(false);
    }
    let first = offset / BLOCK_BYTES as u64;
    let blocks = (out.len() / BLOCK_BYTES) as u64;
    let Some(extent) = Extent::covering(forest, catalog, object, first)? else {
        return Ok(false);
    };
    let end = extent.end()?;
    if end < first + blocks {
        return Ok(false);
    }
    let continues = end.saturating_mul(BLOCK_BYTES as u64) < size;
    let next_run = if continues && end - (first + blocks) <= NEXT_RUN_BLOCKS {
        // A hint never fails the read: damage past this extent is the error
        // of the read that reaches it.
        Extent::covering(forest, catalog, object, end)
            .ok()
            .flatten()
            .and_then(|next| next.physical.checked_add(end.checked_sub(next.logical)?))
            .and_then(|block| block.checked_mul(BLOCK_BYTES as u64))
    } else {
        None
    };
    let physical = extent
        .physical
        .checked_add(first - extent.logical)
        .ok_or(Error::Corrupt("extent physical overflow"))?;
    let device_offset = physical
        .checked_mul(BLOCK_BYTES as u64)
        .ok_or(Error::Corrupt("data block offset overflow"))?;
    forest.read_data(device_offset, out, extent.device_end(), next_run)?;
    let first_unit = offset / UNIT_BYTES as u64;
    let mut group = None;
    for (index, unit_bytes) in out.chunks(UNIT_BYTES).enumerate() {
        let unit = first_unit + index as u64;
        if index == 0 || unit.is_multiple_of(UNITS_PER_GROUP) {
            group = forest.lookup(
                catalog,
                TreeKind::Catalog,
                &group_key(object, unit / UNITS_PER_GROUP),
            )?;
        }
        let block = physical
            .checked_add(index as u64 * BLOCKS_PER_UNIT)
            .ok_or(Error::Corrupt("extent physical overflow"))?;
        let crc = group
            .as_deref()
            .map(|group| checksum_slot(group, (unit % UNITS_PER_GROUP) as usize))
            .transpose()?
            .flatten()
            .ok_or(Error::Corruption {
                block,
                reason: "backed data unit has no checksum",
            })?;
        if data_unit_crc(&forest.base.uuid, object, unit, unit_bytes)? != crc {
            return Err(Error::Corruption {
                block,
                reason: "file data checksum mismatch",
            });
        }
    }
    Ok(true)
}

/// Verify and return one data unit. `group` is the unit's checksum-group
/// record when the caller already holds it, as a scan over groups does; the
/// same record otherwise costs a catalog descent for every one of its units.
/// `held` carries the extents ahead of a caller that walks consecutive units,
/// with the block its walk ends before. Descending for every unit re-reads a
/// branch page and each of its children, which a read-only volume never
/// caches, so one forward scan serves many units, and the end keeps a short
/// read from copying extents it never reaches. A caller of a single unit
/// passes `None` and pays one descent.
pub(super) fn inspect_unit<D: BlockDevice>(
    forest: &mut Forest<'_, D>,
    catalog: &Edge,
    object: u64,
    size: u64,
    unit: u64,
    group: Option<&[u8]>,
    mut held: Option<(&mut Vec<Extent>, u64)>,
) -> std::result::Result<Option<Vec<u8>>, UnitError> {
    let start = unit
        .checked_mul(UNIT_BYTES as u64)
        .filter(|start| *start < size)
        .ok_or(Error::InvalidInput("data unit is past EOF"))?;
    let valid = (size - start).min(UNIT_BYTES as u64) as usize;
    let first = unit * BLOCKS_PER_UNIT;
    let end = first + valid.div_ceil(BLOCK_BYTES) as u64;
    let slot = (unit % UNITS_PER_GROUP) as usize;
    let crc = match group {
        Some(group) => checksum_slot(group, slot)?,
        None => match forest.lookup(
            catalog,
            TreeKind::Catalog,
            &group_key(object, unit / UNITS_PER_GROUP),
        )? {
            Some(group) => checksum_slot(&group, slot)?,
            None => None,
        },
    };
    let Some(crc) = crc else {
        let last = first + BLOCKS_PER_UNIT - 1;
        let tail = forest.floor(catalog, TreeKind::Catalog, &extent_key(object, last))?;
        if let Some(extent) = tail
            .map(|record| Extent::of(object, &record))
            .transpose()?
            .flatten()
            && extent.end()? > first
        {
            return Err(Error::Corruption {
                block: extent.physical,
                reason: "backed data unit has no checksum",
            }
            .into());
        }
        return Ok(None);
    };

    let mut bytes = vec![0; (end - first) as usize * BLOCK_BYTES];
    let mut logical = first;
    let mut first_physical = None;
    while logical < end {
        let covers = |extent: &Extent| {
            extent.logical <= logical && extent.end().is_ok_and(|end| end > logical)
        };
        let extent = match held
            .as_ref()
            .and_then(|(ahead, _)| ahead.iter().copied().find(covers))
        {
            Some(extent) => extent,
            None => match held.as_mut() {
                Some((ahead, until)) => {
                    **ahead = extents_from(forest, catalog, object, logical, *until)?;
                    ahead
                        .iter()
                        .copied()
                        .find(covers)
                        .ok_or(Error::Corrupt("checksummed data unit is not fully backed"))?
                }
                None => Extent::covering(forest, catalog, object, logical)?
                    .ok_or(Error::Corrupt("checksummed data unit is not fully backed"))?,
            },
        };
        let run_end = extent.end()?.min(end);
        // The lookup and the held extent both cover `logical`, so this cannot
        // happen; without it a wrong extent would spin instead of failing.
        if run_end <= logical {
            return Err(Error::Corrupt("extent does not advance the data unit").into());
        }
        // The run is contiguous on the device, so it is one read of at most
        // 64 KiB instead of one call per block.
        let physical = extent
            .physical
            .checked_add(logical - extent.logical)
            .ok_or(Error::Corrupt("extent physical overflow"))?;
        first_physical.get_or_insert(physical);
        let at = (logical - first) as usize * BLOCK_BYTES;
        let len = (run_end - logical) as usize * BLOCK_BYTES;
        let offset = physical
            .checked_mul(BLOCK_BYTES as u64)
            .ok_or(Error::Corrupt("data block offset overflow"))?;
        forest
            .read_data(offset, &mut bytes[at..at + len], extent.device_end(), None)
            .map_err(UnitError::Data)?;
        logical = run_end;
    }
    let block = first_physical.unwrap_or_default();
    if !all_zero(&bytes[valid..]) {
        return Err(UnitError::Data(Error::Corruption {
            block,
            reason: "nonzero data past EOF",
        }));
    }
    bytes.truncate(valid);
    if data_unit_crc(&forest.base.uuid, object, unit, &bytes)? != crc {
        return Err(UnitError::Data(Error::Corruption {
            block,
            reason: "file data checksum mismatch",
        }));
    }
    Ok(Some(bytes))
}

impl<D: BlockDevice> Volume<D> {
    /// Create an empty regular file under `parent` and open it read-write.
    pub fn create_file(&mut self, parent: ObjectId, name: Name) -> Result<Handle> {
        if !self.handles.has_room() {
            return Err(Error::LimitExceeded);
        }
        let object = self.create(parent, name, ObjectKind::File)?;
        self.handles.open(object, Access::ReadWrite)
    }

    /// Open the regular file `name` under `parent`.
    pub fn open_file(&mut self, parent: ObjectId, name: &Name, access: Access) -> Result<Handle> {
        self.readable()?;
        if access == Access::ReadWrite {
            self.writable()?;
        }
        let object = self.lookup(parent, name)?;
        if self.attributes(object)?.kind != ObjectKind::File {
            return Err(Error::WrongType);
        }
        self.handles.open(object, access)
    }

    /// Write a prefix of `bytes` at `offset` and publish it. Returns the number
    /// of bytes accepted: all of them up to 1 MiB, or 0 for empty input, which
    /// changes nothing. Growing the file past its end leaves holes.
    pub fn write(&mut self, file: &Handle, offset: u64, bytes: &[u8]) -> Result<usize> {
        let (object, access) = self.handles.validate_access(file)?;
        self.writable()?;
        if access != Access::ReadWrite {
            return Err(Error::ReadOnly);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let bytes = &bytes[..bytes.len().min(MAX_TRANSFER_BYTES)];
        offset
            .checked_add(bytes.len() as u64)
            .filter(|end| *end <= i64::MAX as u64)
            .ok_or(Error::LimitExceeded)?;
        let now = Timestamp::now()?;
        let mut hint = self.data_hint;
        self.edit(|transaction| {
            let key = inode_key(object.0);
            let value = transaction
                .forest
                .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::Corrupt("handle target inode is missing"))?;
            let mut inode = Inode::decode(&value)?;
            if inode.kind != ObjectKind::File {
                return Err(Error::WrongType);
            }
            let change = Change {
                object: object.0,
                offset,
                bytes,
                old_size: inode.size,
                new_size: inode.size.max(offset + bytes.len() as u64),
            };
            rewrite_units(transaction, &change, &mut inode, &change.units(), &mut hint)?;
            inode.size = change.new_size;
            inode.modification = now;
            inode.metadata_change = now;
            put(transaction, &key, &inode.encode())
        })?;
        self.data_hint = hint;
        Ok(bytes.len())
    }

    /// Set the file size and publish it as one commit. Shrinking rewrites a
    /// retained partial unit and detaches everything past it into cleanup jobs
    /// without reading discarded data; growing leaves holes.
    pub fn truncate(&mut self, file: &Handle, size: u64) -> Result<()> {
        let (object, access) = self.handles.validate_access(file)?;
        self.writable()?;
        if access != Access::ReadWrite {
            return Err(Error::ReadOnly);
        }
        if size > i64::MAX as u64 {
            return Err(Error::LimitExceeded);
        }
        // An unchanged size changes nothing, so it publishes nothing either.
        let current = self.attributes(object)?;
        if current.kind != ObjectKind::File {
            return Err(Error::WrongType);
        }
        if current.size == size {
            return Ok(());
        }
        let now = Timestamp::now()?;
        let mut hint = self.data_hint;
        self.edit(|transaction| {
            let object = object.0;
            let key = inode_key(object);
            let value = transaction
                .forest
                .lookup(&transaction.catalog, TreeKind::Catalog, &key)?
                .ok_or(Error::Corrupt("handle target inode is missing"))?;
            let mut inode = Inode::decode(&value)?;
            if inode.kind != ObjectKind::File {
                return Err(Error::WrongType);
            }
            if size == inode.size {
                return Ok(());
            }
            let unit = UNIT_BYTES as u64;
            // The unit whose valid length changes: the new tail when shrinking,
            // the old tail when growing. A whole-unit boundary changes none.
            let boundary = size.min(inode.size);
            if !boundary.is_multiple_of(unit) {
                let tail = boundary / unit;
                let change = Change {
                    object,
                    offset: tail * unit,
                    bytes: &[],
                    old_size: inode.size,
                    new_size: size,
                };
                rewrite_units(transaction, &change, &mut inode, &[tail], &mut hint)?;
            }
            if size < inode.size {
                shrink(transaction, object, &mut inode, size)?;
            }
            inode.size = size;
            inode.modification = now;
            inode.metadata_change = now;
            put(transaction, &key, &inode.encode())
        })?;
        self.data_hint = hint;
        Ok(())
    }

    /// Read verified bytes at `offset`. Returns 0 at or past EOF, otherwise a
    /// nonzero prefix of at most 1 MiB. Holes read as zeros; damaged data is
    /// an error, never zeros.
    pub fn read(&mut self, file: &Handle, offset: u64, out: &mut [u8]) -> Result<usize> {
        let object = self.handles.validate(file)?.0;
        self.read_catalog(|forest, catalog| {
            let value = forest
                .lookup(&catalog, TreeKind::Catalog, &inode_key(object))?
                .ok_or(Error::Corrupt("handle target inode is missing"))?;
            let inode = Inode::decode(&value)?;
            if inode.kind != ObjectKind::File {
                return Err(Error::WrongType);
            }
            if offset >= inode.size || out.is_empty() {
                return Ok(0);
            }
            let len = (inode.size - offset)
                .min(out.len() as u64)
                .min(MAX_TRANSFER_BYTES as u64) as usize;
            if read_contiguous(
                forest,
                &catalog,
                object,
                inode.size,
                offset,
                &mut out[..len],
            )? {
                return Ok(len);
            }
            let mut copied = 0;
            // The units of one request are consecutive, so they share extents.
            // Each unit is verified whole, so the extents wanted end with the
            // request's last unit, not its last byte.
            let mut held = Vec::new();
            let until = (offset + len as u64).div_ceil(UNIT_BYTES as u64) * BLOCKS_PER_UNIT;
            while copied < len {
                let position = offset + copied as u64;
                let unit = position / UNIT_BYTES as u64;
                let within = (position % UNIT_BYTES as u64) as usize;
                let take = (UNIT_BYTES - within).min(len - copied);
                let target = &mut out[copied..copied + take];
                let read = inspect_unit(
                    forest,
                    &catalog,
                    object,
                    inode.size,
                    unit,
                    None,
                    Some((&mut held, until)),
                )
                .map_err(UnitError::into_error)?;
                match read {
                    Some(bytes) => target.copy_from_slice(&bytes[within..within + take]),
                    None => target.fill(0),
                }
                copied += take;
            }
            Ok(len)
        })
    }
}
