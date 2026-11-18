//! NetBSD raw disk access shared by the mount and offline tools.

#[cfg(not(target_arch = "x86_64"))]
compile_error!("NetBSD support currently requires amd64");

use std::ffi::{CStr, OsStr};
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::format::{BLOCK_BYTES, ROOT_MAGIC};
use crate::{Error, Geometry, MountMode, Result};

// sys/dkio.h, on the supported amd64 releases.
const DIOCGSECTORSIZE: libc::c_ulong = 0x4004_6485;
const DIOCGMEDIASIZE: libc::c_ulong = 0x4008_6484;
const DIOCCACHESYNC: libc::c_ulong = 0x8004_6476;

// Keep leases until process exit, including when a session ends after an error.
static HELD: Mutex<Vec<(String, File)>> = Mutex::new(Vec::new());

// libc 0.2 still describes the pre-NetBSD-10 statvfs and compatibility symbol.
// This is sys/statvfs.h's current ABI on both supported releases. The native
// header probe and the test below check its size and the field we read.
#[repr(C)]
struct StatVfs {
    _flag: libc::c_ulong,
    _bsize: libc::c_ulong,
    _frsize: libc::c_ulong,
    _iosize: libc::c_ulong,
    _blocks: u64,
    _bfree: u64,
    _bavail: u64,
    _bresvd: u64,
    _files: u64,
    _ffree: u64,
    _favail: u64,
    _fresvd: u64,
    _syncreads: u64,
    _syncwrites: u64,
    _asyncreads: u64,
    _asyncwrites: u64,
    _fsidx: libc::fsid_t,
    _fsid: libc::c_ulong,
    _namemax: libc::c_ulong,
    _owner: libc::uid_t,
    _spare: [u64; 4],
    _fstypename: [libc::c_char; 32],
    _mntonname: [libc::c_char; 1024],
    source: [libc::c_char; 1024],
    _mntfromlabel: [libc::c_char; 1024],
}

unsafe extern "C" {
    #[link_name = "__getvfsstat90"]
    fn getvfsstat(buf: *mut StatVfs, bytes: usize, flags: libc::c_int) -> libc::c_int;
}

/// Resolve block nodes and aliases to the corresponding canonical raw node.
pub fn raw_node(path: &Path) -> Result<PathBuf> {
    let metadata = fs::metadata(path).map_err(|e| Error::io("stat NetBSD device", e))?;
    let kind = metadata.file_type();
    let mode = if kind.is_char_device() {
        libc::S_IFCHR
    } else if kind.is_block_device() {
        libc::S_IFBLK
    } else {
        return Err(Error::InvalidInput("expected a NetBSD disk device"));
    };
    // SAFETY: devname returns a NUL-terminated name for a device number. Copy
    // it before another call can replace libc's buffer.
    let name = unsafe {
        let name = libc::devname(metadata.rdev() as libc::dev_t, mode);
        if name.is_null() {
            return Err(Error::InvalidInput("unknown NetBSD disk device"));
        }
        CStr::from_ptr(name).to_bytes().to_vec()
    };
    let name = OsStr::from_bytes(&name);
    Ok(if kind.is_char_device() {
        Path::new("/dev").join(name)
    } else {
        let mut raw = std::ffi::OsString::from("r");
        raw.push(name);
        Path::new("/dev").join(raw)
    })
}

/// Open a raw device, excluding other CarryFS tools and existing native mounts.
pub fn open_device(path: &Path, mode: MountMode) -> Result<File> {
    let path = raw_node(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(mode == MountMode::ReadWrite)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .map_err(|e| Error::io("open NetBSD raw device", e))?;
    geometry(&file)?.validate()?;
    lock(format!(
        "device-{:x}",
        file.metadata()
            .map_err(|e| Error::io("stat raw device", e))?
            .rdev()
    ))?;
    reject_mounted(&file)?;
    // A UUID lease also catches two raw nodes exposing the same volume.
    let blocks = geometry(&file)?.blocks();
    let mut page = vec![0; BLOCK_BYTES];
    for block in [256, blocks.saturating_sub(257)] {
        file.read_exact_at(&mut page, block * BLOCK_BYTES as u64)
            .map_err(|e| Error::io("read volume identity", e))?;
        if page.starts_with(ROOT_MAGIC) {
            let uuid: String = page[24..40].iter().map(|b| format!("{b:02x}")).collect();
            lock(format!("volume-{uuid}"))?;
            break;
        }
    }
    Ok(file)
}

fn lock(name: String) -> Result<()> {
    let mut held = HELD
        .lock()
        .map_err(|_| Error::InvalidInput("device lock table poisoned"))?;
    if held.iter().any(|(key, _)| *key == name) {
        return Ok(());
    }
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create("/var/run/carryfs")
        .map_err(|e| Error::io("create device lock directory", e))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(Path::new("/var/run/carryfs").join(format!("{name}.lock")))
        .map_err(|e| Error::io("open device lock", e))?;
    match file.try_lock() {
        Ok(()) => {
            held.push((name, file));
            Ok(())
        }
        Err(TryLockError::WouldBlock) => Err(Error::Busy),
        Err(TryLockError::Error(e)) => Err(Error::io("lock NetBSD device", e)),
    }
}

fn reject_mounted(file: &File) -> Result<()> {
    let device = file
        .metadata()
        .map_err(|e| Error::io("stat raw device", e))?
        .rdev();
    let mut capacity = 16;
    loop {
        let mut mounts = Vec::<StatVfs>::with_capacity(capacity);
        // SAFETY: the allocation has room for capacity complete statvfs values.
        let count = unsafe {
            getvfsstat(
                mounts.as_mut_ptr(),
                capacity * size_of::<StatVfs>(),
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
        // SAFETY: getvfsstat initialized exactly count records.
        unsafe { mounts.set_len(count as usize) };
        for mount in mounts {
            // SAFETY: the kernel supplies a NUL-terminated mount source.
            let source = unsafe { CStr::from_ptr(mount.source.as_ptr()) };
            if !source.to_bytes().starts_with(b"/dev/") {
                continue;
            }
            let source = Path::new(OsStr::from_bytes(source.to_bytes()));
            if let Ok(raw) = raw_node(source)
                && fs::metadata(raw).is_ok_and(|m| m.rdev() == device)
            {
                return Err(Error::Busy);
            }
        }
        return Ok(());
    }
}

/// Read the raw disk or partition's media length and logical sector size.
pub fn geometry(file: &File) -> Result<Geometry> {
    let mut sector: u32 = 0;
    let mut bytes: libc::off_t = 0;
    // SAFETY: both ioctls write one value into a live local of the ABI's size.
    if unsafe {
        libc::ioctl(file.as_raw_fd(), DIOCGSECTORSIZE, &mut sector) != 0
            || libc::ioctl(file.as_raw_fd(), DIOCGMEDIASIZE, &mut bytes) != 0
    } {
        return Err(Error::io(
            "read NetBSD disk geometry",
            io::Error::last_os_error(),
        ));
    }
    Ok(Geometry {
        byte_len: bytes.try_into().map_err(|_| Error::InvalidGeometry)?,
        logical_sector_bytes: sector,
        physical_sector_bytes: sector,
    })
}

/// Ask the disk driver to flush its cache; unsupported barriers fail closed.
pub fn flush_device(file: &File) -> io::Result<()> {
    let force: libc::c_int = 1;
    // SAFETY: the ioctl reads one int from a live local on an open device.
    if unsafe { libc::ioctl(file.as_raw_fd(), DIOCCACHESYNC, &force) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn statvfs_matches_the_native_header_probe() {
        assert_eq!(size_of::<StatVfs>(), 3296);
        assert_eq!(align_of::<StatVfs>(), 8);
        assert_eq!(std::mem::offset_of!(StatVfs, source), 1248);
    }
}
