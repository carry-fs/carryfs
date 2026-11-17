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
        if self.pending_overlaps(offset, out.len()) {
            self.send()?;
        }
        self.disk()?
            .read_at(at, out)
            .map_err(usb_error("read disk"))
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.mode == MountMode::ReadOnly {
            return Err(Error::ReadOnly);
        }
        self.range(offset, data.len())?;
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
    for (start, len) in partitions(disk)? {
        let mut partition = Partition::new(disk, start, len);
        if has_root_magic(&mut partition) {
            found.push(partition);
        }
    }
    Ok(found)
}

/// Candidate byte ranges: the partitions, or the whole disk if it has none.
fn partitions(disk: &SharedDisk) -> io::Result<Vec<(u64, u64)>> {
    let mut disk = disk
        .lock()
        .map_err(|_| io::Error::other("disk lock poisoned"))?;
    let block = u64::from(disk.block_bytes());
    let disk_len = disk.byte_len();
    let mut sector = vec![0; block as usize];
    disk.read_at(0, &mut sector)?;

    let mut ranges = Vec::new();
    if sector[510..512] == [0x55, 0xaa] {
        let entries: Vec<&[u8]> = sector[446..510].chunks(16).collect();
        if entries.iter().any(|entry| entry[4] == 0xee) {
            ranges = gpt(&mut *disk, block)?;
        } else {
            // ponytail: primary partitions only; read the extended chain if users need it.
            for entry in entries {
                let kind = entry[4];
                let first = u32::from_le_bytes(entry[8..12].try_into().unwrap());
                let count = u32::from_le_bytes(entry[12..16].try_into().unwrap());
                if kind != 0 && !matches!(kind, 0x05 | 0x0f | 0x85) && count != 0 {
                    ranges.push((u64::from(first) * block, u64::from(count) * block));
                }
            }
        }
    }
    ranges.retain(|(start, len)| start.checked_add(*len).is_some_and(|end| end <= disk_len));
    // A volume on the whole disk zeroes the first sector, so it has no table.
    if ranges.is_empty() {
        ranges.push((0, disk_len));
    }
    Ok(ranges)
}

fn gpt(disk: &mut dyn Sectors, block: u64) -> io::Result<Vec<(u64, u64)>> {
    let mut header = vec![0; block as usize];
    disk.read_at(block, &mut header)?;
    if &header[0..8] != b"EFI PART" {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "protective MBR without a GPT header",
        ));
    }
    let table = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as u64;
    let size = u32::from_le_bytes(header[84..88].try_into().unwrap()) as u64;
    if size < 128 || count > 1024 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "GPT header is invalid",
        ));
    }
    let bytes = (count * size).div_ceil(block) * block;
    let mut entries = vec![0; bytes as usize];
    disk.read_at(table * block, &mut entries)?;
    Ok(entries
        .chunks(size as usize)
        .take(count as usize)
        .filter(|entry| entry[0..16] != [0; 16])
        .filter_map(|entry| {
            let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
            (last >= first).then(|| (first * block, (last - first + 1) * block))
        })
        .collect())
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
        let mut header = vec![0; 512];
        header[0..8].copy_from_slice(b"EFI PART");
        header[72..80].copy_from_slice(&2u64.to_le_bytes());
        header[80..84].copy_from_slice(&128u32.to_le_bytes());
        header[84..88].copy_from_slice(&128u32.to_le_bytes());
        disk.write_at(512, &header).unwrap();
        let mut entries = vec![0; 128 * 128];
        for (entry, (start, len)) in entries.chunks_mut(128).zip(ranges) {
            entry[0] = 1;
            entry[32..40].copy_from_slice(&(start / 512).to_le_bytes());
            entry[40..48].copy_from_slice(&((start + len) / 512 - 1).to_le_bytes());
        }
        disk.write_at(1024, &entries).unwrap();
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
