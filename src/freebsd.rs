//! FreeBSD GEOM device access shared by the mount and offline tools.

use std::ffi::CStr;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::{Error, Geometry, MountMode, Result};

// FreeBSD sys/sys/disk.h. These requests have the same layout on amd64 and arm64.
const DIOCGSECTORSIZE: libc::c_ulong = 0x4004_6480;
const DIOCGMEDIASIZE: libc::c_ulong = 0x4008_6481;
const DIOCGFLUSH: libc::c_ulong = 0x2000_6487;
const DIOCGPROVIDERNAME: libc::c_ulong = 0x4400_648a;

/// Open and lock the canonical GEOM provider, so device aliases share a lease.
/// Writable opens also participate in GEOM's exclusion of native mounts.
pub fn open_device(path: &Path, mode: MountMode) -> Result<File> {
    let open = |path: &Path| {
        OpenOptions::new()
            .read(true)
            .write(mode == MountMode::ReadWrite)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|source| Error::io("open GEOM device", source))
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| Error::io("open GEOM device", e))?;
    if !file
        .metadata()
        .map_err(|e| Error::io("stat GEOM device", e))?
        .file_type()
        .is_char_device()
    {
        return Err(Error::InvalidInput("device is not a GEOM character device"));
    }
    let mut name = [0u8; 1024];
    // SAFETY: the ioctl writes at most MAXPATHLEN bytes into this live buffer.
    if unsafe { libc::ioctl(file.as_raw_fd(), DIOCGPROVIDERNAME, name.as_mut_ptr()) } != 0 {
        return Err(Error::io(
            "read GEOM provider name",
            io::Error::last_os_error(),
        ));
    }
    let name = CStr::from_bytes_until_nul(&name)
        .map_err(|_| Error::InvalidInput("unterminated GEOM provider name"))?;
    let canonical = Path::new("/dev").join(std::ffi::OsStr::from_bytes(name.to_bytes()));
    // Inspect before requesting write access, which can spoil GEOM consumers.
    reject_mounted(&file)?;
    let file = open(&canonical)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(Error::Busy),
        Err(TryLockError::Error(source)) => return Err(Error::io("lock GEOM device", source)),
    }
    reject_mounted(&file)?;
    geometry(&file)?.validate()?;
    Ok(file)
}

fn reject_mounted(file: &File) -> Result<()> {
    let device = file
        .metadata()
        .map_err(|e| Error::io("stat GEOM device", e))?
        .rdev();
    // Read-only native mounts need this check too: GEOM excludes only writers.
    let mut capacity = 16usize;
    loop {
        let mut mounts = Vec::<libc::statfs>::with_capacity(capacity);
        // SAFETY: the allocation holds capacity statfs values; getfsstat initializes
        // only whole values and returns at most the capacity passed in bytes.
        let count = unsafe {
            libc::getfsstat(
                mounts.as_mut_ptr(),
                (capacity * std::mem::size_of::<libc::statfs>()) as libc::c_long,
                libc::MNT_NOWAIT,
            )
        };
        if count < 0 {
            return Err(Error::io("read mount table", io::Error::last_os_error()));
        }
        if count as usize >= capacity {
            capacity *= 2;
            continue;
        }
        // SAFETY: getfsstat initialized exactly count records in this allocation.
        unsafe { mounts.set_len(count as usize) };
        for mount in mounts {
            // SAFETY: the kernel returns a NUL-terminated mount source in each record.
            let source = unsafe { CStr::from_ptr(mount.f_mntfromname.as_ptr()) };
            let path = Path::new(std::ffi::OsStr::from_bytes(source.to_bytes()));
            if let Ok(metadata) = std::fs::metadata(path)
                && metadata.file_type().is_char_device()
                && metadata.rdev() == device
            {
                return Err(Error::Busy);
            }
        }
        return Ok(());
    }
}

/// Read the GEOM provider's media length and sector size.
pub fn geometry(file: &File) -> Result<Geometry> {
    let mut sector: u32 = 0;
    let mut bytes: libc::off_t = 0;
    // SAFETY: each ioctl writes one value of the declared size into a live local.
    let ok = unsafe {
        libc::ioctl(file.as_raw_fd(), DIOCGSECTORSIZE, &mut sector) == 0
            && libc::ioctl(file.as_raw_fd(), DIOCGMEDIASIZE, &mut bytes) == 0
    };
    if !ok {
        return Err(Error::io("read GEOM geometry", io::Error::last_os_error()));
    }
    Ok(Geometry {
        byte_len: u64::try_from(bytes).map_err(|_| Error::InvalidGeometry)?,
        logical_sector_bytes: sector,
        // GEOM exposes no separate physical size; all CarryFS writes are 4 KiB aligned.
        physical_sector_bytes: sector,
    })
}

/// Flush earlier raw-device writes through GEOM to the drive's write cache.
pub fn flush_device(file: &File) -> io::Result<()> {
    // SAFETY: DIOCGFLUSH takes no argument on an open GEOM descriptor.
    if unsafe { libc::ioctl(file.as_raw_fd(), DIOCGFLUSH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
