//! CRC32C with the parameters of FORMAT.md section 2.
//!
//! `extend` is the raw reflected register update; callers apply the initial
//! and final inversion. The std build uses SSE4.2 or ARMv8 CRC instructions
//! when the processor has them, the Linux kernel module calls the kernel's
//! `crc32c`, and other no_std builds use the table below.

use super::error::{Error, Result};

/// Bytes in one logical data checksum unit.
pub const UNIT_BYTES: usize = 65_536;

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
#[cfg(all(feature = "std", target_arch = "aarch64"))]
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
#[cfg(all(feature = "std", target_arch = "x86_64"))]
mod hardware {
    use std::sync::atomic::{AtomicU8, Ordering};

    const UNKNOWN: u8 = 2;
    static SUPPORTED: AtomicU8 = AtomicU8::new(UNKNOWN);

    pub(super) fn available() -> bool {
        match SUPPORTED.load(Ordering::Relaxed) {
            0 => false,
            1 => true,
            _ => {
                // Miri cannot query the processor, but runs the intrinsics:
                // a build with SSE4.2 enabled checks the path below under it.
                let detected = if cfg!(miri) {
                    cfg!(target_feature = "sse4.2")
                } else {
                    is_x86_feature_detected!("sse4.2")
                };
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

/// The reflected CRC32C register after `bytes`, without pre- or post-inversion.
#[cfg(not(MODULE))]
pub fn extend(crc: u32, bytes: &[u8]) -> u32 {
    #[cfg(all(feature = "std", target_arch = "aarch64"))]
    if hardware::available() {
        // SAFETY: `available` reported CRC support on this processor.
        return unsafe { hardware::extend(crc, bytes) };
    }
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    if hardware::available() {
        // SAFETY: `available` reported SSE4.2 support on this processor.
        return unsafe { hardware::extend(crc, bytes) };
    }
    software_extend(crc, bytes)
}

/// The reflected CRC32C register after `bytes`, through the kernel's
/// architecture-optimized `crc32c`, which may use vector registers safely.
#[cfg(MODULE)]
pub fn extend(crc: u32, bytes: &[u8]) -> u32 {
    unsafe extern "C" {
        fn carryfs_crc32c(crc: u32, data: *const u8, len: usize) -> u32;
    }
    // SAFETY: `bytes` is a valid slice for the duration of the call, and the
    // helper only reads `len` bytes from `data`.
    unsafe { carryfs_crc32c(crc, bytes.as_ptr(), bytes.len()) }
}

#[cfg_attr(MODULE, allow(dead_code))]
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
pub fn crc32c(bytes: &[u8]) -> u32 {
    extend(0xFFFF_FFFF, bytes) ^ 0xFFFF_FFFF
}

static ZEROS: [u8; UNIT_BYTES] = [0; UNIT_BYTES];

/// CRC32C of one backed data unit. `valid` holds its valid bytes; the rest of
/// the 64 KiB unit contributes zeros, as FORMAT.md section 5 specifies.
pub fn data_unit_crc(uuid: &[u8; 16], object: u64, unit: u64, valid: &[u8]) -> Result<u32> {
    let mut crc = DataUnitCrc::new(uuid, object, unit, valid.len())?;
    crc.update(valid);
    crc.finish()
}

/// `data_unit_crc` over valid bytes supplied in pieces, such as the pages of
/// a unit that are not contiguous in memory.
pub struct DataUnitCrc {
    crc: u32,
    valid: usize,
    seen: usize,
}

impl DataUnitCrc {
    /// Start the checksum of `unit` of `object` holding `valid` valid bytes.
    pub fn new(uuid: &[u8; 16], object: u64, unit: u64, valid: usize) -> Result<Self> {
        let valid_len = u32::try_from(valid)
            .ok()
            .filter(|len| (1..=UNIT_BYTES as u32).contains(len))
            .ok_or(Error::InvalidInput("data unit valid length"))?;
        let mut crc = extend(0xFFFF_FFFF, b"CFSXDATA");
        for field in [
            &uuid[..],
            &object.to_le_bytes(),
            &unit.to_le_bytes(),
            &valid_len.to_le_bytes(),
        ] {
            crc = extend(crc, field);
        }
        Ok(Self {
            crc,
            valid,
            seen: 0,
        })
    }

    /// Add the next valid bytes.
    pub fn update(&mut self, bytes: &[u8]) {
        self.crc = extend(self.crc, bytes);
        self.seen = self.seen.saturating_add(bytes.len());
    }

    /// The checksum, once exactly the valid bytes were added.
    pub fn finish(self) -> Result<u32> {
        if self.seen != self.valid {
            return Err(Error::InvalidInput("data unit valid length"));
        }
        Ok(extend_zeros(self.crc, UNIT_BYTES - self.valid) ^ 0xFFFF_FFFF)
    }
}

#[cfg(feature = "std")]
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

#[cfg(feature = "std")]
fn apply_shift(table: &[[u32; 256]; 4], crc: u32) -> u32 {
    let [a, b, c, d] = crc.to_le_bytes();
    table[0][usize::from(a)]
        ^ table[1][usize::from(b)]
        ^ table[2][usize::from(c)]
        ^ table[3][usize::from(d)]
}

/// `extend(crc, &[0; len])` without reading any zeros: a 4 KiB write
/// otherwise checksums the 60 KiB of zeros that pad its unit.
#[cfg(feature = "std")]
fn extend_zeros(crc: u32, len: usize) -> u32 {
    if len > UNIT_BYTES {
        return extend(crc, &std::vec![0; len]);
    }
    zero_shifts()
        .iter()
        .enumerate()
        .filter(|(bit, _)| len >> bit & 1 == 1)
        .fold(crc, |crc, (_, table)| apply_shift(table, crc))
}

/// `extend(crc, &[0; len])`. Only a file's last unit has padding, so reading
/// the zeros costs little here.
#[cfg(not(feature = "std"))]
fn extend_zeros(mut crc: u32, mut len: usize) -> u32 {
    while len > 0 {
        let step = len.min(UNIT_BYTES);
        crc = extend(crc, &ZEROS[..step]);
        len -= step;
    }
    crc
}

/// CRC32C of `bytes` with `zero_start..zero_end` read as zeros. Callers pass
/// constant ranges shorter than `ZEROS` that lie inside `bytes`.
pub(crate) fn crc32c_with_zero(bytes: &[u8], zero_start: usize, zero_end: usize) -> u32 {
    let end = zero_end.min(bytes.len());
    let start = zero_start.min(end);
    let crc = extend(0xFFFF_FFFF, &bytes[..start]);
    let crc = extend(crc, &ZEROS[..end - start]);
    extend(crc, &bytes[end..]) ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;
    use std::{eprintln, vec};

    use super::*;
    use crate::format::BLOCK_BYTES;

    #[test]
    fn crc32c_matches_reference_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(&[0; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[255; 32]), 0x62a8_ab43);
        assert_eq!(crc32c(&(0..32).collect::<Vec<_>>()), 0x46dd_794e);
    }

    #[test]
    #[cfg(all(feature = "std", any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn hardware_crc32c_matches_the_software_tables() {
        if !hardware::available() {
            // scripts/check-miri.sh enables SSE4.2 for this test, so that Miri
            // checks the intrinsics rather than skip them.
            if cfg!(miri) {
                panic!("run Miri with -C target-feature=+sse4.2,+pclmulqdq");
            }
            eprintln!("CRC instructions are unavailable; the software path is the only one");
            return;
        }
        let bytes: Vec<u8> = (0..UNIT_BYTES as u32 + 4_200)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        // Miri interprets every instruction: under it, short lengths and each
        // length where the hardware loop changes stride still reach every path.
        let short = if cfg!(miri) { 17 } else { 71 };
        let long: &[usize] = if cfg!(miri) {
            &[255, 256, 767, 768, 769, 1543, 2304]
        } else {
            &[
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
            ]
        };
        for start in [0, 1, 7, 8, 9, 4093] {
            for len in (0..=short).chain(long.iter().copied()) {
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
                    extend_zeros(seed, len),
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
}
