//! A `BlockDevice` over a raw disk device for the macOS helpers.
//!
//! `carryfs.util`, `newfs_carryfs`, and `fsck_carryfs` need it because
//! DiskArbitration and storagekitd hand them a device path, and `ImageDevice`
//! accepts only regular files.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use carryfs_core::{BlockDevice, Error, Geometry, MountMode, Result};

// From the macOS SDK's <sys/disk.h>.
const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
const DKIOCGETPHYSICALBLOCKSIZE: libc::c_ulong = 0x4004_644d;
const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416;

/// One raw disk device.
pub struct RawDevice {
    file: File,
    geometry: Geometry,
    mode: MountMode,
}

fn failed(operation: &'static str) -> Error {
    Error::Io {
        operation,
        source: io::Error::last_os_error(),
    }
}

impl RawDevice {
    /// Open `/dev/rdiskN`.
    ///
    /// The raw node is unbuffered, which is what a probe and a check want:
    /// they must see the medium, not a stale page. Only `newfs_carryfs` opens
    /// it writable, and the kernel refuses that while a volume is mounted.
    pub fn open(path: impl AsRef<Path>, mode: MountMode) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(mode == MountMode::ReadWrite)
            .open(path.as_ref())
            .map_err(|source| Error::Io {
                operation: "open device",
                source,
            })?;
        let fd = file.as_raw_fd();
        let mut logical: u32 = 0;
        let mut physical: u32 = 0;
        let mut blocks: u64 = 0;
        // SAFETY: each call writes one integer of the size its ioctl declares,
        // into a local of exactly that type, on a file descriptor this function
        // owns.
        let ok = unsafe {
            libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut logical) == 0
                && libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut blocks) == 0
                && libc::ioctl(fd, DKIOCGETPHYSICALBLOCKSIZE, &mut physical) == 0
        };
        if !ok {
            return Err(failed("device geometry"));
        }
        let byte_len = blocks
            .checked_mul(u64::from(logical))
            .ok_or(Error::InvalidGeometry)?;
        Ok(Self {
            file,
            geometry: Geometry {
                byte_len,
                logical_sector_bytes: logical,
                physical_sector_bytes: physical,
            },
            mode,
        })
    }
}

impl BlockDevice for RawDevice {
    fn mode(&self) -> MountMode {
        self.mode
    }

    fn geometry(&mut self) -> Result<Geometry> {
        Ok(self.geometry)
    }

    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < out.len() {
            let slice = &mut out[done..];
            // SAFETY: `slice` is a live mutable borrow of `out`, and the length
            // passed is its own.
            let moved = unsafe {
                libc::pread(
                    self.file.as_raw_fd(),
                    slice.as_mut_ptr().cast(),
                    slice.len(),
                    (offset + done as u64) as libc::off_t,
                )
            };
            match moved {
                -1 => return Err(failed("read_exact_at")),
                0 => {
                    return Err(Error::Io {
                        operation: "read_exact_at",
                        source: io::Error::from(io::ErrorKind::UnexpectedEof),
                    });
                }
                moved => done += moved as usize,
            }
        }
        Ok(())
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.mode == MountMode::ReadOnly {
            return Err(Error::ReadOnly);
        }
        let mut done = 0;
        while done < data.len() {
            let slice = &data[done..];
            // SAFETY: `slice` is a live borrow of `data`, and the length passed
            // is its own.
            let moved = unsafe {
                libc::pwrite(
                    self.file.as_raw_fd(),
                    slice.as_ptr().cast(),
                    slice.len(),
                    (offset + done as u64) as libc::off_t,
                )
            };
            match moved {
                -1 => return Err(failed("write_all_at")),
                0 => {
                    return Err(Error::Io {
                        operation: "write_all_at",
                        source: io::Error::from(io::ErrorKind::WriteZero),
                    });
                }
                moved => done += moved as usize,
            }
        }
        Ok(())
    }

    /// Asks the drive to write out its cache. The raw node has no buffer cache
    /// of its own, so this is the whole barrier.
    fn flush_durable(&mut self) -> Result<()> {
        if self.mode == MountMode::ReadOnly {
            return Ok(());
        }
        // SAFETY: DKIOCSYNCHRONIZECACHE takes no argument, on a descriptor this
        // device owns.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), DKIOCSYNCHRONIZECACHE) } != 0 {
            return Err(failed("flush_durable"));
        }
        Ok(())
    }
}
