//! Whole disks, their partition tables, and the CarryFS volumes on them.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

use carryfs_core::{BlockDevice, Error, Geometry, MountMode, Result, has_root_magic};

use crate::scsi::{Disk, Transport};

/// A whole disk addressed in bytes, in whole sectors.
pub trait Sectors: Send {
    fn block_bytes(&self) -> u32;
    fn byte_len(&self) -> u64;
    fn read_only(&self) -> bool;
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

impl<T: Transport> Sectors for Disk<T> {
    fn block_bytes(&self) -> u32 {
        Disk::block_bytes(self)
    }
    fn byte_len(&self) -> u64 {
        self.blocks() * u64::from(Disk::block_bytes(self))
    }
    fn read_only(&self) -> bool {
        self.write_protected()
    }
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        Disk::read_at(self, offset, out)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        Disk::write_at(self, offset, data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Disk::flush(self)
    }
}

/// A disk image file, standing in for a USB disk on the emulator.
pub struct ImageFile {
    file: File,
    len: u64,
}

impl ImageFile {
    pub fn new(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl Sectors for ImageFile {
    fn block_bytes(&self) -> u32 {
        512
    }
    fn byte_len(&self) -> u64 {
        self.len
    }
    fn read_only(&self) -> bool {
        false
    }
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(out, offset)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write_all_at(data, offset)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

pub type SharedDisk = Arc<Mutex<dyn Sectors>>;

/// The most bytes held back to go out as one write. The core writes file data
/// a 4 KiB page at a time, and each USB command costs a round trip, so sending
/// pages one by one caps writes near 2 MB/s.
const COALESCE_BYTES: usize = 1 << 20;

/// Recent reads kept for exact repeats: apps often read one file through two handles.
const RECENT_READS: usize = 8;
const RECENT_READ_BYTES: usize = 64 << 10;

/// A byte range of a disk that holds a CarryFS volume.
#[derive(Clone)]
pub struct Partition {
    disk: SharedDisk,
    start: u64,
    geometry: Geometry,
    mode: MountMode,
    /// Consecutive writes not yet sent, starting at `pending_at`.
    pending: Vec<u8>,
    pending_at: u64,
    /// Recent reads by offset, oldest first. Any write clears them.
    recent: Vec<(u64, Vec<u8>)>,
}

impl Partition {
    fn new(disk: &SharedDisk, start: u64, byte_len: u64) -> Self {
        let (block_bytes, read_only) = {
            let disk = disk.lock().unwrap();
            (disk.block_bytes(), disk.read_only())
        };
        Self {
            disk: disk.clone(),
            start,
            geometry: Geometry {
                byte_len,
                logical_sector_bytes: block_bytes,
                physical_sector_bytes: 4096,
            },
            mode: if read_only {
                MountMode::ReadOnly
            } else {
                MountMode::ReadWrite
            },
            pending: Vec::new(),
            pending_at: 0,
            recent: Vec::new(),
        }
    }

    /// The same range, refusing writes.
    pub fn read_only(self) -> Self {
        Self {
            mode: MountMode::ReadOnly,
            ..self
        }
    }

    fn disk(&self) -> Result<std::sync::MutexGuard<'_, dyn Sectors + 'static>> {
        self.disk.lock().map_err(|_| Error::Faulted)
    }

    fn range(&self, offset: u64, len: usize) -> Result<u64> {
        offset
            .checked_add(len as u64)
            .filter(|end| *end <= self.geometry.byte_len)
            .map(|_| self.start + offset)
            .ok_or(Error::InvalidInput("access is outside the partition"))
    }

    /// Write out the held-back bytes.
    fn send(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let at = self.range(self.pending_at, self.pending.len())?;
        let result = self
            .disk()?
            .write_at(at, &self.pending)
            .map_err(usb_error("write disk"));
        self.pending.clear();
        result
    }

    fn pending_overlaps(&self, offset: u64, len: usize) -> bool {
        let end = self.pending_at + self.pending.len() as u64;
        !self.pending.is_empty() && offset < end && offset + len as u64 > self.pending_at
    }
}

fn usb_error(operation: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::Io { operation, source }
}

impl BlockDevice for Partition {
    fn mode(&self) -> MountMode {
        self.mode
    }

    fn geometry(&mut self) -> Result<Geometry> {
        Ok(self.geometry)
    }

    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        let at = self.range(offset, out.len())?;
        if let Some((_, data)) = self
            .recent
            .iter()
            .find(|(start, data)| *start == offset && data.len() == out.len())
        {
            out.copy_from_slice(data);
            return Ok(());
        }
        if self.pending_overlaps(offset, out.len()) {
            self.send()?;
        }
        self.disk()?
            .read_at(at, out)
            .map_err(usb_error("read disk"))?;
        if out.len() <= RECENT_READ_BYTES {
            if self.recent.len() == RECENT_READS {
                self.recent.remove(0);
            }
            self.recent.push((offset, out.to_vec()));
        }
        Ok(())
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.mode == MountMode::ReadOnly {
            return Err(Error::ReadOnly);
        }
        self.range(offset, data.len())?;
        self.recent.clear();
        let follows = offset == self.pending_at + self.pending.len() as u64;
        if !follows || self.pending.len() + data.len() > COALESCE_BYTES {
            self.send()?;
            self.pending_at = offset;
        }
        self.pending.extend_from_slice(data);
        Ok(())
    }

    fn flush_durable(&mut self) -> Result<()> {
        self.send()?;
        self.disk()?.flush().map_err(usb_error("flush disk"))
    }
}

/// Every CarryFS volume on the disk: in a GPT or MBR partition, or on the
/// whole disk.
pub fn volumes(disk: &SharedDisk) -> io::Result<Vec<Partition>> {
    let mut found = Vec::new();
    let (ranges, read_only) = partitions(disk)?;
    for (start, len) in ranges {
        let mut partition = Partition::new(disk, start, len);
        if read_only {
            partition = partition.read_only();
        }
        if has_root_magic(&mut partition) {
            found.push(partition);
        }
    }
    Ok(found)
}

/// Candidate byte ranges and whether a damaged GPT forces read-only access.
type Ranges = Vec<(u64, u64)>;

fn partitions(disk: &SharedDisk) -> io::Result<(Ranges, bool)> {
    let mut disk = disk
        .lock()
        .map_err(|_| io::Error::other("disk lock poisoned"))?;
    let block = u64::from(disk.block_bytes());
    let disk_len = disk.byte_len();
    let mut sector = vec![0; block as usize];
    disk.read_at(0, &mut sector)?;

    if sector[510..512] != [0x55, 0xaa] {
        return Ok((vec![(0, disk_len)], false));
    }
    let entries = || sector[446..510].chunks_exact(16);
    if entries().any(|entry| entry[4] == 0xee) {
        return gpt(&mut *disk, block);
    }
    let mut ranges = Vec::new();
    let mut primary = Vec::new();
    for entry in entries().filter(|entry| entry[4] != 0) {
        let first = u64::from(le32(entry, 8));
        let count = u64::from(le32(entry, 12));
        if first == 0 || count == 0 || first + count > disk_len / block {
            return Err(invalid_map("MBR partition is outside the disk"));
        }
        let range = (first * block, count * block);
        ranges.push(range);
        // Extended partitions reserve space, but their logical partitions are unsupported.
        if !matches!(entry[4], 0x05 | 0x0f | 0x85) {
            primary.push(range);
        }
    }
    validate_disjoint(&mut ranges)?;
    Ok((primary, false))
}

#[derive(Debug, PartialEq, Eq)]
struct Gpt {
    disk_guid: [u8; 16],
    first_usable: u64,
    last_usable: u64,
    count: u32,
    size: u32,
    entries_crc: u32,
    ranges: Ranges,
}

fn gpt(disk: &mut dyn Sectors, block: u64) -> io::Result<(Ranges, bool)> {
    let last = disk
        .byte_len()
        .checked_div(block)
        .and_then(|sectors| sectors.checked_sub(1))
        .filter(|last| *last > 2)
        .ok_or_else(|| invalid_map("Disk is too small for GPT"))?;
    match (
        gpt_copy(disk, block, 1, last),
        gpt_copy(disk, block, last, 1),
    ) {
        (Ok(primary), Ok(backup)) if primary == backup => Ok((primary.ranges, false)),
        (Ok(_), Ok(_)) => Err(invalid_map("Primary and backup GPT disagree")),
        (Ok(map), Err(_)) | (Err(_), Ok(map)) => Ok((map.ranges, true)),
        (Err(primary), Err(backup)) => Err(invalid_map(format!(
            "Both GPT copies are invalid (primary: {primary}; backup: {backup})"
        ))),
    }
}

fn gpt_copy(disk: &mut dyn Sectors, block: u64, at: u64, alternate: u64) -> io::Result<Gpt> {
    let mut header = vec![0; block as usize];
    disk.read_at(at * block, &mut header)?;
    let header_len = le32(&header, 12) as usize;
    if &header[0..8] != b"EFI PART"
        || le32(&header, 8) != 0x0001_0000
        || !(92..=header.len()).contains(&header_len)
        || le64(&header, 24) != at
        || le64(&header, 32) != alternate
    {
        return Err(invalid_map("GPT header fields are invalid"));
    }
    let expected = le32(&header, 16);
    header[16..20].fill(0);
    if gpt_crc(&header[..header_len]) != expected {
        return Err(invalid_map("GPT header checksum mismatch"));
    }

    let first_usable = le64(&header, 40);
    let last_usable = le64(&header, 48);
    let disk_guid = header[56..72].try_into().unwrap();
    let table = le64(&header, 72);
    let count = le32(&header, 80);
    let size = le32(&header, 84);
    let entries_crc = le32(&header, 88);
    if !(1..=1024).contains(&count) || !(128..=4096).contains(&size) || !size.is_power_of_two() {
        return Err(invalid_map("Unsupported GPT entry count or size"));
    }
    let bytes = u64::from(count) * u64::from(size);
    let sectors = bytes.div_ceil(block);
    let last_header = at.max(alternate);
    let table_end = table.checked_add(sectors);
    let table_fits = match (at, table_end) {
        (1, Some(end)) => table >= 2 && end <= first_usable,
        (_, Some(end)) => table > last_usable && end <= last_header,
        (_, None) => false,
    };
    if first_usable < 2 || first_usable > last_usable || last_usable >= last_header || !table_fits {
        return Err(invalid_map("GPT metadata is outside the reserved region"));
    }

    let mut entries = Vec::new();
    let rounded = (sectors * block) as usize;
    entries
        .try_reserve_exact(rounded)
        .map_err(io::Error::other)?;
    entries.resize(rounded, 0);
    disk.read_at(table * block, &mut entries)?;
    let entries = &entries[..bytes as usize];
    if gpt_crc(entries) != entries_crc {
        return Err(invalid_map("GPT entry array checksum mismatch"));
    }

    let mut ranges = Vec::new();
    for entry in entries.chunks_exact(size as usize) {
        if entry[..16] == [0; 16] {
            continue;
        }
        let first = le64(entry, 32);
        let last = le64(entry, 40);
        if first < first_usable || first > last || last > last_usable {
            return Err(invalid_map("GPT partition is outside the usable range"));
        }
        ranges.push((first * block, (last - first + 1) * block));
    }
    validate_disjoint(&mut ranges)?;
    Ok(Gpt {
        disk_guid,
        first_usable,
        last_usable,
        count,
        size,
        entries_crc,
        ranges,
    })
}

fn validate_disjoint(ranges: &mut [(u64, u64)]) -> io::Result<()> {
    ranges.sort_unstable_by_key(|&(start, _)| start);
    if ranges
        .windows(2)
        .any(|pair| pair[0].0 + pair[0].1 > pair[1].0)
    {
        return Err(invalid_map("Partition ranges overlap"));
    }
    Ok(())
}

fn invalid_map(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

// GPT uses IEEE CRC-32; the CarryFS on-disk metadata uses CRC-32C.
fn gpt_crc(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::scsi::fake::FakeUsb;
    use carryfs_core::{Volume, erase_reserved_bands};

    pub const MIB: u64 = 1 << 20;

    pub fn usb_disk(bytes: u64) -> SharedDisk {
        Arc::new(Mutex::new(Disk::open(FakeUsb::new(512, bytes)).unwrap()))
    }

    /// Write a GPT with one partition per range, in 512-byte sectors.
    pub fn write_gpt(disk: &SharedDisk, ranges: &[(u64, u64)]) {
        let mut disk = disk.lock().unwrap();
        let mut mbr = vec![0; 512];
        mbr[446 + 4] = 0xee;
        mbr[510] = 0x55;
        mbr[511] = 0xaa;
        disk.write_at(0, &mbr).unwrap();
        let last = disk.byte_len() / 512 - 1;
        let mut entries = vec![0; 128 * 128];
        for (entry, (start, len)) in entries.chunks_mut(128).zip(ranges) {
            entry[0] = 1;
            entry[32..40].copy_from_slice(&(start / 512).to_le_bytes());
            entry[40..48].copy_from_slice(&((start + len) / 512 - 1).to_le_bytes());
        }
        for (at, alternate, table) in [(1u64, last, 2u64), (last, 1, last - 32)] {
            let mut header = [0; 512];
            header[0..8].copy_from_slice(b"EFI PART");
            header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            header[12..16].copy_from_slice(&92u32.to_le_bytes());
            header[24..32].copy_from_slice(&at.to_le_bytes());
            header[32..40].copy_from_slice(&alternate.to_le_bytes());
            header[40..48].copy_from_slice(&34u64.to_le_bytes());
            header[48..56].copy_from_slice(&(last - 33).to_le_bytes());
            header[56] = 1;
            header[72..80].copy_from_slice(&table.to_le_bytes());
            header[80..84].copy_from_slice(&128u32.to_le_bytes());
            header[84..88].copy_from_slice(&128u32.to_le_bytes());
            header[88..92].copy_from_slice(&gpt_crc(&entries).to_le_bytes());
            let crc = gpt_crc(&header[..92]);
            header[16..20].copy_from_slice(&crc.to_le_bytes());
            disk.write_at(at * 512, &header).unwrap();
            disk.write_at(table * 512, &entries).unwrap();
        }
    }

    pub fn format(disk: &SharedDisk, start: u64, len: u64) {
        let mut partition = Partition::new(disk, start, len);
        erase_reserved_bands(&mut partition).unwrap();
        Volume::format(partition).unwrap().eject().unwrap();
    }

    #[test]
    fn consecutive_writes_go_out_together() {
        let usb = FakeUsb::new(512, 64 * MIB);
        let media = usb.media.clone();
        let disk: SharedDisk = Arc::new(Mutex::new(Disk::open(usb).unwrap()));
        let mut partition = Partition::new(&disk, MIB, 32 * MIB);
        let writes = || media.lock().unwrap().writes;

        for page in 0..16u8 {
            partition
                .write_all_at(u64::from(page) * 4096, &[page; 4096])
                .unwrap();
        }
        assert_eq!(writes(), 0);
        // A read of held-back bytes sends them first, in as few commands as the drive takes.
        let run = (16 * 4096usize).div_ceil(crate::scsi::MAX_WRITE);
        let mut back = [0; 4096];
        partition.read_exact_at(5 * 4096, &mut back).unwrap();
        assert_eq!((back, writes()), ([5; 4096], run));

        partition.write_all_at(100 * 4096, &[1; 4096]).unwrap();
        partition.write_all_at(50 * 4096, &[2; 4096]).unwrap();
        assert_eq!(writes(), run + 1);
        partition.flush_durable().unwrap();
        assert_eq!((writes(), media.lock().unwrap().flushes), (run + 2, 1));
    }

    #[test]
    fn repeated_reads_reach_the_disk_once_until_a_write() {
        let usb = FakeUsb::new(512, 64 * MIB);
        let media = usb.media.clone();
        let disk: SharedDisk = Arc::new(Mutex::new(Disk::open(usb).unwrap()));
        let mut partition = Partition::new(&disk, MIB, 32 * MIB);
        partition.write_all_at(0, &[1; 8192]).unwrap();
        partition.flush_durable().unwrap();
        let reads = || media.lock().unwrap().reads;

        let mut out = [0; 4096];
        let before = reads();
        for _ in 0..3 {
            partition.read_exact_at(4096, &mut out).unwrap();
        }
        assert_eq!((out, reads() - before), ([1; 4096], 1));

        partition.write_all_at(4096, &[2; 4096]).unwrap();
        partition.read_exact_at(4096, &mut out).unwrap();
        assert_eq!((out, reads() - before), ([2; 4096], 2));
    }

    #[test]
    fn finds_volumes_in_gpt_partitions_only() {
        let disk = usb_disk(1100 * MIB);
        let ranges = [
            (MIB, 300 * MIB),
            (301 * MIB, 300 * MIB),
            (601 * MIB, 400 * MIB),
        ];
        write_gpt(&disk, &ranges);
        format(&disk, ranges[0].0, ranges[0].1);
        format(&disk, ranges[2].0, ranges[2].1);

        let found: Vec<_> = volumes(&disk).unwrap().iter().map(|p| p.start).collect();
        assert_eq!(found, [ranges[0].0, ranges[2].0]);
    }

    #[test]
    fn refuses_overlapping_gpt_partitions() {
        let disk = usb_disk(700 * MIB);
        write_gpt(&disk, &[(MIB, 300 * MIB), (2 * MIB, 300 * MIB)]);
        assert!(matches!(partitions(&disk), Err(error) if error.kind() == ErrorKind::InvalidData));
    }

    #[test]
    fn a_damaged_primary_table_leaves_the_backup_read_only() {
        let disk = usb_disk(350 * MIB);
        write_gpt(&disk, &[(MIB, 300 * MIB)]);
        format(&disk, MIB, 300 * MIB);
        disk.lock().unwrap().write_at(512, &[0; 512]).unwrap();
        let found = volumes(&disk).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mode(), MountMode::ReadOnly);
    }

    #[test]
    fn gpt_checksum_matches_the_ieee_check_value() {
        assert_eq!(gpt_crc(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn finds_a_volume_on_the_whole_disk() {
        let disk = usb_disk(300 * MIB);
        format(&disk, 0, 300 * MIB);
        let found = volumes(&disk).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].geometry.byte_len, 300 * MIB);
    }

    #[test]
    fn reads_mbr_primary_partitions() {
        let disk = usb_disk(700 * MIB);
        {
            let mut disk = disk.lock().unwrap();
            let mut mbr = vec![0; 512];
            for (i, (start, len)) in [(MIB, 300 * MIB), (301 * MIB, 300 * MIB)]
                .iter()
                .enumerate()
            {
                let entry = &mut mbr[446 + i * 16..462 + i * 16];
                entry[4] = 0x83;
                entry[8..12].copy_from_slice(&((start / 512) as u32).to_le_bytes());
                entry[12..16].copy_from_slice(&((len / 512) as u32).to_le_bytes());
            }
            mbr[510] = 0x55;
            mbr[511] = 0xaa;
            disk.write_at(0, &mbr).unwrap();
        }
        format(&disk, 301 * MIB, 300 * MIB);
        let found: Vec<_> = volumes(&disk).unwrap().iter().map(|p| p.start).collect();
        assert_eq!(found, [301 * MIB]);
    }
}
