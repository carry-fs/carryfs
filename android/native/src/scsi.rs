//! USB mass storage over the Bulk-Only Transport, using the SCSI block
//! commands every USB drive and card reader supports.

use std::io::{self, ErrorKind};
use std::thread;
use std::time::Duration;

/// Endpoint direction, for clearing a stall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    In,
    Out,
}

/// The bulk pipes of one claimed mass-storage interface. A stall is reported
/// as `ErrorKind::BrokenPipe`, which is how usbfs returns `EPIPE`.
pub trait Transport: Send {
    fn bulk_out(&mut self, data: &[u8]) -> io::Result<usize>;
    fn bulk_in(&mut self, data: &mut [u8]) -> io::Result<usize>;
    fn clear_halt(&mut self, endpoint: Endpoint) -> io::Result<()>;
    /// The Bulk-Only Mass Storage Reset class request.
    fn reset(&mut self) -> io::Result<()>;
}

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
// The most bytes one command moves; bigger commands cost fewer round trips. A
// Lexar stick dropped off the bus under sustained 256 KiB READs, and under any
// single bulk OUT transfer of 64 KiB or more, so a WRITE's data goes out in
// pieces. ponytail: fixed; tune per drive if one proves faster.
const MAX_READ: usize = 64 << 10;
pub(crate) const MAX_WRITE: usize = 256 << 10;
const OUT_PIECE: usize = 16 << 10;

const NOT_READY: u8 = 0x2;
const ILLEGAL_REQUEST: u8 = 0x5;
const UNIT_ATTENTION: u8 = 0x6;

enum Data<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

enum Failure {
    Io(io::Error),
    Sense { key: u8, asc: u8, ascq: u8 },
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Failure::Io(error)
    }
}

impl From<Failure> for io::Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Io(error) => error,
            Failure::Sense { key, asc, ascq } => io::Error::other(format!(
                "SCSI command failed: sense key {key:#x}, ASC {asc:#04x}, ASCQ {ascq:#04x}"
            )),
        }
    }
}

/// LUN 0 of a USB mass-storage device.
pub struct Disk<T> {
    usb: T,
    tag: u32,
    block_bytes: u32,
    blocks: u64,
    write_protected: bool,
}

impl<T: Transport> Disk<T> {
    /// Identify the device, wait for its medium, and read its capacity.
    pub fn open(usb: T) -> io::Result<Self> {
        let mut disk = Self {
            usb,
            tag: 0,
            block_bytes: 0,
            blocks: 0,
            write_protected: false,
        };
        let mut inquiry = [0; 36];
        disk.run(&[0x12, 0, 0, 0, 36, 0], Data::In(&mut inquiry))?;
        if inquiry[0] & 0x1f != 0 {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "USB device is not a direct-access disk",
            ));
        }
        disk.wait_ready()?;
        disk.read_capacity()?;
        // Some devices reject MODE SENSE; they are treated as writable.
        disk.write_protected = disk.mode_sense_write_protect().unwrap_or(false);
        Ok(disk)
    }

    pub fn block_bytes(&self) -> u32 {
        self.block_bytes
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn write_protected(&self) -> bool {
        self.write_protected
    }

    pub fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.check(offset, out.len())?;
        for (i, chunk) in out.chunks_mut(MAX_READ).enumerate() {
            let (cdb, len) = self.rw_cdb(false, offset + (i * MAX_READ) as u64, chunk.len());
            let wanted = chunk.len();
            if self.run(&cdb[..len], Data::In(chunk))? != wanted {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "USB read was short",
                ));
            }
        }
        Ok(())
    }

    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.check(offset, data.len())?;
        for (i, chunk) in data.chunks(MAX_WRITE).enumerate() {
            let (cdb, len) = self.rw_cdb(true, offset + (i * MAX_WRITE) as u64, chunk.len());
            if self.run(&cdb[..len], Data::Out(chunk))? != chunk.len() {
                return Err(io::Error::new(ErrorKind::WriteZero, "USB write was short"));
            }
        }
        Ok(())
    }

    /// SYNCHRONIZE CACHE: the device's durability barrier. Drives without a
    /// write cache may reject the command; Linux sd accepts that too.
    pub fn flush(&mut self) -> io::Result<()> {
        match self.run(&[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], Data::None) {
            Ok(_)
            | Err(Failure::Sense {
                key: ILLEGAL_REQUEST,
                asc: 0x20 | 0x24,
                ..
            }) => Ok(()),
            Err(failure) => Err(failure.into()),
        }
    }

    fn check(&self, offset: u64, len: usize) -> io::Result<()> {
        let block = u64::from(self.block_bytes);
        let end = offset.checked_add(len as u64);
        if !offset.is_multiple_of(block)
            || !(len as u64).is_multiple_of(block)
            || end.is_none_or(|end| end > self.blocks * block)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "USB transfer is unaligned or outside the disk",
            ));
        }
        Ok(())
    }

    /// READ or WRITE (10), or (16) when the range passes 2^32 blocks.
    fn rw_cdb(&self, write: bool, offset: u64, len: usize) -> ([u8; 16], usize) {
        let lba = offset / u64::from(self.block_bytes);
        let count = (len / self.block_bytes as usize) as u32;
        let mut cdb = [0; 16];
        if lba + u64::from(count) <= u64::from(u32::MAX) {
            cdb[0] = if write { 0x2a } else { 0x28 };
            cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
            cdb[7..9].copy_from_slice(&(count as u16).to_be_bytes());
            (cdb, 10)
        } else {
            cdb[0] = if write { 0x8a } else { 0x88 };
            cdb[2..10].copy_from_slice(&lba.to_be_bytes());
            cdb[10..14].copy_from_slice(&count.to_be_bytes());
            (cdb, 16)
        }
    }

    fn wait_ready(&mut self) -> io::Result<()> {
        // Spin-up and card detection take a few seconds at most.
        for _ in 0..20 {
            match self.command(&[0; 6], &mut Data::None) {
                Ok(_) => return Ok(()),
                Err(Failure::Sense {
                    key: NOT_READY | UNIT_ATTENTION,
                    ..
                }) => {
                    thread::sleep(Duration::from_millis(250));
                }
                Err(failure) => return Err(failure.into()),
            }
        }
        Err(io::Error::new(
            ErrorKind::TimedOut,
            "USB disk is not ready; is a medium inserted?",
        ))
    }

    fn read_capacity(&mut self) -> io::Result<()> {
        let mut cap = [0; 8];
        self.run(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], Data::In(&mut cap))?;
        let last = u32::from_be_bytes(cap[0..4].try_into().unwrap());
        let (last, block_bytes) = if last == u32::MAX {
            let mut cap = [0; 32];
            let mut cdb = [0; 16];
            cdb[0] = 0x9e;
            cdb[1] = 0x10;
            cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
            self.run(&cdb, Data::In(&mut cap))?;
            (
                u64::from_be_bytes(cap[0..8].try_into().unwrap()),
                u32::from_be_bytes(cap[8..12].try_into().unwrap()),
            )
        } else {
            (
                u64::from(last),
                u32::from_be_bytes(cap[4..8].try_into().unwrap()),
            )
        };
        if !matches!(block_bytes, 512 | 4096) {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!("USB disk has {block_bytes}-byte sectors; CarryFS needs 512 or 4096"),
            ));
        }
        self.blocks = last + 1;
        self.block_bytes = block_bytes;
        Ok(())
    }

    fn mode_sense_write_protect(&mut self) -> io::Result<bool> {
        let mut header = [0; 192];
        let got = self.run(&[0x1a, 0, 0x3f, 0, 192, 0], Data::In(&mut header))?;
        Ok(got >= 3 && header[2] & 0x80 != 0)
    }

    /// Run a command, retrying the unit attentions a device reports after
    /// reset or medium change.
    fn run(&mut self, cdb: &[u8], mut data: Data<'_>) -> Result<usize, Failure> {
        let mut attempts = 0;
        loop {
            match self.command(cdb, &mut data) {
                Err(Failure::Sense {
                    key: UNIT_ATTENTION,
                    ..
                }) if attempts < 3 => attempts += 1,
                result => return result,
            }
        }
    }

    /// One command and, if it failed, its sense data.
    fn command(&mut self, cdb: &[u8], data: &mut Data<'_>) -> Result<usize, Failure> {
        let (moved, status) = self.transfer(cdb, data)?;
        if status == 0 {
            return Ok(moved);
        }
        let mut sense = [0; 18];
        let (_, status) = self.transfer(&[0x03, 0, 0, 0, 18, 0], &mut Data::In(&mut sense))?;
        if status != 0 {
            return Err(io::Error::other("USB disk failed REQUEST SENSE").into());
        }
        Err(Failure::Sense {
            key: sense[2] & 0x0f,
            asc: sense[12],
            ascq: sense[13],
        })
    }

    /// The three BOT phases: command block, data, status. Returns the bytes
    /// moved and the CSW status.
    fn transfer(&mut self, cdb: &[u8], data: &mut Data<'_>) -> io::Result<(usize, u8)> {
        self.tag = self.tag.wrapping_add(1);
        let (len, flags) = match data {
            Data::None => (0, 0),
            Data::In(buffer) => (buffer.len(), 0x80),
            Data::Out(buffer) => (buffer.len(), 0),
        };
        let mut cbw = [0; 31];
        cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        cbw[4..8].copy_from_slice(&self.tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&(len as u32).to_le_bytes());
        cbw[12] = flags;
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);
        match self.usb.bulk_out(&cbw) {
            Ok(31) => {}
            Ok(_) => return Err(self.recover(io::Error::other("USB command block was short"))),
            Err(error) => return Err(self.recover(error)),
        }

        let (moved, endpoint) = match data {
            Data::None => (Ok(0), Endpoint::In),
            Data::In(buffer) => (self.usb.bulk_in(buffer), Endpoint::In),
            Data::Out(buffer) => (self.send_pieces(buffer), Endpoint::Out),
        };
        // A stall ends the data phase early; the status phase still follows.
        let moved = match moved {
            Ok(moved) => moved,
            Err(error) if error.kind() == ErrorKind::BrokenPipe => {
                self.usb.clear_halt(endpoint)?;
                0
            }
            Err(error) => return Err(self.recover(error)),
        };

        let mut csw = [0; 13];
        let got = match self.usb.bulk_in(&mut csw) {
            Err(error) if error.kind() == ErrorKind::BrokenPipe => {
                self.usb.clear_halt(Endpoint::In)?;
                self.usb.bulk_in(&mut csw)
            }
            result => result,
        };
        let got = got.map_err(|error| self.recover(error))?;
        let valid = got == 13
            && csw[0..4] == CSW_SIGNATURE.to_le_bytes()
            && csw[4..8] == self.tag.to_le_bytes();
        if !valid {
            return Err(self.recover(io::Error::other("USB disk sent an invalid status")));
        }
        let status = csw[12];
        if status >= 2 {
            return Err(self.recover(io::Error::other("USB disk reported a phase error")));
        }
        let residue = u32::from_le_bytes(csw[8..12].try_into().unwrap()) as usize;
        Ok((moved.min(len.saturating_sub(residue)), status))
    }

    /// One data-out phase sent as several bulk transfers. A short transfer ends it.
    fn send_pieces(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut moved = 0;
        for piece in data.chunks(OUT_PIECE) {
            let sent = self.usb.bulk_out(piece)?;
            moved += sent;
            if sent < piece.len() {
                break;
            }
        }
        Ok(moved)
    }

    /// Reset recovery after the device lost protocol sync. Returns `error`,
    /// the cause: a failed recovery shows up as the next command's error.
    fn recover(&mut self, error: io::Error) -> io::Error {
        let _ = self.usb.reset();
        let _ = self.usb.clear_halt(Endpoint::In);
        let _ = self.usb.clear_halt(Endpoint::Out);
        error
    }
}

/// A sparse in-memory USB disk that speaks BOT, for host tests.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    enum Phase {
        Command,
        DataIn(Vec<u8>, [u8; 13]),
        /// The next sector to write and the bytes still to come.
        DataOut(u64, usize, [u8; 13]),
        Stalled([u8; 13]),
        Status([u8; 13]),
    }

    pub struct Media {
        pub sectors: HashMap<u64, Vec<u8>>,
        pub flushes: usize,
        /// WRITE commands received.
        pub writes: usize,
        /// Reject SYNCHRONIZE CACHE as an unknown command, as some flash drives do.
        pub no_cache: bool,
        /// Commands left to fail with UNIT ATTENTION.
        pub attentions: usize,
    }

    pub struct FakeUsb {
        pub media: Arc<Mutex<Media>>,
        block_bytes: u32,
        blocks: u64,
        phase: Phase,
        sense: [u8; 18],
    }

    impl FakeUsb {
        pub fn new(block_bytes: u32, bytes: u64) -> Self {
            Self {
                media: Arc::new(Mutex::new(Media {
                    sectors: HashMap::new(),
                    flushes: 0,
                    writes: 0,
                    no_cache: false,
                    attentions: 1,
                })),
                block_bytes,
                blocks: bytes / u64::from(block_bytes),
                phase: Phase::Command,
                sense: [0; 18],
            }
        }

        fn csw(tag: &[u8], residue: usize, status: u8) -> [u8; 13] {
            let mut csw = [0; 13];
            csw[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
            csw[4..8].copy_from_slice(tag);
            csw[8..12].copy_from_slice(&(residue as u32).to_le_bytes());
            csw[12] = status;
            csw
        }

        fn command(&mut self, cbw: &[u8]) -> Phase {
            let tag = &cbw[4..8];
            let len = u32::from_le_bytes(cbw[8..12].try_into().unwrap()) as usize;
            let cdb = &cbw[15..15 + cbw[14] as usize];
            let bs = self.block_bytes as usize;
            let mut media = self.media.lock().unwrap();
            if media.attentions > 0 && !matches!(cdb[0], 0x03 | 0x12) {
                media.attentions -= 1;
                self.sense = [0; 18];
                self.sense[2] = UNIT_ATTENTION;
                self.sense[12] = 0x28;
                let csw = Self::csw(tag, len, 1);
                return if len == 0 {
                    Phase::Status(csw)
                } else {
                    Phase::Stalled(csw)
                };
            }
            let data_in = |data: Vec<u8>| {
                let residue = len.saturating_sub(data.len());
                let mut data = data;
                data.truncate(len);
                Phase::DataIn(data, Self::csw(tag, residue, 0))
            };
            let range = |cdb: &[u8]| -> (u64, usize) {
                if cdb.len() == 10 {
                    let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap());
                    let count = u16::from_be_bytes(cdb[7..9].try_into().unwrap());
                    (u64::from(lba), usize::from(count))
                } else {
                    let lba = u64::from_be_bytes(cdb[2..10].try_into().unwrap());
                    let count = u32::from_be_bytes(cdb[10..14].try_into().unwrap());
                    (lba, count as usize)
                }
            };
            match cdb[0] {
                0x00 => Phase::Status(Self::csw(tag, 0, 0)),
                0x03 => data_in(self.sense.to_vec()),
                0x12 => data_in(vec![0; 36]),
                0x1a => data_in(vec![3, 0, 0, 0]),
                0x25 => {
                    let mut cap = ((self.blocks - 1) as u32).to_be_bytes().to_vec();
                    cap.extend_from_slice(&self.block_bytes.to_be_bytes());
                    data_in(cap)
                }
                0x28 | 0x88 => {
                    let (lba, count) = range(cdb);
                    assert_eq!(count * bs, len);
                    let mut data = Vec::with_capacity(len);
                    for sector in lba..lba + count as u64 {
                        match media.sectors.get(&sector) {
                            Some(bytes) => data.extend_from_slice(bytes),
                            None => data.resize(data.len() + bs, 0),
                        }
                    }
                    data_in(data)
                }
                0x2a | 0x8a => {
                    media.writes += 1;
                    let (lba, count) = range(cdb);
                    assert_eq!(count * bs, len);
                    Phase::DataOut(lba, len, Self::csw(tag, 0, 0))
                }
                0x35 if media.no_cache => {
                    self.sense = [0; 18];
                    self.sense[2] = ILLEGAL_REQUEST;
                    self.sense[12] = 0x20;
                    Phase::Status(Self::csw(tag, 0, 1))
                }
                0x35 => {
                    media.flushes += 1;
                    Phase::Status(Self::csw(tag, 0, 0))
                }
                other => panic!("fake USB disk got opcode {other:#x}"),
            }
        }
    }

    fn stall() -> io::Error {
        io::Error::from(ErrorKind::BrokenPipe)
    }

    impl Transport for FakeUsb {
        fn bulk_out(&mut self, data: &[u8]) -> io::Result<usize> {
            match std::mem::replace(&mut self.phase, Phase::Command) {
                Phase::Command => {
                    assert_eq!(data[0..4], CBW_SIGNATURE.to_le_bytes());
                    self.phase = self.command(data);
                }
                Phase::DataOut(lba, left, csw) => {
                    let bs = self.block_bytes as usize;
                    let mut media = self.media.lock().unwrap();
                    for (i, sector) in data.chunks(bs).enumerate() {
                        media.sectors.insert(lba + i as u64, sector.to_vec());
                    }
                    let left = left - data.len();
                    self.phase = if left == 0 {
                        Phase::Status(csw)
                    } else {
                        Phase::DataOut(lba + (data.len() / bs) as u64, left, csw)
                    };
                }
                Phase::Stalled(csw) => {
                    self.phase = Phase::Status(csw);
                    return Err(stall());
                }
                _ => panic!("fake USB disk got an unexpected bulk OUT"),
            }
            Ok(data.len())
        }

        fn bulk_in(&mut self, out: &mut [u8]) -> io::Result<usize> {
            match std::mem::replace(&mut self.phase, Phase::Command) {
                Phase::DataIn(data, csw) => {
                    out[..data.len()].copy_from_slice(&data);
                    self.phase = Phase::Status(csw);
                    Ok(data.len())
                }
                Phase::Status(csw) => {
                    out[..13].copy_from_slice(&csw);
                    Ok(13)
                }
                Phase::Stalled(csw) => {
                    self.phase = Phase::Status(csw);
                    Err(stall())
                }
                _ => panic!("fake USB disk got an unexpected bulk IN"),
            }
        }

        fn clear_halt(&mut self, _endpoint: Endpoint) -> io::Result<()> {
            Ok(())
        }

        fn reset(&mut self) -> io::Result<()> {
            panic!("fake USB disk never loses sync");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeUsb;
    use super::*;

    #[test]
    fn reads_back_writes_across_command_boundaries() {
        let usb = FakeUsb::new(512, 64 << 20);
        let media = usb.media.clone();
        let mut disk = Disk::open(usb).unwrap();
        assert_eq!((disk.block_bytes(), disk.blocks()), (512, (64 << 20) / 512));
        assert!(!disk.write_protected());

        let data: Vec<u8> = (0..MAX_WRITE + 4096).map(|i| i as u8).collect();
        disk.write_at(1 << 20, &data).unwrap();
        disk.flush().unwrap();
        let mut back = vec![0; data.len()];
        disk.read_at(1 << 20, &mut back).unwrap();
        assert_eq!(back, data);
        assert_eq!(media.lock().unwrap().flushes, 1);
    }

    #[test]
    fn retries_unit_attention_after_a_stalled_data_phase() {
        let usb = FakeUsb::new(4096, 16 << 20);
        let media = usb.media.clone();
        let mut disk = Disk::open(usb).unwrap();
        media.lock().unwrap().attentions = 2;
        let mut page = vec![1; 4096];
        disk.read_at(0, &mut page).unwrap();
        assert_eq!(page, vec![0; 4096]);
    }

    #[test]
    fn a_drive_without_a_cache_may_reject_flush() {
        let usb = FakeUsb::new(512, 1 << 20);
        usb.media.lock().unwrap().no_cache = true;
        let mut disk = Disk::open(usb).unwrap();
        disk.flush().unwrap();
    }

    #[test]
    fn rejects_unaligned_and_out_of_range_transfers() {
        let mut disk = Disk::open(FakeUsb::new(512, 1 << 20)).unwrap();
        let mut sector = [0; 512];
        assert!(disk.read_at(100, &mut sector).is_err());
        assert!(disk.read_at(1 << 20, &mut sector).is_err());
        assert!(disk.write_at(0, &sector[..100]).is_err());
    }
}
