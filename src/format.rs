use std::str;

use crate::device::Geometry;
use crate::error::{Error, Result};

pub(crate) const BLOCK_BYTES: usize = 4096;

/// Format revision of roots and every page (FORMAT.md). Any other revision is
/// rejected as unsupported.
pub(crate) const REVISION: u16 = 1;
pub(crate) const ROOT_MAGIC: &[u8; 8] = b"CFSXROOT";
const PAGE_MAGIC: &[u8; 8] = b"CFSXPAGE";
const ALLOCATION_BYTES: u32 = 4096;
const CHECKSUM_UNIT_BYTES: u32 = 65_536;
const RESERVED_BLOCKS: u64 = 512;
const RESERVED_TOTAL_BLOCKS: u64 = RESERVED_BLOCKS * 2;
const MAX_ID: u64 = i64::MAX as u64;
const MAX_FILE_BYTES: u64 = i64::MAX as u64;
const MAX_LOGICAL_BLOCK: u64 = (MAX_FILE_BYTES / 4096) + 1;
const MAX_CHECKSUM_GROUP: u64 = MAX_FILE_BYTES / (256 * 65_536);
const ALLOCATION_GROUP_BLOCKS: u64 = 16_384;
const ALLOCATION_GROUP_BYTES: usize = 2_048;
pub(crate) const CHECKSUM_GROUP_BYTES: usize = 1_056;
const PAGE_PAYLOAD_OFFSET: usize = 80;
const PAGE_PAYLOAD_BYTES: usize = BLOCK_BYTES - PAGE_PAYLOAD_OFFSET;
const MAX_RECORD_BYTES: usize = 3_072;
const MAINTENANCE_RESERVE: u64 = 16_384;
/// Longest symlink target, macOS `MAXPATHLEN` less its NUL, so a link made on
/// any adapter reads on all of them.
pub(crate) const MAX_SYMLINK_TARGET: usize = 1_023;

// Deliberately not #[repr(align(4096))]: that alignment corrupted memory in
// optimized x86-64 Linux builds. Direct I/O backends must allocate their own
// aligned buffers.
#[derive(Clone, Debug, PartialEq, Eq)]
// Boxed so a queued or published page moves as a pointer, not 4 KiB.
pub(crate) struct Page(pub Box<[u8; BLOCK_BYTES]>);

impl Page {
    /// A page holding `bytes`, at most one page, followed by zeros. A full
    /// page is copied once, without zeroing first.
    pub(crate) fn from_prefix(bytes: &[u8]) -> Self {
        match <Box<[u8; BLOCK_BYTES]>>::try_from(Box::<[u8]>::from(bytes)) {
            Ok(page) => Self(page),
            Err(bytes) => {
                let mut page = Self(Box::new([0; BLOCK_BYTES]));
                page.0[..bytes.len()].copy_from_slice(&bytes);
                page
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TreeRef {
    pub block: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    pub label: Option<crate::VolumeLabel>,
    pub uuid: [u8; 16],
    pub generation: u64,
    pub blocks: u64,
    pub catalog: TreeRef,
    pub allocation: TreeRef,
    pub cleanup: TreeRef,
    pub next_object: u64,
    pub free_blocks: u64,
    pub next_job: u64,
    pub workspace: TreeRef,
    pub workspace_cursor: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TreeKind {
    Catalog = 1,
    Allocation = 2,
    Cleanup = 3,
    Workspace = 4,
}

pub(crate) struct DecodedPage<'a> {
    pub level: u8,
    pub data_blocks: u64,
    pub records: Vec<(&'a [u8], &'a [u8])>,
}

#[derive(Clone, Copy)]
pub(crate) enum Validation {
    Input,
    Disk,
}

type PageRecord<'a> = (&'a [u8], &'a [u8]);

#[derive(Clone, Copy)]
struct RecordValidation {
    kind: TreeKind,
    page_kind: u8,
    page_generation: u64,
    blocks: u64,
    mode: Validation,
}

fn violation(mode: Validation, reason: &'static str) -> Error {
    match mode {
        Validation::Input => Error::InvalidInput(reason),
        Validation::Disk => Error::Corrupt(reason),
    }
}

pub(crate) fn validate_timestamp_range(seconds: i64, nanos: u32, mode: Validation) -> Result<()> {
    if !(-11_644_473_600..=253_402_300_799).contains(&seconds) {
        return Err(violation(mode, "timestamp seconds out of range"));
    }
    if nanos > 999_999_999 {
        return Err(violation(mode, "timestamp nanoseconds out of range"));
    }
    Ok(())
}

// Slicing-by-8: CRC_TABLES[k][b] is the CRC of byte `b` followed by `k` zero bytes.
const CRC_TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0u32; 256]; 8];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0x82F6_3B78 } else { 0 };
            bit += 1;
        }
        tables[0][byte] = crc;
        byte += 1;
    }
    let mut table = 1;
    while table < 8 {
        let mut byte = 0;
        while byte < 256 {
            let previous = tables[table - 1][byte];
            tables[table][byte] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            byte += 1;
        }
        table += 1;
    }
    tables
};

/// CRC32C through the ARMv8 CRC instructions when this processor has them. The
/// result matches `software_extend`, which every other target uses.
#[cfg(target_arch = "aarch64")]
mod hardware {
    use std::sync::atomic::{AtomicU8, Ordering};

    const UNKNOWN: u8 = 2;
    static SUPPORTED: AtomicU8 = AtomicU8::new(UNKNOWN);

    pub(super) fn available() -> bool {
        match SUPPORTED.load(Ordering::Relaxed) {
            0 => false,
            1 => true,
            _ => {
                // Miri does not run the intrinsics, so it takes the software path.
                let detected = !cfg!(miri) && std::arch::is_aarch64_feature_detected!("crc");
                SUPPORTED.store(u8::from(detected), Ordering::Relaxed);
                detected
            }
        }
    }

    /// # Safety
    ///
    /// The processor must support the CRC extension, which `available` reports.
    #[target_feature(enable = "crc")]
    pub(super) unsafe fn extend(crc: u32, bytes: &[u8]) -> u32 {
        use std::arch::aarch64::{__crc32cb, __crc32cd};

        /// Bytes per stream, 2^8, so `zero_shifts` levels 8 and 9 shift past
        /// one and two lanes.
        const LANE: usize = 256;
        const STRIDE: usize = 3 * LANE;

        let mut crc = crc;
        // Three independent streams hide `crc32cx` latency, as on x86-64:
        // continuing A, B, C from `s` equals
        // shift(A from s, 2 lanes) ^ shift(B from 0, 1 lane) ^ (C from 0).
        let (strides, tail) = bytes.as_chunks::<STRIDE>();
        if !strides.is_empty() {
            let shifts = super::zero_shifts();
            for stride in strides {
                let (lanes, _) = stride.as_chunks::<LANE>();
                let (a, _) = lanes[0].as_chunks::<8>();
                let (b, _) = lanes[1].as_chunks::<8>();
                let (c, _) = lanes[2].as_chunks::<8>();
                let (mut x, mut y, mut z) = (crc, 0, 0);
                // The intrinsics are safe to call where the target feature is enabled.
                for index in 0..LANE / 8 {
                    x = __crc32cd(x, u64::from_le_bytes(a[index]));
                    y = __crc32cd(y, u64::from_le_bytes(b[index]));
                    z = __crc32cd(z, u64::from_le_bytes(c[index]));
                }
                crc = super::apply_shift(&shifts[9], x) ^ super::apply_shift(&shifts[8], y) ^ z;
            }
        }
        let (chunks, rest) = tail.as_chunks::<8>();
        for chunk in chunks {
            crc = __crc32cd(crc, u64::from_le_bytes(*chunk));
        }
        for &byte in rest {
            crc = __crc32cb(crc, byte);
        }
        crc
    }
}

/// CRC32C through the SSE4.2 instructions when this processor has them. The
/// result matches `software_extend`, which every other target uses.
#[cfg(target_arch = "x86_64")]
mod hardware {
    use std::sync::atomic::{AtomicU8, Ordering};

    const UNKNOWN: u8 = 2;
    static SUPPORTED: AtomicU8 = AtomicU8::new(UNKNOWN);

    pub(super) fn available() -> bool {
        match SUPPORTED.load(Ordering::Relaxed) {
            0 => false,
            1 => true,
            _ => {
                // Miri does not run the intrinsics, so it takes the software path.
                let detected = !cfg!(miri) && is_x86_feature_detected!("sse4.2");
                SUPPORTED.store(u8::from(detected), Ordering::Relaxed);
                detected
            }
        }
    }

    /// Bytes per stream of the three-way interleave below.
    const LANE: usize = 256;
    const STRIDE: usize = 3 * LANE;

    /// The CRC register after `bytes` zero bytes, as a linear map of the
    /// register before them: one table per register byte.
    struct Shift([[u32; 256]; 4]);

    impl Shift {
        fn new(bytes: usize) -> Self {
            let zeros = &super::ZEROS[..bytes];
            let basis: Vec<u32> = (0..32)
                .map(|bit| super::software_extend(1 << bit, zeros))
                .collect();
            let mut tables = [[0u32; 256]; 4];
            for (index, table) in tables.iter_mut().enumerate() {
                for (value, entry) in table.iter_mut().enumerate() {
                    *entry = (0..8)
                        .filter(|bit| value >> bit & 1 == 1)
                        .fold(0, |acc, bit| acc ^ basis[index * 8 + bit]);
                }
            }
            Self(tables)
        }

        fn apply(&self, crc: u32) -> u32 {
            let [a, b, c, d] = crc.to_le_bytes();
            self.0[0][usize::from(a)]
                ^ self.0[1][usize::from(b)]
                ^ self.0[2][usize::from(c)]
                ^ self.0[3][usize::from(d)]
        }
    }

    /// Shifts past one and two lanes.
    fn shifts() -> &'static (Shift, Shift) {
        static SHIFTS: std::sync::OnceLock<(Shift, Shift)> = std::sync::OnceLock::new();
        SHIFTS.get_or_init(|| (Shift::new(LANE), Shift::new(2 * LANE)))
    }

    /// # Safety
    ///
    /// The processor must support SSE4.2, which `available` reports.
    #[target_feature(enable = "sse4.2")]
    pub(super) unsafe fn extend(crc: u32, bytes: &[u8]) -> u32 {
        use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

        let mut wide = u64::from(crc);
        // One `crc32` takes three cycles but a new one can start every cycle,
        // so three independent streams run about three times as fast as one
        // chain. The register is linear: continuing A, B, C from `s` equals
        // shift(A from s, 2 lanes) ^ shift(B from 0, 1 lane) ^ (C from 0).
        let (strides, tail) = bytes.as_chunks::<STRIDE>();
        if !strides.is_empty() {
            let (one, two) = shifts();
            for stride in strides {
                let (lanes, _) = stride.as_chunks::<LANE>();
                let (a, _) = lanes[0].as_chunks::<8>();
                let (b, _) = lanes[1].as_chunks::<8>();
                let (c, _) = lanes[2].as_chunks::<8>();
                let (mut x, mut y, mut z) = (wide, 0, 0);
                // The intrinsics are safe to call where the target feature is enabled.
                for index in 0..LANE / 8 {
                    x = _mm_crc32_u64(x, u64::from_le_bytes(a[index]));
                    y = _mm_crc32_u64(y, u64::from_le_bytes(b[index]));
                    z = _mm_crc32_u64(z, u64::from_le_bytes(c[index]));
                }
                wide = u64::from(two.apply(x as u32) ^ one.apply(y as u32) ^ z as u32);
            }
        }
        let (chunks, rest) = tail.as_chunks::<8>();
        for chunk in chunks {
            wide = _mm_crc32_u64(wide, u64::from_le_bytes(*chunk));
        }
        let mut crc = wide as u32;
        for &byte in rest {
            crc = _mm_crc32_u8(crc, byte);
        }
        crc
    }
}

fn crc32c_extend(crc: u32, bytes: &[u8]) -> u32 {
    #[cfg(target_arch = "aarch64")]
    if hardware::available() {
        // SAFETY: `available` reported CRC support on this processor.
        return unsafe { hardware::extend(crc, bytes) };
    }
    #[cfg(target_arch = "x86_64")]
    if hardware::available() {
        // SAFETY: `available` reported SSE4.2 support on this processor.
        return unsafe { hardware::extend(crc, bytes) };
    }
    software_extend(crc, bytes)
}

fn software_extend(mut crc: u32, bytes: &[u8]) -> u32 {
    let t = &CRC_TABLES;
    let (chunks, rest) = bytes.as_chunks::<8>();
    for chunk in chunks {
        let low = crc ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        crc = t[7][(low & 0xFF) as usize]
            ^ t[6][(low >> 8 & 0xFF) as usize]
            ^ t[5][(low >> 16 & 0xFF) as usize]
            ^ t[4][(low >> 24) as usize]
            ^ t[3][usize::from(chunk[4])]
            ^ t[2][usize::from(chunk[5])]
            ^ t[1][usize::from(chunk[6])]
            ^ t[0][usize::from(chunk[7])];
    }
    rest.iter().fold(crc, |crc, &byte| {
        (crc >> 8) ^ t[0][((crc ^ u32::from(byte)) & 0xFF) as usize]
    })
}

/// CRC32C in the reflected wire format used by CarryFS.
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    crc32c_extend(0xFFFF_FFFF, bytes) ^ 0xFFFF_FFFF
}

pub(crate) const UNIT_BYTES: usize = CHECKSUM_UNIT_BYTES as usize;

static ZEROS: [u8; UNIT_BYTES] = [0; UNIT_BYTES];

/// CRC32C of one backed data unit. `valid` holds its valid bytes; the rest of
/// the 64 KiB unit contributes zeros, as FORMAT.md section 5 specifies.
pub(crate) fn data_unit_crc(uuid: &[u8; 16], object: u64, unit: u64, valid: &[u8]) -> Result<u32> {
    let valid_len = u32::try_from(valid.len())
        .ok()
        .filter(|len| (1..=CHECKSUM_UNIT_BYTES).contains(len))
        .ok_or(Error::InvalidInput("data unit valid length"))?;
    let mut crc = crc32c_extend(0xFFFF_FFFF, b"CFSXDATA");
    for field in [
        &uuid[..],
        &object.to_le_bytes(),
        &unit.to_le_bytes(),
        &valid_len.to_le_bytes(),
        valid,
    ] {
        crc = crc32c_extend(crc, field);
    }
    crc = crc32c_zeros(crc, UNIT_BYTES - valid.len());
    Ok(crc ^ 0xFFFF_FFFF)
}

/// The register-to-register maps of 1, 2, 4, ... 65,536 zero bytes, one
/// lookup table per register byte. CRC32C without the final inversion is
/// linear in its register over zero input, so each map is the XOR of its
/// images of the 32 register bits.
fn zero_shifts() -> &'static [[[u32; 256]; 4]; 17] {
    static SHIFTS: std::sync::OnceLock<Box<[[[u32; 256]; 4]; 17]>> = std::sync::OnceLock::new();
    SHIFTS.get_or_init(|| {
        let mut shifts = Box::new([[[0u32; 256]; 4]; 17]);
        let mut basis: [u32; 32] = std::array::from_fn(|bit| software_extend(1 << bit, &[0]));
        for level in 0..17 {
            for (index, table) in shifts[level].iter_mut().enumerate() {
                for (value, entry) in table.iter_mut().enumerate() {
                    *entry = (0..8)
                        .filter(|bit| value >> bit & 1 == 1)
                        .fold(0, |acc, bit| acc ^ basis[index * 8 + bit]);
                }
            }
            // Twice this level's length: apply its map twice.
            let table = &shifts[level];
            basis = basis.map(|image| apply_shift(table, image));
        }
        shifts
    })
}

fn apply_shift(table: &[[u32; 256]; 4], crc: u32) -> u32 {
    let [a, b, c, d] = crc.to_le_bytes();
    table[0][usize::from(a)]
        ^ table[1][usize::from(b)]
        ^ table[2][usize::from(c)]
        ^ table[3][usize::from(d)]
}

/// `crc32c_extend(crc, &[0; len])` without reading any zeros: a 4 KiB write
/// otherwise checksums the 60 KiB of zeros that pad its unit.
fn crc32c_zeros(crc: u32, len: usize) -> u32 {
    if len > UNIT_BYTES {
        return crc32c_extend(crc, &vec![0; len]);
    }
    zero_shifts()
        .iter()
        .enumerate()
        .filter(|(bit, _)| len >> bit & 1 == 1)
        .fold(crc, |crc, (_, table)| apply_shift(table, crc))
}

/// The CRC stored in checksum-group `slot`, or `None` for a hole. The group
/// must be a validated record; slots after its last present one are not stored.
pub(crate) fn checksum_slot(group: &[u8], slot: usize) -> Result<Option<u32>> {
    if slot >= 256 || group.len() < 32 {
        return Err(Error::Corrupt("checksum group slot"));
    }
    if group[slot / 8] & (1 << (slot % 8)) == 0 {
        return Ok(None);
    }
    let bytes = group
        .get(32 + slot * 4..36 + slot * 4)
        .ok_or(Error::Corrupt("checksum group slot"))?;
    Ok(Some(u32::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ])))
}

/// The stored form of `group`: its bitmap and the slots through the last
/// present one (FORMAT.md, "Packed checksum group"). Callers remove a group
/// with no present slot instead of storing it.
pub(crate) fn compact_group(group: &[u8; CHECKSUM_GROUP_BYTES]) -> &[u8] {
    let stored = group[..32]
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |index| {
            index * 8 + 8 - group[index].leading_zeros() as usize
        });
    &group[..32 + 4 * stored]
}

/// A stored group padded back to the full 1,056-byte form that writes edit.
pub(crate) fn expand_group(value: &[u8]) -> [u8; CHECKSUM_GROUP_BYTES] {
    let mut group = [0; CHECKSUM_GROUP_BYTES];
    let len = value.len().min(CHECKSUM_GROUP_BYTES);
    group[..len].copy_from_slice(&value[..len]);
    group
}

/// Set or clear one slot of a checksum group in its full in-memory form.
pub(crate) fn set_checksum_slot(
    group: &mut [u8; CHECKSUM_GROUP_BYTES],
    slot: usize,
    crc: Option<u32>,
) {
    let mask = 1 << (slot % 8);
    if crc.is_some() {
        group[slot / 8] |= mask;
    } else {
        group[slot / 8] &= !mask;
    }
    group[32 + slot * 4..36 + slot * 4].copy_from_slice(&crc.unwrap_or(0).to_le_bytes());
}

/// CRC32C of `bytes` with `zero_start..zero_end` read as zeros. Callers pass
/// constant ranges shorter than `ZEROS` that lie inside `bytes`.
fn crc32c_with_zero(bytes: &[u8], zero_start: usize, zero_end: usize) -> u32 {
    let end = zero_end.min(bytes.len());
    let start = zero_start.min(end);
    let crc = crc32c_extend(0xFFFF_FFFF, &bytes[..start]);
    let crc = crc32c_extend(crc, &ZEROS[..end - start]);
    crc32c_extend(crc, &bytes[end..]) ^ 0xFFFF_FFFF
}

fn read_array<const N: usize>(
    bytes: &[u8],
    offset: usize,
    reason: &'static str,
) -> Result<[u8; N]> {
    let end = offset.checked_add(N).ok_or(Error::Corrupt(reason))?;
    let slice = bytes.get(offset..end).ok_or(Error::Corrupt(reason))?;
    slice.try_into().map_err(|_| Error::Corrupt(reason))
}

fn read_u16_le(bytes: &[u8], offset: usize, reason: &'static str) -> Result<u16> {
    Ok(u16::from_le_bytes(read_array(bytes, offset, reason)?))
}

fn read_u32_le(bytes: &[u8], offset: usize, reason: &'static str) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(bytes, offset, reason)?))
}

fn read_u64_le(bytes: &[u8], offset: usize, reason: &'static str) -> Result<u64> {
    Ok(u64::from_le_bytes(read_array(bytes, offset, reason)?))
}

fn put_u16_le(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64_le(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_ref(bytes: &mut [u8], offset: usize, reference: TreeRef) {
    put_u64_le(bytes, offset, reference.block);
    put_u64_le(bytes, offset + 8, reference.generation);
}

fn read_ref(bytes: &[u8], offset: usize, reason: &'static str) -> Result<TreeRef> {
    Ok(TreeRef {
        block: read_u64_le(bytes, offset, reason)?,
        generation: read_u64_le(bytes, offset + 8, reason)?,
    })
}

pub(crate) fn all_zero(bytes: &[u8]) -> bool {
    // No early exit: valid bytes are all zero, and the OR loop vectorizes.
    bytes.iter().fold(0, |acc, &byte| acc | byte) == 0
}

fn geometry_blocks(geometry: &Geometry) -> Result<u64> {
    geometry.validate()?;
    let blocks = geometry.blocks();
    if blocks < RESERVED_TOTAL_BLOCKS {
        return Err(Error::InvalidGeometry);
    }
    Ok(blocks)
}

fn validate_blocks(blocks: u64, mode: Validation) -> Result<()> {
    if blocks < RESERVED_TOTAL_BLOCKS {
        return Err(violation(mode, "device has no usable block range"));
    }
    Ok(())
}

fn is_nonreserved_block(block: u64, blocks: u64) -> bool {
    block >= RESERVED_BLOCKS && blocks >= RESERVED_TOTAL_BLOCKS && block < blocks - RESERVED_BLOCKS
}

fn validate_tree_ref(reference: TreeRef, blocks: u64, mode: Validation) -> Result<()> {
    validate_blocks(blocks, mode)?;
    if reference.generation == 0 || !is_nonreserved_block(reference.block, blocks) {
        return Err(violation(mode, "invalid tree reference"));
    }
    Ok(())
}

fn validate_page_ref(
    reference: TreeRef,
    kind: TreeKind,
    blocks: u64,
    mode: Validation,
) -> Result<()> {
    if kind != TreeKind::Workspace {
        return validate_tree_ref(reference, blocks, mode);
    }
    validate_blocks(blocks, mode)?;
    if reference.generation == 0 || !crate::workspace::is_bank_block(reference.block, blocks) {
        return Err(violation(mode, "invalid workspace page reference"));
    }
    Ok(())
}

fn validate_workspace_key(key: &[u8], blocks: u64, mode: Validation) -> Result<()> {
    let block = u64::from_be_bytes(
        key.try_into()
            .map_err(|_| violation(mode, "workspace key length"))?,
    );
    if !is_nonreserved_block(block, blocks) {
        return Err(violation(mode, "invalid workspace block"));
    }
    Ok(())
}

/// A workspace leaf record is a run: the first block as the key, and the
/// number of consecutive slots it covers as a four-byte value. The nonreserved
/// region is one interval, so checking the run's last block is enough.
fn validate_workspace_run(key: &[u8], value: &[u8], blocks: u64, mode: Validation) -> Result<()> {
    validate_workspace_key(key, blocks, mode)?;
    let first = u64::from_be_bytes(
        key.try_into()
            .map_err(|_| violation(mode, "workspace key length"))?,
    );
    let count = u32::from_le_bytes(
        value
            .try_into()
            .map_err(|_| violation(mode, "workspace run length"))?,
    );
    let last = u64::from(count)
        .checked_sub(1)
        .and_then(|span| first.checked_add(span))
        .ok_or_else(|| violation(mode, "invalid workspace run length"))?;
    if !is_nonreserved_block(last, blocks) {
        return Err(violation(mode, "workspace run leaves the volume"));
    }
    Ok(())
}

fn validate_uuid(uuid: &[u8; 16], mode: Validation) -> Result<()> {
    if all_zero(uuid) {
        return Err(violation(mode, "zero volume UUID"));
    }
    Ok(())
}

fn validate_root_slot(slot: u64, blocks: u64, mode: Validation) -> Result<()> {
    validate_blocks(blocks, mode)?;
    let root_b = blocks
        .checked_sub(257)
        .ok_or_else(|| violation(mode, "invalid root slot"))?;
    if slot != 256 && slot != root_b {
        return Err(violation(mode, "invalid root slot"));
    }
    Ok(())
}

fn validate_root_fields(root: &Root, slot: u64, blocks: u64, mode: Validation) -> Result<()> {
    validate_blocks(blocks, mode)?;
    validate_root_slot(slot, blocks, mode)?;
    validate_uuid(&root.uuid, mode)?;
    if root.generation == 0 || root.blocks != blocks {
        return Err(violation(mode, "invalid root generation or geometry"));
    }
    if root.workspace_cursor >= blocks.div_ceil(ALLOCATION_GROUP_BLOCKS) {
        return Err(violation(mode, "workspace scan cursor exceeds geometry"));
    }
    validate_tree_ref(root.catalog, blocks, mode)?;
    validate_tree_ref(root.allocation, blocks, mode)?;
    validate_tree_ref(root.cleanup, blocks, mode)?;
    validate_page_ref(root.workspace, TreeKind::Workspace, blocks, mode)?;
    if !crate::workspace::is_bank_root(root.workspace.block, blocks)
        || root.workspace.generation > root.generation
    {
        return Err(violation(mode, "invalid workspace root"));
    }
    if root.catalog.block == root.allocation.block
        || root.catalog.block == root.cleanup.block
        || root.allocation.block == root.cleanup.block
    {
        return Err(violation(mode, "root tree references share a block"));
    }
    if root.catalog.generation > root.generation
        || root.allocation.generation > root.generation
        || root.cleanup.generation > root.generation
    {
        return Err(violation(mode, "root tree generation is newer than root"));
    }
    if !(2..=MAX_ID).contains(&root.next_object) {
        return Err(violation(mode, "invalid next object ID"));
    }
    if !(1..=MAX_ID).contains(&root.next_job) {
        return Err(violation(mode, "invalid next cleanup-job ID"));
    }
    let allocatable = blocks - RESERVED_TOTAL_BLOCKS - 3;
    if root.free_blocks > allocatable {
        return Err(violation(mode, "free block count exceeds geometry"));
    }
    Ok(())
}

impl Root {
    pub(crate) fn decode(bytes: &[u8], slot: u64, geometry: Geometry) -> Result<Self> {
        if bytes.len() != BLOCK_BYTES {
            return Err(Error::Corrupt("root page length"));
        }
        let blocks = geometry_blocks(&geometry)?;
        validate_root_slot(slot, blocks, Validation::Input)?;

        let stored_crc = read_u32_le(bytes, 4092, "root checksum")?;
        if crc32c_with_zero(bytes, 4092, BLOCK_BYTES) != stored_crc {
            return Err(Error::Corrupt("root checksum"));
        }
        if bytes[0..8] != *ROOT_MAGIC {
            return Err(Error::Corrupt("root magic"));
        }

        if read_u32_le(bytes, 8, "root revision")? != u32::from(REVISION) {
            return Err(Error::UnsupportedFormat);
        }
        if read_u32_le(bytes, 12, "root record bytes")? != BLOCK_BYTES as u32
            || read_u32_le(bytes, 16, "root allocation bytes")? != ALLOCATION_BYTES
            || read_u32_le(bytes, 20, "root checksum-unit bytes")? != CHECKSUM_UNIT_BYTES
        {
            return Err(Error::Corrupt("root envelope"));
        }
        // A zero label length means the volume is unnamed.
        let length = usize::from(read_u16_le(bytes, 184, "root label length")?);
        if length > 255 {
            return Err(Error::Corrupt("root label length"));
        }
        if !all_zero(&bytes[186 + length..4092]) {
            return Err(Error::Corrupt("root label padding"));
        }
        let label = if length == 0 {
            None
        } else {
            let text = str::from_utf8(&bytes[186..186 + length])
                .map_err(|_| Error::Corrupt("root label UTF-8"))?;
            Some(
                crate::VolumeLabel::parse(text)
                    .map_err(|_| Error::Corrupt("invalid root label"))?,
            )
        };

        let uuid = read_array(bytes, 24, "root UUID")?;
        let self_block = read_u64_le(bytes, 40, "root self block")?;
        let generation = read_u64_le(bytes, 48, "root generation")?;
        let device_blocks = read_u64_le(bytes, 56, "root device blocks")?;
        let catalog = read_ref(bytes, 64, "root catalog reference")?;
        let allocation = read_ref(bytes, 80, "root allocation reference")?;
        let cleanup = read_ref(bytes, 96, "root cleanup reference")?;
        let next_object = read_u64_le(bytes, 112, "root next object")?;
        let free_blocks = read_u64_le(bytes, 120, "root free blocks")?;
        let feature_bits = read_u64_le(bytes, 128, "root feature bits")?;
        let filename_profile = read_u32_le(bytes, 136, "root filename profile")?;
        let state_flags = read_u32_le(bytes, 140, "root state flags")?;
        let maintenance_reserve = read_u64_le(bytes, 144, "root maintenance reserve")?;
        let next_job = read_u64_le(bytes, 152, "root next job")?;

        if feature_bits != 0 || filename_profile != 2 || state_flags != 0 {
            return Err(Error::UnsupportedFormat);
        }
        if self_block != slot || device_blocks != blocks {
            return Err(Error::Corrupt("root identity or geometry"));
        }
        if maintenance_reserve != MAINTENANCE_RESERVE {
            return Err(Error::Corrupt("root maintenance reserve"));
        }

        let workspace = read_ref(bytes, 160, "root workspace reference")?;
        let root = Root {
            label,
            uuid,
            generation,
            blocks: device_blocks,
            catalog,
            allocation,
            cleanup,
            next_object,
            free_blocks,
            next_job,
            workspace,
            workspace_cursor: read_u64_le(bytes, 176, "workspace scan cursor")?,
        };
        validate_root_fields(&root, slot, blocks, Validation::Disk)?;
        Ok(root)
    }

    pub(crate) fn encode(&self, slot: u64, geometry: Geometry) -> Result<Page> {
        let blocks = geometry_blocks(&geometry)?;
        validate_root_fields(self, slot, blocks, Validation::Input)?;

        let mut page = [0u8; BLOCK_BYTES];
        page[0..8].copy_from_slice(ROOT_MAGIC);
        put_u32_le(&mut page, 8, u32::from(REVISION));
        put_u32_le(&mut page, 12, BLOCK_BYTES as u32);
        put_u32_le(&mut page, 16, ALLOCATION_BYTES);
        put_u32_le(&mut page, 20, CHECKSUM_UNIT_BYTES);
        page[24..40].copy_from_slice(&self.uuid);
        put_u64_le(&mut page, 40, slot);
        put_u64_le(&mut page, 48, self.generation);
        put_u64_le(&mut page, 56, self.blocks);
        put_ref(&mut page, 64, self.catalog);
        put_ref(&mut page, 80, self.allocation);
        put_ref(&mut page, 96, self.cleanup);
        put_u64_le(&mut page, 112, self.next_object);
        put_u64_le(&mut page, 120, self.free_blocks);
        put_u64_le(&mut page, 128, 0);
        put_u32_le(&mut page, 136, 2);
        put_u32_le(&mut page, 140, 0);
        put_u64_le(&mut page, 144, MAINTENANCE_RESERVE);
        put_u64_le(&mut page, 152, self.next_job);
        put_ref(&mut page, 160, self.workspace);
        put_u64_le(&mut page, 176, self.workspace_cursor);
        if let Some(label) = &self.label {
            let bytes = label.as_str().as_bytes();
            put_u16_le(
                &mut page,
                184,
                u16::try_from(bytes.len())
                    .map_err(|_| Error::InvalidInput("volume label too long"))?,
            );
            page[186..186 + bytes.len()].copy_from_slice(bytes);
        }
        let crc = crc32c(&page);
        put_u32_le(&mut page, 4092, crc);
        Ok(Page(Box::new(page)))
    }
}

fn tree_kind_value(kind: TreeKind) -> u8 {
    kind as u8
}

fn validate_owner_id(owner: u64, mode: Validation) -> Result<()> {
    if !(1..MAX_ID).contains(&owner) {
        return Err(violation(mode, "invalid object ID"));
    }
    Ok(())
}

fn validate_catalog_key(key: &[u8], mode: Validation) -> Result<u8> {
    if key.len() < 9 {
        return Err(violation(mode, "catalog key is too short"));
    }
    validate_owner_id(
        u64::from_be_bytes(
            key[0..8]
                .try_into()
                .map_err(|_| violation(mode, "catalog owner ID"))?,
        ),
        mode,
    )?;
    let kind = key[8];
    match kind {
        1 if key.len() == 9 => Ok(kind),
        2 if (10..=774).contains(&key.len()) && str::from_utf8(&key[9..]).is_ok() => Ok(kind),
        3 | 4 if key.len() == 17 => Ok(kind),
        _ => Err(violation(mode, "invalid catalog key")),
    }
}

fn validate_timestamp(bytes: &[u8], offset: usize, mode: Validation) -> Result<()> {
    let seconds = i64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .map_err(|_| violation(mode, "timestamp seconds"))?,
    );
    let nanos = u32::from_le_bytes(
        bytes[offset + 8..offset + 12]
            .try_into()
            .map_err(|_| violation(mode, "timestamp nanoseconds"))?,
    );
    let reserved = u32::from_le_bytes(
        bytes[offset + 12..offset + 16]
            .try_into()
            .map_err(|_| violation(mode, "timestamp reserved bytes"))?,
    );
    validate_timestamp_range(seconds, nanos, mode)?;
    if reserved != 0 {
        return Err(violation(mode, "timestamp reserved bytes"));
    }
    Ok(())
}

/// A symlink target is 1 through `MAX_SYMLINK_TARGET` bytes of UTF-8 without
/// NUL. It is stored verbatim: the host resolves it, CarryFS never does.
pub(crate) fn validate_symlink_target(target: &[u8], mode: Validation) -> Result<()> {
    if target.len() > MAX_SYMLINK_TARGET {
        return Err(match mode {
            Validation::Input => Error::TooLong("symlink target is too long"),
            Validation::Disk => Error::Corrupt("symlink target is too long"),
        });
    }
    if target.is_empty() || target.contains(&0) || str::from_utf8(target).is_err() {
        return Err(violation(mode, "invalid symlink target"));
    }
    Ok(())
}

fn validate_inode(value: &[u8], blocks: u64, mode: Validation) -> Result<()> {
    let object_type = *value
        .first()
        .ok_or_else(|| violation(mode, "invalid inode length"))?;
    if !(1..=3).contains(&object_type) {
        return Err(violation(mode, "invalid inode type"));
    }
    // A symlink's target follows the fixed 128 bytes; other inodes end there.
    if value.len() < 128 || (object_type != 3 && value.len() != 128) {
        return Err(violation(mode, "invalid inode length"));
    }
    // Flag bit 0 marks an orphan. Only a symlink may set bit 1, its Windows
    // link kind is recorded, and then bit 2, that kind is directory.
    if value[1] & !7 != 0
        || (value[1] & 6 != 0 && object_type != 3)
        || value[1] & 6 == 4
        || !all_zero(&value[2..8])
        || !all_zero(&value[88..128])
    {
        return Err(violation(mode, "invalid inode flags or reserved bytes"));
    }
    let parent = u64::from_le_bytes(
        value[8..16]
            .try_into()
            .map_err(|_| violation(mode, "inode parent"))?,
    );
    let size = u64::from_le_bytes(
        value[16..24]
            .try_into()
            .map_err(|_| violation(mode, "inode size"))?,
    );
    let allocated = u64::from_le_bytes(
        value[24..32]
            .try_into()
            .map_err(|_| violation(mode, "inode allocation"))?,
    );
    let children = u64::from_le_bytes(
        value[32..40]
            .try_into()
            .map_err(|_| violation(mode, "inode child count"))?,
    );
    validate_owner_id(parent, mode)?;
    if size > MAX_FILE_BYTES || allocated > blocks || children > MAX_ID {
        return Err(violation(mode, "inode range"));
    }
    if (object_type == 2 && (size != 0 || allocated != 0))
        || (object_type == 1 && children != 0)
        || (object_type == 3
            && (allocated != 0 || children != 0 || size != (value.len() - 128) as u64))
    {
        return Err(violation(mode, "inode type-specific fields"));
    }
    if object_type == 3 {
        validate_symlink_target(&value[128..], mode)?;
    }
    validate_timestamp(value, 40, mode)?;
    validate_timestamp(value, 56, mode)?;
    validate_timestamp(value, 72, mode)?;
    Ok(())
}

fn validate_directory_entry(value: &[u8], mode: Validation) -> Result<()> {
    if value.len() < 12 {
        return Err(violation(mode, "directory entry is too short"));
    }
    let target = u64::from_le_bytes(
        value[0..8]
            .try_into()
            .map_err(|_| violation(mode, "directory target"))?,
    );
    validate_owner_id(target, mode)?;
    if !(1..=3).contains(&value[8]) {
        return Err(violation(mode, "invalid directory entry type"));
    }
    if value[9] != 0 {
        return Err(violation(mode, "directory entry reserved byte"));
    }
    let name_bytes = usize::from(u16::from_le_bytes(
        value[10..12]
            .try_into()
            .map_err(|_| violation(mode, "directory entry name length"))?,
    ));
    if !(1..=255).contains(&name_bytes)
        || value.len() != 12 + name_bytes
        || str::from_utf8(&value[12..]).is_err()
    {
        return Err(violation(mode, "invalid directory entry name"));
    }
    Ok(())
}

fn validate_extent(key: &[u8], value: &[u8], blocks: u64, mode: Validation) -> Result<u64> {
    let logical_start = u64::from_be_bytes(
        key[9..17]
            .try_into()
            .map_err(|_| violation(mode, "extent logical start"))?,
    );
    if value.len() != 16 {
        return Err(violation(mode, "invalid extent length"));
    }
    let physical_start = u64::from_le_bytes(
        value[0..8]
            .try_into()
            .map_err(|_| violation(mode, "extent physical start"))?,
    );
    let count = u64::from(u32::from_le_bytes(
        value[8..12]
            .try_into()
            .map_err(|_| violation(mode, "extent block count"))?,
    ));
    let reserved = u32::from_le_bytes(
        value[12..16]
            .try_into()
            .map_err(|_| violation(mode, "extent reserved bytes"))?,
    );
    let physical_end = physical_start
        .checked_add(count)
        .ok_or_else(|| violation(mode, "extent physical range"))?;
    let logical_end = logical_start
        .checked_add(count)
        .ok_or_else(|| violation(mode, "extent logical range"))?;
    if count == 0
        || reserved != 0
        || logical_end > MAX_LOGICAL_BLOCK
        || !is_nonreserved_block(physical_start, blocks)
        || physical_end > blocks - RESERVED_BLOCKS
    {
        return Err(violation(mode, "invalid extent range"));
    }
    Ok(count)
}

fn validate_checksum_group(key: &[u8], value: &[u8], mode: Validation) -> Result<()> {
    let group = u64::from_be_bytes(
        key[9..17]
            .try_into()
            .map_err(|_| violation(mode, "checksum group index"))?,
    );
    if group > MAX_CHECKSUM_GROUP
        || !(36..=CHECKSUM_GROUP_BYTES).contains(&value.len())
        || !(value.len() - 32).is_multiple_of(4)
    {
        return Err(violation(mode, "invalid checksum group"));
    }
    // The record ends at its last present slot, so that slot's bit is set and
    // no bit above it is: one encoding per group, and never an empty one.
    let last = (value.len() - 32) / 4 - 1;
    let (byte, bit) = (last / 8, last % 8);
    if value[byte] >> bit != 1 || !all_zero(&value[byte + 1..32]) {
        return Err(violation(mode, "checksum group is not compact"));
    }
    // Each bitmap byte covers eight four-byte checksum slots; the slots of a
    // partially stored last byte are checked one at a time.
    let (whole, tail) = value[32..].as_chunks::<32>();
    for (slot, checksum) in tail.as_chunks::<4>().0.iter().enumerate() {
        if value[whole.len()] & (1 << slot) == 0 && *checksum != [0; 4] {
            return Err(violation(mode, "hole checksum is nonzero"));
        }
    }
    for (&present, checksums) in value[..32].iter().zip(whole) {
        if present == 0 {
            if *checksums != [0; 32] {
                return Err(violation(mode, "hole checksum is nonzero"));
            }
        } else {
            for (slot, checksum) in checksums.as_chunks::<4>().0.iter().enumerate() {
                if present & (1 << slot) == 0 && *checksum != [0; 4] {
                    return Err(violation(mode, "hole checksum is nonzero"));
                }
            }
        }
    }
    Ok(())
}

fn allocation_group_count(blocks: u64) -> Result<u64> {
    blocks
        .checked_add(ALLOCATION_GROUP_BLOCKS - 1)
        .map(|rounded| rounded / ALLOCATION_GROUP_BLOCKS)
        .ok_or(Error::Corrupt("allocation group range"))
}

fn validate_allocation_group(
    key: &[u8],
    value: &[u8],
    blocks: u64,
    mode: Validation,
) -> Result<()> {
    if key.len() != 8 || value.len() != ALLOCATION_GROUP_BYTES {
        return Err(violation(mode, "invalid allocation group"));
    }
    let group = u64::from_be_bytes(
        key.try_into()
            .map_err(|_| violation(mode, "allocation group index"))?,
    );
    if group
        >= allocation_group_count(blocks).map_err(|_| violation(mode, "allocation group index"))?
    {
        return Err(violation(mode, "allocation group index out of range"));
    }
    let free = |block: u64| {
        value[(block % ALLOCATION_GROUP_BLOCKS / 8) as usize] & (1 << (block % 8)) == 0
    };
    if reserved_group_blocks(group, blocks).any(free) {
        return Err(violation(mode, "reserved allocation bit is free"));
    }
    Ok(())
}

/// The blocks of allocation group `group` that can never be allocated: those
/// in the reserved bands at both ends of the volume and those past its end.
/// Only the first and last groups have any, so the groups between them cost
/// their callers nothing per block.
pub(crate) fn reserved_group_blocks(group: u64, blocks: u64) -> impl Iterator<Item = u64> {
    let first = group.saturating_mul(ALLOCATION_GROUP_BLOCKS);
    let end = first.saturating_add(ALLOCATION_GROUP_BLOCKS);
    let tail = blocks.saturating_sub(RESERVED_BLOCKS).max(first);
    (first..end.min(RESERVED_BLOCKS)).chain(tail..end)
}

fn validate_cleanup_key_value(
    key: &[u8],
    value: &[u8],
    blocks: u64,
    page_generation: u64,
    mode: Validation,
) -> Result<()> {
    if key.len() != 9 && key.len() != 17 {
        return Err(violation(mode, "invalid cleanup key length"));
    }
    match key[0] {
        1 => {
            if key.len() != 17 || value.len() != 8 {
                return Err(violation(mode, "invalid retirement record"));
            }
            let generation = u64::from_be_bytes(
                key[1..9]
                    .try_into()
                    .map_err(|_| violation(mode, "retirement generation"))?,
            );
            let first_block = u64::from_be_bytes(
                key[9..17]
                    .try_into()
                    .map_err(|_| violation(mode, "retirement first block"))?,
            );
            let count = u64::from(u32::from_le_bytes(
                value[0..4]
                    .try_into()
                    .map_err(|_| violation(mode, "retirement count"))?,
            ));
            let reserved = u32::from_le_bytes(
                value[4..8]
                    .try_into()
                    .map_err(|_| violation(mode, "retirement reserved bytes"))?,
            );
            let end = first_block
                .checked_add(count)
                .ok_or_else(|| violation(mode, "retirement range"))?;
            if generation == 0
                || generation > page_generation
                || count == 0
                || reserved != 0
                || !is_nonreserved_block(first_block, blocks)
                || end > blocks - RESERVED_BLOCKS
            {
                return Err(violation(mode, "invalid retirement range"));
            }
        }
        2 => {
            if key.len() != 9 || value.len() != 32 {
                return Err(violation(mode, "invalid detached-tree job"));
            }
            let job_id = u64::from_be_bytes(
                key[1..9]
                    .try_into()
                    .map_err(|_| violation(mode, "cleanup job ID"))?,
            );
            validate_owner_id(job_id, mode)?;
            let owner = u64::from_le_bytes(
                value[0..8]
                    .try_into()
                    .map_err(|_| violation(mode, "detached-tree owner"))?,
            );
            validate_owner_id(owner, mode)?;
            let low = value[8];
            let high = value[9];
            if !(1..=4).contains(&low)
                || !(1..=4).contains(&high)
                || low > high
                || !all_zero(&value[10..16])
            {
                return Err(violation(mode, "invalid detached-tree kind range"));
            }
            let reference = read_ref(value, 16, "detached-tree reference")?;
            validate_tree_ref(reference, blocks, mode)?;
            if reference.generation > page_generation {
                return Err(violation(mode, "detached-tree reference is newer"));
            }
        }
        3 => {
            if key.len() != 9 || !value.is_empty() {
                return Err(violation(mode, "invalid orphan marker"));
            }
            let object_id = u64::from_be_bytes(
                key[1..9]
                    .try_into()
                    .map_err(|_| violation(mode, "orphan object ID"))?,
            );
            validate_owner_id(object_id, mode)?;
        }
        _ => return Err(violation(mode, "unknown cleanup record kind")),
    }
    Ok(())
}

/// The data blocks one leaf record accounts for, after the checks
/// `encode_records` makes of each record in a leaf.
pub(crate) fn leaf_record_blocks(
    kind: TreeKind,
    key: &[u8],
    value: &[u8],
    blocks: u64,
    generation: u64,
) -> Result<u64> {
    if !(1..=1024).contains(&key.len()) {
        return Err(Error::InvalidInput("invalid record key length"));
    }
    if 4 + key.len() + value.len() > MAX_RECORD_BYTES {
        return Err(Error::InvalidInput("record is too large"));
    }
    validate_leaf_record(kind, key, value, blocks, generation, Validation::Input)
}

fn validate_leaf_record(
    kind: TreeKind,
    key: &[u8],
    value: &[u8],
    blocks: u64,
    page_generation: u64,
    mode: Validation,
) -> Result<u64> {
    match kind {
        TreeKind::Catalog => {
            let record_kind = validate_catalog_key(key, mode)?;
            match record_kind {
                1 => validate_inode(value, blocks, mode)?,
                2 => validate_directory_entry(value, mode)?,
                3 => return validate_extent(key, value, blocks, mode),
                4 => validate_checksum_group(key, value, mode)?,
                _ => return Err(violation(mode, "unknown catalog record kind")),
            }
            Ok(0)
        }
        TreeKind::Allocation => {
            validate_allocation_group(key, value, blocks, mode)?;
            Ok(0)
        }
        TreeKind::Cleanup => {
            validate_cleanup_key_value(key, value, blocks, page_generation, mode)?;
            Ok(0)
        }
        TreeKind::Workspace => {
            validate_workspace_run(key, value, blocks, mode)?;
            Ok(0)
        }
    }
}

fn validate_internal_record(
    kind: TreeKind,
    key: &[u8],
    value: &[u8],
    blocks: u64,
    page_generation: u64,
    mode: Validation,
) -> Result<TreeRef> {
    match kind {
        TreeKind::Catalog => {
            validate_catalog_key(key, mode)?;
        }
        TreeKind::Allocation => {
            if key.len() != 8 {
                return Err(violation(mode, "invalid allocation internal key"));
            }
            let group = u64::from_be_bytes(
                key.try_into()
                    .map_err(|_| violation(mode, "allocation internal key"))?,
            );
            if group
                >= allocation_group_count(blocks)
                    .map_err(|_| violation(mode, "allocation internal key"))?
            {
                return Err(violation(mode, "allocation internal key range"));
            }
        }
        TreeKind::Workspace => validate_workspace_key(key, blocks, mode)?,
        TreeKind::Cleanup => {
            if key.len() != 9 && key.len() != 17 {
                return Err(violation(mode, "invalid cleanup internal key"));
            }
            match key[0] {
                1 if key.len() == 17 => {
                    let generation = u64::from_be_bytes(
                        key[1..9]
                            .try_into()
                            .map_err(|_| violation(mode, "cleanup internal key"))?,
                    );
                    if generation == 0 || generation > page_generation {
                        return Err(violation(mode, "invalid retirement generation"));
                    }
                    let first_block = u64::from_be_bytes(
                        key[9..17]
                            .try_into()
                            .map_err(|_| violation(mode, "cleanup internal key"))?,
                    );
                    if !is_nonreserved_block(first_block, blocks) {
                        return Err(violation(mode, "invalid retirement block"));
                    }
                }
                2 | 3 if key.len() == 9 => {
                    validate_owner_id(
                        u64::from_be_bytes(
                            key[1..9]
                                .try_into()
                                .map_err(|_| violation(mode, "cleanup internal key"))?,
                        ),
                        mode,
                    )?;
                }
                _ => return Err(violation(mode, "invalid cleanup internal key")),
            }
        }
    }
    if value.len() != 16 {
        return Err(violation(mode, "internal value is not a tree reference"));
    }
    let reference = read_ref(value, 0, "internal tree reference")?;
    validate_page_ref(reference, kind, blocks, mode)?;
    if reference.generation > page_generation {
        return Err(violation(mode, "child generation is newer than parent"));
    }
    Ok(reference)
}

fn validate_records(
    bytes: &[u8],
    payload_bytes: usize,
    count: usize,
    context: RecordValidation,
) -> Result<(Vec<PageRecord<'_>>, u64)> {
    let RecordValidation {
        kind,
        page_kind,
        page_generation,
        blocks,
        mode,
    } = context;
    let payload_start = PAGE_PAYLOAD_OFFSET;
    if count > payload_bytes / 4 {
        return Err(violation(mode, "record count exceeds payload"));
    }
    let payload_end = payload_start
        .checked_add(payload_bytes)
        .ok_or_else(|| violation(mode, "payload range"))?;
    if payload_end > bytes.len() {
        return Err(violation(mode, "payload range"));
    }
    let mut records = Vec::with_capacity(count);
    let mut position = payload_start;
    let mut data_blocks = 0u64;
    let mut child_blocks = Vec::new();

    for _ in 0..count {
        let key_bytes = usize::from(read_u16_le(bytes, position, "record key length")?);
        let value_bytes = usize::from(read_u16_le(bytes, position + 2, "record value length")?);
        if !(1..=1024).contains(&key_bytes) {
            return Err(violation(mode, "invalid record key length"));
        }
        let record_bytes = 4usize
            .checked_add(key_bytes)
            .and_then(|value| value.checked_add(value_bytes))
            .ok_or_else(|| violation(mode, "record length"))?;
        if record_bytes > MAX_RECORD_BYTES {
            return Err(violation(mode, "record is too large"));
        }
        let record_end = position
            .checked_add(record_bytes)
            .ok_or_else(|| violation(mode, "record range"))?;
        if record_end > payload_end {
            return Err(violation(mode, "record exceeds payload"));
        }
        let key_start = position + 4;
        let value_start = key_start + key_bytes;
        let key = &bytes[key_start..value_start];
        let value = &bytes[value_start..record_end];
        if records.last().is_some_and(|(last_key, _)| *last_key >= key) {
            return Err(violation(mode, "record keys are not increasing"));
        }
        if page_kind == 1 {
            data_blocks = data_blocks
                .checked_add(validate_leaf_record(
                    kind,
                    key,
                    value,
                    blocks,
                    page_generation,
                    mode,
                )?)
                .ok_or_else(|| violation(mode, "leaf data-block total overflow"))?;
        } else {
            let reference =
                validate_internal_record(kind, key, value, blocks, page_generation, mode)?;
            if child_blocks.contains(&reference.block) {
                return Err(violation(mode, "internal children share a block"));
            }
            child_blocks.push(reference.block);
        }
        records.push((key, value));
        position = record_end;
    }

    if position != payload_end {
        return Err(violation(mode, "payload has unconsumed bytes"));
    }
    Ok((records, data_blocks))
}

fn validate_page_header(
    bytes: &[u8],
    expected: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    parent_generation: u64,
    expected_level: Option<u8>,
    blocks: u64,
) -> Result<(u8, u8, u64, u64, usize, usize)> {
    if bytes.len() != BLOCK_BYTES {
        return Err(Error::Corrupt("page length"));
    }
    validate_blocks(blocks, Validation::Input)?;
    validate_page_ref(expected, kind, blocks, Validation::Input)?;
    validate_uuid(uuid, Validation::Input)?;
    if parent_generation == 0 || expected.generation > parent_generation {
        return Err(Error::InvalidInput("invalid parent generation"));
    }
    if expected_level.is_some_and(|level| level > 7) {
        return Err(Error::InvalidInput("invalid expected page level"));
    }

    let stored_crc = read_u32_le(bytes, 64, "page checksum")?;
    if crc32c_with_zero(bytes, 64, 68) != stored_crc {
        return Err(Error::Corrupt("page checksum"));
    }
    if bytes[0..8] != *PAGE_MAGIC {
        return Err(Error::Corrupt("page magic"));
    }
    if read_u16_le(bytes, 8, "page revision")? != REVISION {
        return Err(Error::UnsupportedFormat);
    }
    if read_u16_le(bytes, 10, "page flags")? != 0 {
        return Err(Error::UnsupportedFormat);
    }
    let page_kind = bytes[12];
    if page_kind != 1 && page_kind != 2 {
        return Err(Error::Corrupt("page kind"));
    }
    let tree_kind = match bytes[13] {
        1 => TreeKind::Catalog,
        2 => TreeKind::Allocation,
        3 => TreeKind::Cleanup,
        4 => TreeKind::Workspace,
        _ => return Err(Error::Corrupt("page tree kind")),
    };
    if tree_kind != kind {
        return Err(Error::Corrupt("page tree kind mismatch"));
    }
    let level = bytes[14];
    if (page_kind == 1 && level != 0) || (page_kind == 2 && !(1..=7).contains(&level)) {
        return Err(Error::Corrupt("page level"));
    }
    if expected_level.is_some_and(|expected_level| expected_level != level) {
        return Err(Error::Corrupt("unexpected page level"));
    }
    if bytes[15] != 0 || !all_zero(&bytes[36..40]) || !all_zero(&bytes[68..80]) {
        return Err(Error::Corrupt("page reserved bytes"));
    }
    let self_block = read_u64_le(bytes, 16, "page self block")?;
    let page_generation = read_u64_le(bytes, 24, "page generation")?;
    if self_block != expected.block
        || page_generation != expected.generation
        || page_generation > parent_generation
    {
        return Err(Error::Corrupt("page identity or generation"));
    }
    if bytes[40..56] != *uuid {
        return Err(Error::Corrupt("page UUID"));
    }
    let count = usize::from(read_u16_le(bytes, 32, "page entry count")?);
    let payload_bytes = usize::from(read_u16_le(bytes, 34, "page payload bytes")?);
    if payload_bytes > PAGE_PAYLOAD_BYTES {
        return Err(Error::Corrupt("page payload length"));
    }
    let data_blocks = read_u64_le(bytes, 56, "page data-block total")?;
    if data_blocks > blocks {
        return Err(Error::Corrupt("page data-block total"));
    }
    if tree_kind != TreeKind::Catalog && data_blocks != 0 {
        return Err(Error::Corrupt("non-catalog data-block total"));
    }
    Ok((
        page_kind,
        level,
        page_generation,
        data_blocks,
        count,
        payload_bytes,
    ))
}

pub(crate) fn decode_page<'a>(
    bytes: &'a [u8],
    expected: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    parent_generation: u64,
    expected_level: Option<u8>,
    blocks: u64,
) -> Result<DecodedPage<'a>> {
    let (page_kind, level, page_generation, data_blocks, count, payload_bytes) =
        validate_page_header(
            bytes,
            expected,
            uuid,
            kind,
            parent_generation,
            expected_level,
            blocks,
        )?;
    let (records, leaf_data_blocks) = validate_records(
        bytes,
        payload_bytes,
        count,
        RecordValidation {
            kind,
            page_kind,
            page_generation,
            blocks,
            mode: Validation::Disk,
        },
    )?;
    if page_kind == 1 && data_blocks != leaf_data_blocks {
        return Err(Error::Corrupt("leaf data-block total"));
    }
    let payload_end = PAGE_PAYLOAD_OFFSET + payload_bytes;
    if !all_zero(&bytes[payload_end..]) {
        return Err(Error::Corrupt("page padding"));
    }
    if page_kind == 2 && count < 2 {
        return Err(Error::Corrupt("internal page has too few children"));
    }
    Ok(DecodedPage {
        level,
        data_blocks,
        records,
    })
}

/// Validate `records` for one page and return the payload length and the
/// leaf data-block total, writing them into `page` when there is one.
fn encode_records(
    mut page: Option<&mut [u8; BLOCK_BYTES]>,
    records: &[(&[u8], &[u8])],
    kind: TreeKind,
    reference: TreeRef,
    blocks: u64,
    level: u8,
) -> Result<(usize, u64)> {
    if records.len() > usize::from(u16::MAX) {
        return Err(Error::InvalidInput("too many page records"));
    }
    let mut position = PAGE_PAYLOAD_OFFSET;
    let mut data_blocks = 0u64;
    let mut previous_key: Option<&[u8]> = None;
    for (key, value) in records {
        if !(1..=1024).contains(&key.len()) {
            return Err(Error::InvalidInput("invalid record key length"));
        }
        if previous_key.is_some_and(|previous| previous >= *key) {
            return Err(Error::InvalidInput("record keys are not increasing"));
        }
        let record_bytes = 4usize
            .checked_add(key.len())
            .and_then(|total| total.checked_add(value.len()))
            .ok_or(Error::InvalidInput("record length overflow"))?;
        if record_bytes > MAX_RECORD_BYTES {
            return Err(Error::InvalidInput("record is too large"));
        }
        let end = position
            .checked_add(record_bytes)
            .ok_or(Error::InvalidInput("page payload overflow"))?;
        if end > BLOCK_BYTES {
            return Err(Error::InvalidInput("page payload is too large"));
        }
        let key_len = u16::try_from(key.len()).map_err(|_| Error::InvalidInput("key length"))?;
        let value_len =
            u16::try_from(value.len()).map_err(|_| Error::InvalidInput("value length"))?;
        if let Some(page) = page.as_deref_mut() {
            put_u16_le(page, position, key_len);
            put_u16_le(page, position + 2, value_len);
            page[position + 4..position + 4 + key.len()].copy_from_slice(key);
            let value_start = position + 4 + key.len();
            page[value_start..end].copy_from_slice(value);
        }

        let record_blocks = if level == 0 {
            validate_leaf_record(
                kind,
                key,
                value,
                blocks,
                reference.generation,
                Validation::Input,
            )?
        } else {
            validate_internal_record(
                kind,
                key,
                value,
                blocks,
                reference.generation,
                Validation::Input,
            )?;
            0
        };
        data_blocks = data_blocks
            .checked_add(record_blocks)
            .ok_or(Error::InvalidInput("leaf data-block total overflow"))?;
        if data_blocks > blocks {
            return Err(Error::InvalidInput(
                "leaf data-block total exceeds geometry",
            ));
        }
        previous_key = Some(key);
        position = end;
    }
    Ok((position - PAGE_PAYLOAD_OFFSET, data_blocks))
}

pub(crate) fn encode_leaf(
    reference: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    records: &[(&[u8], &[u8])],
    blocks: u64,
) -> Result<Page> {
    encode_node(reference, uuid, kind, records, blocks, 0, 0)
}

/// The data-block total of a leaf holding `records`, after every check
/// `encode_leaf` makes, without building the page.
pub(crate) fn leaf_data_blocks(
    reference: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    records: &[(&[u8], &[u8])],
    blocks: u64,
) -> Result<u64> {
    validate_node(reference, uuid, kind, records, blocks, 0, 0)?;
    let (payload_bytes, data_blocks) = encode_records(None, records, kind, reference, blocks, 0)?;
    u16::try_from(payload_bytes).map_err(|_| Error::InvalidInput("payload length"))?;
    Ok(data_blocks)
}

fn validate_node(
    reference: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    records: &[(&[u8], &[u8])],
    blocks: u64,
    level: u8,
    child_data_blocks: u64,
) -> Result<()> {
    validate_blocks(blocks, Validation::Input)?;
    validate_page_ref(reference, kind, blocks, Validation::Input)?;
    validate_uuid(uuid, Validation::Input)?;
    if level > 7
        || (level > 0 && records.len() < 2)
        || child_data_blocks > blocks
        || (kind != TreeKind::Catalog && child_data_blocks != 0)
    {
        return Err(Error::InvalidInput(
            "invalid internal page level or aggregate",
        ));
    }
    Ok(())
}

pub(crate) fn encode_node(
    reference: TreeRef,
    uuid: &[u8; 16],
    kind: TreeKind,
    records: &[(&[u8], &[u8])],
    blocks: u64,
    level: u8,
    child_data_blocks: u64,
) -> Result<Page> {
    validate_node(
        reference,
        uuid,
        kind,
        records,
        blocks,
        level,
        child_data_blocks,
    )?;
    let mut page = [0u8; BLOCK_BYTES];
    page[0..8].copy_from_slice(PAGE_MAGIC);
    put_u16_le(&mut page, 8, REVISION);
    put_u16_le(&mut page, 10, 0);
    page[12] = if level == 0 { 1 } else { 2 };
    page[13] = tree_kind_value(kind);
    page[14] = level;
    page[15] = 0;
    put_u64_le(&mut page, 16, reference.block);
    put_u64_le(&mut page, 24, reference.generation);
    let (payload_bytes, leaf_data_blocks) =
        encode_records(Some(&mut page), records, kind, reference, blocks, level)?;
    let data_blocks = if level == 0 {
        leaf_data_blocks
    } else {
        child_data_blocks
    };
    let count = u16::try_from(records.len()).map_err(|_| Error::InvalidInput("record count"))?;
    let payload =
        u16::try_from(payload_bytes).map_err(|_| Error::InvalidInput("payload length"))?;
    put_u16_le(&mut page, 32, count);
    put_u16_le(&mut page, 34, payload);
    page[40..56].copy_from_slice(uuid);
    put_u64_le(&mut page, 56, data_blocks);
    let crc = crc32c(&page);
    put_u32_le(&mut page, 64, crc);
    Ok(Page(Box::new(page)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_page_round_trip_preserves_child_references() {
        let reference = TreeRef {
            block: 1024,
            generation: 2,
        };
        let keys = [[3, 0, 0, 0, 0, 0, 0, 0, 1], [3, 0, 0, 0, 0, 0, 0, 0, 2]];
        let values = [1025u64, 1026].map(|block| {
            let mut value = [0; 16];
            value[..8].copy_from_slice(&block.to_le_bytes());
            value[8..].copy_from_slice(&1u64.to_le_bytes());
            value
        });
        let records = [
            (keys[0].as_slice(), values[0].as_slice()),
            (keys[1].as_slice(), values[1].as_slice()),
        ];
        let page = encode_node(
            reference,
            &UUID,
            TreeKind::Cleanup,
            &records,
            GEOMETRY.blocks(),
            1,
            0,
        )
        .unwrap();
        let decoded = decode_page(
            &page.0[..],
            reference,
            &UUID,
            TreeKind::Cleanup,
            2,
            Some(1),
            GEOMETRY.blocks(),
        )
        .unwrap();
        assert_eq!(decoded.records, records);
        assert_eq!(decoded.level, 1);
        assert_eq!(
            encode_node(
                reference,
                &UUID,
                TreeKind::Cleanup,
                &decoded.records,
                GEOMETRY.blocks(),
                decoded.level,
                decoded.data_blocks
            )
            .unwrap(),
            page
        );
    }

    const GEOMETRY: Geometry = Geometry {
        byte_len: 256 * 1024 * 1024,
        logical_sector_bytes: 4096,
        physical_sector_bytes: 4096,
    };
    const UUID: [u8; 16] = [0x42; 16];

    fn root_a() -> Root {
        Root {
            label: None,
            uuid: [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ],
            generation: 7,
            blocks: GEOMETRY.blocks(),
            catalog: TreeRef {
                block: 1024,
                generation: 5,
            },
            allocation: TreeRef {
                block: 1025,
                generation: 7,
            },
            cleanup: TreeRef {
                block: 1026,
                generation: 7,
            },
            next_object: 18,
            free_blocks: 50_000,
            next_job: 9,
            workspace: TreeRef {
                block: 32,
                generation: 7,
            },
            workspace_cursor: 0,
        }
    }

    fn root_crc(page: &mut [u8; BLOCK_BYTES]) {
        page[4092..].fill(0);
        let crc = crc32c(page);
        page[4092..].copy_from_slice(&crc.to_le_bytes());
    }

    fn page_crc(page: &mut [u8; BLOCK_BYTES]) {
        page[64..68].fill(0);
        let crc = crc32c(page);
        page[64..68].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn data_unit_crcs_match_the_published_vectors() {
        let json = include_str!("../testdata/checksum-vectors.json");
        let field = |object: &str, name: &str| -> String {
            let start = object.find(&format!("\"{name}\": ")).expect(name) + name.len() + 4;
            object[start..]
                .split([',', '\n', '}'])
                .next()
                .unwrap()
                .trim()
                .trim_matches('"')
                .to_owned()
        };
        let hex = |text: &str| -> Vec<u8> {
            (0..text.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
                .collect()
        };
        let uuid: [u8; 16] = hex(&field(json, "uuid_hex")).try_into().unwrap();
        let units = &json[json.find("\"data_units\"").unwrap()..];
        let mut checked = 0;
        for object in units.split('{').skip(1) {
            let valid = field(object, "valid_bytes").parse::<usize>().unwrap();
            let payload = match field(object, "payload").as_str() {
                "zeros" => vec![0; valid],
                _ => hex(&field(object, "payload_hex")),
            };
            assert_eq!(payload.len(), valid);
            let expected = u32::from_str_radix(&field(object, "crc32c"), 16).unwrap();
            let actual = data_unit_crc(
                &uuid,
                field(object, "object_id").parse().unwrap(),
                field(object, "unit_index").parse().unwrap(),
                &payload,
            )
            .unwrap();
            assert_eq!(actual, expected, "{object}");
            checked += 1;
        }
        assert_eq!(checked, 5);
        assert!(data_unit_crc(&uuid, 2, 0, &[]).is_err());
        assert!(data_unit_crc(&uuid, 2, 0, &vec![0; UNIT_BYTES + 1]).is_err());
    }

    #[test]
    fn zero_checks_find_any_nonzero_byte() {
        for len in 0..=65 {
            let mut bytes = vec![0; len + 1];
            assert!(all_zero(&bytes[1..]));
            for index in 1..=len {
                for nonzero in [1, 128, 255] {
                    bytes[index] = nonzero;
                    assert!(!all_zero(&bytes[1..]), "len {len}, index {index}");
                }
                bytes[index] = 0;
            }
        }
    }

    #[test]
    fn checksum_slots_round_trip_through_group_validation() {
        let mut group = [0u8; CHECKSUM_GROUP_BYTES];
        let key = |index: u64| {
            let mut key = 2_u64.to_be_bytes().to_vec();
            key.push(4);
            key.extend_from_slice(&index.to_be_bytes());
            key
        };
        set_checksum_slot(&mut group, 0, Some(0));
        set_checksum_slot(&mut group, 255, Some(0xdead_beef));
        let stored = compact_group(&group);
        assert_eq!(stored.len(), CHECKSUM_GROUP_BYTES);
        validate_checksum_group(&key(0), stored, Validation::Disk).unwrap();
        assert_eq!(checksum_slot(stored, 0).unwrap(), Some(0));
        assert_eq!(checksum_slot(stored, 255).unwrap(), Some(0xdead_beef));
        assert_eq!(checksum_slot(stored, 1).unwrap(), None);
        assert!(checksum_slot(stored, 256).is_err());

        set_checksum_slot(&mut group, 255, None);
        let stored = compact_group(&group);
        assert_eq!(stored.len(), 36, "a one-unit group ends after slot 0");
        validate_checksum_group(&key(0), stored, Validation::Disk).unwrap();
        assert_eq!(checksum_slot(stored, 255).unwrap(), None);
        assert_eq!(expand_group(stored), group);
        set_checksum_slot(&mut group, 0, None);
        assert_eq!(group, [0u8; CHECKSUM_GROUP_BYTES]);
        assert!(validate_checksum_group(&key(0), compact_group(&group), Validation::Disk).is_err());
    }

    #[test]
    fn compact_groups_validate_exactly_their_canonical_form() {
        let mut key = 2_u64.to_be_bytes().to_vec();
        key.push(4);
        key.extend_from_slice(&0_u64.to_be_bytes());
        // FORMAT.md, slot by slot: the value ends at its last present slot,
        // no bitmap bit lies beyond it, and holes before it store CRC zero.
        let canonical = |value: &[u8]| {
            let present = |slot: usize| value[slot / 8] & (1 << (slot % 8)) != 0;
            let stored = (value.len() - 32) / 4;
            present(stored - 1)
                && (stored..256).all(|slot| !present(slot))
                && (0..stored)
                    .all(|slot| present(slot) || value[32 + slot * 4..36 + slot * 4] == [0; 4])
        };
        let valid = |value: &[u8]| validate_checksum_group(&key, value, Validation::Disk).is_ok();
        for stored in 1..=256usize {
            for mask in 0..=255u8 {
                let mut group = [0u8; CHECKSUM_GROUP_BYTES];
                group[(stored - 1) / 8] = mask;
                for slot in 0..stored {
                    if group[slot / 8] & (1 << (slot % 8)) != 0 {
                        group[32 + slot * 4] = 0xaa;
                    }
                }
                let value = &group[..32 + 4 * stored];
                let context = format!("stored {stored}, mask {mask:#04x}");
                assert_eq!(valid(value), canonical(value), "{context}");
                if let Some(hole) =
                    (0..stored).find(|slot| value[slot / 8] & (1 << (slot % 8)) == 0)
                {
                    let mut changed = value.to_vec();
                    changed[32 + hole * 4] = 1;
                    assert!(!valid(&changed), "{context}: CRC in hole {hole}");
                }
                if stored < 256 {
                    let mut changed = value.to_vec();
                    changed[stored / 8] |= 1 << (stored % 8);
                    assert!(!valid(&changed), "{context}: bit beyond the stored slots");
                }
                if canonical(value) {
                    assert_eq!(compact_group(&expand_group(value)), value, "{context}");
                }
            }
        }
        for len in [0, 32, 35, 37, 1_057, 1_060] {
            let mut value = vec![0u8; len];
            if let Some(first) = value.first_mut() {
                *first = 1;
            }
            assert!(!valid(&value), "length {len}");
        }
    }

    /// `reserved_group_blocks` names exactly the blocks a per-block scan of the
    /// group would: the first and last groups, a short final group, and the
    /// empty answer for every group between them.
    #[test]
    fn reserved_group_blocks_match_a_per_block_scan() {
        for blocks in [
            RESERVED_TOTAL_BLOCKS,
            ALLOCATION_GROUP_BLOCKS - 1,
            ALLOCATION_GROUP_BLOCKS,
            ALLOCATION_GROUP_BLOCKS + 1,
            3 * ALLOCATION_GROUP_BLOCKS + RESERVED_BLOCKS - 1,
            5 * ALLOCATION_GROUP_BLOCKS,
        ] {
            for group in 0..blocks.div_ceil(ALLOCATION_GROUP_BLOCKS) {
                let first = group * ALLOCATION_GROUP_BLOCKS;
                let scanned: Vec<u64> = (first..first + ALLOCATION_GROUP_BLOCKS)
                    .filter(|block| *block < RESERVED_BLOCKS || *block >= blocks - RESERVED_BLOCKS)
                    .collect();
                let listed: Vec<u64> = reserved_group_blocks(group, blocks).collect();
                assert_eq!(listed, scanned, "group {group} of {blocks} blocks");
            }
        }
    }

    #[test]
    fn crc32c_matches_reference_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(&[0; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[255; 32]), 0x62a8_ab43);
        assert_eq!(crc32c(&(0..32).collect::<Vec<_>>()), 0x46dd_794e);
    }

    #[test]
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn hardware_crc32c_matches_the_software_tables() {
        if !hardware::available() {
            eprintln!("CRC instructions are unavailable; the software path is the only one");
            return;
        }
        let bytes: Vec<u8> = (0..UNIT_BYTES as u32 + 4_200)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        for start in [0, 1, 7, 8, 9, 4093] {
            for len in (0..=71).chain([
                255,
                256,
                767,
                768,
                769,
                1543,
                2304,
                BLOCK_BYTES - 1,
                BLOCK_BYTES,
                UNIT_BYTES,
            ]) {
                let input = &bytes[start..start + len];
                for seed in [0, 0xFFFF_FFFF, 0x1234_5678] {
                    // SAFETY: `available` reported support.
                    let fast = unsafe { hardware::extend(seed, input) };
                    assert_eq!(
                        fast,
                        software_extend(seed, input),
                        "start {start}, length {len}, seed {seed:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_runs_match_checksumming_the_zeros() {
        for len in (0..=300).chain([4095, 4096, 61_440, UNIT_BYTES - 1, UNIT_BYTES]) {
            for seed in [0, 0xFFFF_FFFF, 0x1234_5678] {
                assert_eq!(
                    crc32c_zeros(seed, len),
                    software_extend(seed, &ZEROS[..len]),
                    "length {len}, seed {seed:#x}"
                );
            }
        }
    }

    #[test]
    fn crc32c_matches_bitwise_reference() {
        fn bitwise(bytes: &[u8], zero: std::ops::Range<usize>) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for (index, &byte) in bytes.iter().enumerate() {
                crc ^= u32::from(if zero.contains(&index) { 0 } else { byte });
                for _ in 0..8 {
                    crc = (crc >> 1) ^ if crc & 1 != 0 { 0x82F6_3B78 } else { 0 };
                }
            }
            crc ^ 0xFFFF_FFFF
        }
        let bytes: Vec<u8> = (0..UNIT_BYTES as u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for len in (0..=70).chain([BLOCK_BYTES, UNIT_BYTES]) {
            let input = &bytes[..len];
            assert_eq!(crc32c(input), bitwise(input, 0..0), "length {len}");
            assert_eq!(
                software_extend(0xFFFF_FFFF, input) ^ 0xFFFF_FFFF,
                bitwise(input, 0..0),
                "software length {len}"
            );
        }
        for (start, end) in [(64, 68), (BLOCK_BYTES - 4, BLOCK_BYTES), (0, 4)] {
            let input = &bytes[..BLOCK_BYTES];
            assert_eq!(
                crc32c_with_zero(input, start, end),
                bitwise(input, start..end)
            );
        }
        let uuid = [7; 16];
        for valid in [1, 4095, UNIT_BYTES] {
            let mut framed = b"CFSXDATA".to_vec();
            framed.extend_from_slice(&uuid);
            framed.extend_from_slice(&9u64.to_le_bytes());
            framed.extend_from_slice(&3u64.to_le_bytes());
            framed.extend_from_slice(&(valid as u32).to_le_bytes());
            framed.extend_from_slice(&bytes[..valid]);
            framed.resize(framed.len() + UNIT_BYTES - valid, 0);
            assert_eq!(
                data_unit_crc(&uuid, 9, 3, &bytes[..valid]).unwrap(),
                bitwise(&framed, 0..0)
            );
        }
    }

    #[test]
    #[ignore = "bounded checksum throughput measurement"]
    fn measure_crc32c_throughput() {
        let bytes = vec![0x59; 1 << 20];
        let start = std::time::Instant::now();
        for _ in 0..64 {
            std::hint::black_box(crc32c(std::hint::black_box(&bytes)));
        }
        eprintln!("64 MiB CRC32C: {:?}", start.elapsed());
    }

    #[test]
    #[ignore = "checksum CPU measurement; run in release with --nocapture"]
    fn measure_data_unit_checksum_costs() {
        let bytes = vec![0x59; UNIT_BYTES];
        let uuid = [7; 16];
        for valid in [1, 4096, UNIT_BYTES] {
            // Initialize the zero-padding tables and runtime CPU dispatch.
            let expected = data_unit_crc(&uuid, 9, 3, &bytes[..valid]).unwrap();
            let iterations = 200_000;
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(
                    data_unit_crc(
                        std::hint::black_box(&uuid),
                        9,
                        3,
                        std::hint::black_box(&bytes[..valid]),
                    )
                    .unwrap(),
                );
            }
            eprintln!(
                "checksum\t{valid}\t{iterations}\t{:.6}",
                start.elapsed().as_secs_f64() * 1e9 / f64::from(iterations)
            );
            assert_eq!(
                data_unit_crc(&uuid, 9, 3, &bytes[..valid]).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn root_fixtures_round_trip_byte_for_byte() {
        for (slot, fixture) in [
            (256, include_bytes!("../testdata/vectors/root-a.bin")),
            (
                GEOMETRY.blocks() - 257,
                include_bytes!("../testdata/vectors/root-b.bin"),
            ),
        ] {
            let decoded = Root::decode(fixture, slot, GEOMETRY).unwrap();
            assert_eq!(read_u32_le(fixture, 8, "revision").unwrap(), 1);
            assert_eq!(decoded.encode(slot, GEOMETRY).unwrap().0[..], fixture[..]);
            for revision in [0_u32, 2] {
                let mut unsupported = *fixture;
                unsupported[8..12].copy_from_slice(&revision.to_le_bytes());
                root_crc(&mut unsupported);
                assert!(matches!(
                    Root::decode(&unsupported, slot, GEOMETRY),
                    Err(Error::UnsupportedFormat)
                ));
            }
        }
    }

    #[test]
    fn labeled_root_fixture_and_malformed_labels() {
        let fixture = include_bytes!("../testdata/vectors/labeled-root-a.bin");
        let decoded = Root::decode(fixture, 256, GEOMETRY).unwrap();
        assert_eq!(
            decoded.label.as_ref().unwrap().as_str(),
            "Andrei, Backup \\ 東京"
        );
        assert_eq!(decoded.encode(256, GEOMETRY).unwrap().0[..], fixture[..]);
        for slot in [256, GEOMETRY.blocks() - 257] {
            let mut page = decoded.encode(slot, GEOMETRY).unwrap().0;
            assert_eq!(Root::decode(&page[..], slot, GEOMETRY).unwrap(), decoded);
            for length in [0u16, 256, u16::MAX] {
                page = decoded.encode(slot, GEOMETRY).unwrap().0;
                page[184..186].copy_from_slice(&length.to_le_bytes());
                root_crc(&mut page);
                assert!(matches!(
                    Root::decode(&page[..], slot, GEOMETRY),
                    Err(Error::Corrupt(_))
                ));
            }
            for byte in [0, b'/', b'\n', 0xff] {
                page = decoded.encode(slot, GEOMETRY).unwrap().0;
                page[186] = byte;
                root_crc(&mut page);
                assert!(matches!(
                    Root::decode(&page[..], slot, GEOMETRY),
                    Err(Error::Corrupt(_))
                ));
            }
            for offset in [441, 4091] {
                page = decoded.encode(slot, GEOMETRY).unwrap().0;
                page[offset] = 1;
                root_crc(&mut page);
                assert!(matches!(
                    Root::decode(&page[..], slot, GEOMETRY),
                    Err(Error::Corrupt(_))
                ));
            }
        }
    }

    #[test]
    fn page_fixtures_round_trip_byte_for_byte() {
        let uuid = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let reference = TreeRef {
            block: 1024,
            generation: 5,
        };
        for fixture in [
            include_bytes!("../testdata/vectors/empty-catalog-leaf.bin"),
            include_bytes!("../testdata/vectors/extent-catalog-leaf.bin"),
            include_bytes!("../testdata/vectors/symlink-catalog-leaf.bin"),
            include_bytes!("../testdata/vectors/checksum-catalog-leaf.bin"),
        ] {
            assert_eq!(read_u16_le(fixture, 8, "revision").unwrap(), 1);
            for revision in [0_u16, 2] {
                let mut unsupported = *fixture;
                unsupported[8..10].copy_from_slice(&revision.to_le_bytes());
                page_crc(&mut unsupported);
                assert!(matches!(
                    decode_page(
                        &unsupported,
                        reference,
                        &uuid,
                        TreeKind::Catalog,
                        7,
                        Some(0),
                        GEOMETRY.blocks(),
                    ),
                    Err(Error::UnsupportedFormat)
                ));
            }
            let decoded = decode_page(
                fixture,
                reference,
                &uuid,
                TreeKind::Catalog,
                7,
                Some(0),
                GEOMETRY.blocks(),
            )
            .unwrap();
            assert_eq!(
                encode_leaf(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &decoded.records,
                    GEOMETRY.blocks(),
                )
                .unwrap()
                .0[..],
                fixture[..]
            );
            assert_eq!(
                leaf_data_blocks(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &decoded.records,
                    GEOMETRY.blocks(),
                )
                .unwrap(),
                u64::from_le_bytes(fixture[56..64].try_into().unwrap())
            );
            // A repeated key fails both ways alike.
            if let Some(&record) = decoded.records.first() {
                let repeated = [record, record];
                let encoded = encode_leaf(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &repeated,
                    GEOMETRY.blocks(),
                );
                let totalled = leaf_data_blocks(
                    reference,
                    &uuid,
                    TreeKind::Catalog,
                    &repeated,
                    GEOMETRY.blocks(),
                );
                assert!(encoded.is_err());
                assert_eq!(
                    format!("{:?}", encoded.map(|_| ())),
                    format!("{:?}", totalled.map(|_| ()))
                );
            }
        }
    }

    #[test]
    fn malformed_lengths_and_identity_are_corruption_after_recomputed_crc() {
        let uuid = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let reference = TreeRef {
            block: 1024,
            generation: 5,
        };
        let fixture = *include_bytes!("../testdata/vectors/empty-catalog-leaf.bin");

        let mut malformed = fixture;
        malformed[32..34].copy_from_slice(&1u16.to_le_bytes());
        page_crc(&mut malformed);
        assert!(matches!(
            decode_page(
                &malformed,
                reference,
                &uuid,
                TreeKind::Catalog,
                7,
                Some(0),
                GEOMETRY.blocks(),
            ),
            Err(Error::Corrupt(_))
        ));

        let mut identity = fixture;
        identity[16..24].copy_from_slice(&1025u64.to_le_bytes());
        page_crc(&mut identity);
        assert!(matches!(
            decode_page(
                &identity,
                reference,
                &uuid,
                TreeKind::Catalog,
                7,
                Some(0),
                GEOMETRY.blocks(),
            ),
            Err(Error::Corrupt(_))
        ));

        let mut root = *include_bytes!("../testdata/vectors/root-a.bin");
        root[40..48].copy_from_slice(&257u64.to_le_bytes());
        root_crc(&mut root);
        assert!(matches!(
            Root::decode(&root, 256, GEOMETRY),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn short_inputs_never_panic() {
        for length in 0..BLOCK_BYTES {
            let bytes = vec![0u8; length];
            assert!(Root::decode(&bytes, 256, GEOMETRY).is_err());
            assert!(
                decode_page(
                    &bytes,
                    TreeRef {
                        block: 1024,
                        generation: 1
                    },
                    &UUID,
                    TreeKind::Catalog,
                    1,
                    None,
                    GEOMETRY.blocks(),
                )
                .is_err()
            );
        }
    }

    fn inode(seconds: i64) -> [u8; 128] {
        let mut value = [0; 128];
        value[0] = 1;
        value[8..16].copy_from_slice(&1u64.to_le_bytes());
        for offset in [40, 56, 72] {
            value[offset..offset + 8].copy_from_slice(&seconds.to_le_bytes());
        }
        value
    }

    #[test]
    fn timestamp_and_id_boundaries_are_checked() {
        let key = [0, 0, 0, 0, 0, 0, 0, 2, 1];
        for seconds in [-11_644_473_600, 253_402_300_799] {
            assert!(
                encode_leaf(
                    TreeRef {
                        block: 1024,
                        generation: 1
                    },
                    &UUID,
                    TreeKind::Catalog,
                    &[(&key, &inode(seconds))],
                    GEOMETRY.blocks(),
                )
                .is_ok()
            );
        }
        for seconds in [-11_644_473_601, 253_402_300_800] {
            assert!(
                encode_leaf(
                    TreeRef {
                        block: 1024,
                        generation: 1
                    },
                    &UUID,
                    TreeKind::Catalog,
                    &[(&key, &inode(seconds))],
                    GEOMETRY.blocks(),
                )
                .is_err()
            );
        }

        assert!(validate_owner_id(MAX_ID - 1, Validation::Input).is_ok());
        assert!(validate_owner_id(MAX_ID, Validation::Input).is_err());
    }

    #[test]
    fn symlink_inodes_carry_exactly_their_target() {
        let symlink = |target: &[u8], edit: fn(&mut Vec<u8>)| {
            let mut value = inode(0).to_vec();
            value[0] = 3;
            value[16..24].copy_from_slice(&(target.len() as u64).to_le_bytes());
            value.extend_from_slice(target);
            edit(&mut value);
            validate_inode(&value, GEOMETRY.blocks(), Validation::Disk)
        };
        assert!(symlink(b"../notes.txt", |_| {}).is_ok());
        assert!(symlink(&[b'x'; MAX_SYMLINK_TARGET], |_| {}).is_ok());
        assert!(symlink(b"file link", |value| value[1] = 2).is_ok());
        assert!(symlink(b"directory link", |value| value[1] = 6).is_ok());
        let mut file = inode(0);
        file[1] = 2;
        assert!(validate_inode(&file, GEOMETRY.blocks(), Validation::Disk).is_err());
        for (target, edit) in [
            (&b""[..], (|_| {}) as fn(&mut Vec<u8>)),
            (&[b'x'; MAX_SYMLINK_TARGET + 1], |_| {}),
            (b"a\0b", |_| {}),
            (&[0xff], |_| {}),
            (b"size", |value| value[16] += 1),
            (b"allocated", |value| value[24] = 1),
            (b"children", |value| value[32] = 1),
            (b"reserved", |value| value[100] = 1),
            (b"flags", |value| value[1] = 8),
            (b"kind without record", |value| value[1] = 4),
            // A file or directory inode must end at 128 bytes.
            (b"file", |value| value[0] = 1),
            (b"directory", |value| value[0] = 2),
        ] {
            assert!(
                matches!(symlink(target, edit), Err(Error::Corrupt(_))),
                "{target:?}"
            );
        }
        let mut entry = vec![0; 12];
        entry[..8].copy_from_slice(&2u64.to_le_bytes());
        entry[10..12].copy_from_slice(&1u16.to_le_bytes());
        entry.push(b'a');
        for (kind, valid) in [(1, true), (2, true), (3, true), (4, false)] {
            entry[8] = kind;
            assert_eq!(
                validate_directory_entry(&entry, Validation::Disk).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn extent_and_checksum_boundaries_are_checked() {
        let mut key = [0; 17];
        key[..8].copy_from_slice(&2u64.to_be_bytes());
        key[8] = 3;
        let mut value = [0; 16];
        value[..8].copy_from_slice(&512u64.to_le_bytes());
        value[8..12].copy_from_slice(&1u32.to_le_bytes());

        key[9..17].copy_from_slice(&(MAX_LOGICAL_BLOCK - 1).to_be_bytes());
        assert!(validate_extent(&key, &value, GEOMETRY.blocks(), Validation::Input).is_ok());
        key[9..17].copy_from_slice(&MAX_LOGICAL_BLOCK.to_be_bytes());
        assert!(validate_extent(&key, &value, GEOMETRY.blocks(), Validation::Input).is_err());
        key[9..17].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(validate_extent(&key, &value, GEOMETRY.blocks(), Validation::Input).is_err());

        key[8] = 4;
        key[9..17].fill(0);
        let checksum_group = [0u8; CHECKSUM_GROUP_BYTES];
        assert!(
            encode_leaf(
                TreeRef {
                    block: 1024,
                    generation: 1
                },
                &UUID,
                TreeKind::Catalog,
                &[(&key, &checksum_group)],
                GEOMETRY.blocks(),
            )
            .is_err()
        );
    }

    #[test]
    fn root_free_count_includes_tree_roots() {
        let mut root = root_a();
        root.free_blocks = root.blocks - RESERVED_TOTAL_BLOCKS - 3;
        assert!(root.encode(256, GEOMETRY).is_ok());
        root.free_blocks += 1;
        assert!(root.encode(256, GEOMETRY).is_err());
    }
}
