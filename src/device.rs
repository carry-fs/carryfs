use std::{
    collections::{HashMap, VecDeque},
    convert::TryFrom,
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::Path,
};

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "linux"
))]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

use crate::{Error, Result};

const BLOCK_BYTES: u64 = 4_096;
const MIN_IMAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024 * 1024 * 1024;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
const OPEN_NONBLOCK: i32 = 0x0004;
#[cfg(target_os = "linux")]
const OPEN_NONBLOCK: i32 = 0x0800;

/// Access requested for a device lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountMode {
    /// Open the image without write access.
    ReadOnly,
    /// Open the image with write access.
    ReadWrite,
}

/// Geometry of a CarryFS block device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    /// Length of the image in bytes.
    pub byte_len: u64,
    /// Smallest supported I/O alignment in bytes.
    pub logical_sector_bytes: u32,
    /// Physical sector size reported by the device in bytes.
    pub physical_sector_bytes: u32,
}

impl Geometry {
    /// Fails unless CarryFS supports a device of this size and sector sizes.
    /// Format helpers check it before they erase anything.
    pub fn validate(self) -> Result<()> {
        if !(MIN_IMAGE_BYTES..=MAX_IMAGE_BYTES).contains(&self.byte_len)
            || !self.byte_len.is_multiple_of(BLOCK_BYTES)
        {
            return Err(Error::InvalidGeometry);
        }

        match (self.logical_sector_bytes, self.physical_sector_bytes) {
            (512, 512 | 4096) | (4096, 4096) => Ok(()),
            _ => Err(Error::InvalidGeometry),
        }
    }

    pub(crate) fn blocks(self) -> u64 {
        self.byte_len / BLOCK_BYTES
    }

    pub(crate) fn block_offset(self, block: u64) -> Result<u64> {
        self.validate()?;
        if block >= self.blocks() {
            return Err(Error::InvalidInput("block is outside device geometry"));
        }
        block
            .checked_mul(BLOCK_BYTES)
            .ok_or(Error::InvalidInput("block offset overflow"))
    }
}

/// Synchronous positional block-device operations used by the storage core.
pub trait BlockDevice {
    /// Returns the immutable access mode held by this device lease.
    fn mode(&self) -> MountMode;

    /// Returns the device geometry.
    fn geometry(&mut self) -> Result<Geometry>;

    /// Reads the requested whole-sector range completely.
    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()>;

    /// Reads a whole-sector metadata range completely. A backend may use a
    /// separate read policy; the default forwards to [`BlockDevice::read_exact_at`].
    fn read_metadata_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.read_exact_at(offset, out)
    }

    /// Reads a whole-sector range of file data completely. The file's data
    /// is contiguous on the device from `offset` up to `run_end`, the end of
    /// its extent. `next_run` is where the file continues in another extent,
    /// given when the read is near that end. A backend that reads ahead can
    /// stop at one and start at the other. Both are hints: they come from
    /// extent records, replace no validation, and may be any value. The
    /// default forwards to [`BlockDevice::read_exact_at`].
    fn read_data_exact_at(
        &mut self,
        offset: u64,
        out: &mut [u8],
        _run_end: u64,
        _next_run: Option<u64>,
    ) -> Result<()> {
        self.read_exact_at(offset, out)
    }

    /// Hints that an exact whole-sector metadata range will be read soon.
    /// Hints never replace reads or validation. The default does nothing.
    fn prefetch_metadata(&mut self, _offset: u64, _len: usize) -> Result<()> {
        Ok(())
    }

    /// Writes the requested whole-sector range completely.
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;

    /// Writes one whole 4 KiB page at `offset`, handing over its buffer. A
    /// device that holds writes may keep the buffer instead of copying it.
    /// The default writes it through [`BlockDevice::write_all_at`].
    fn write_page(&mut self, offset: u64, page: Box<[u8; 4096]>) -> Result<()> {
        self.write_all_at(offset, &page[..])
    }

    /// Flushes earlier writes through the platform's durable-file barrier.
    fn flush_durable(&mut self) -> Result<()>;

    /// Reports that no root, durable or in memory, references the page at
    /// `offset`, and that the core will write it before reading it again. A
    /// device still holding a write to it may drop that write. The default
    /// does nothing.
    fn discard(&mut self, _offset: u64) {}
}

/// Regular-file image backend with an exclusive lease held until drop.
pub struct ImageDevice {
    file: File,
    geometry: Geometry,
    mode: MountMode,
    // Only a raw device opened for inspection reads through it.
    chunks: Option<Chunks>,
}

// A raw device costs a round trip per read, and the checker's metadata reads
// land in runs of adjacent pages. A read under one chunk fetches its whole
// aligned chunk, like the macOS adapter's read cache, and keeps 16 MiB of them
// for the reads that follow. A million-file check on a USB SSD took 139 s
// unwindowed, 54 s at 64 KiB, 34 s at 256 KiB and 33 s at 512 KiB. The reader
// holds the device exclusively and never writes, so a kept chunk cannot go
// stale.
const CHUNK_BYTES: usize = 256 << 10;
const CHUNK_CAPACITY: usize = 64;

struct Chunks {
    held: HashMap<u64, Box<[u8]>>,
    // The keys of `held`, oldest first.
    order: VecDeque<u64>,
}

impl Chunks {
    fn new() -> Self {
        Self {
            held: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl ImageDevice {
    /// Opens an existing regular image.
    ///
    /// The final path component may be a symlink; it is followed and the
    /// target file identity is locked. This lets hardlink and symlink aliases
    /// coordinate through the same operating-system lock. `create_new` never
    /// follows or replaces an existing path.
    pub fn open(path: impl AsRef<Path>, mode: MountMode) -> Result<Self> {
        let path = path.as_ref();
        let metadata = fs::metadata(path).map_err(|source| Error::io("inspect image", source))?;
        if !metadata.file_type().is_file() {
            return Err(Error::InvalidInput("image is not a regular file"));
        }

        let mut options = OpenOptions::new();
        options.read(true);
        if mode == MountMode::ReadWrite {
            options.write(true);
        }
        configure_image_open(&mut options);
        let file = options
            .open(path)
            .map_err(|source| Error::io("open image", source))?;
        Self::from_image_file(file, mode)
    }

    /// Opens a block or character device read-only, for inspection of a real
    /// disk without copying it to an image first. On Windows the device is a
    /// disk or volume path such as `\\.\Harddisk1Partition2`.
    ///
    /// The platform does not report a raw device's length through the file
    /// system on macOS, so the caller supplies it (for example `diskutil`'s
    /// `TotalSize`); `Volume::open` then rejects roots that disagree with it.
    /// No lease is taken: a lock on a device node would not stop the kernel
    /// or another filesystem from writing it, so the caller must make sure
    /// nothing has the device mounted. Unix and Windows only.
    pub fn open_device(path: impl AsRef<Path>, byte_len: u64) -> Result<Self> {
        let path = path.as_ref();
        // Refuse before opening: opening a FIFO blocks.
        #[cfg(unix)]
        {
            let metadata =
                fs::metadata(path).map_err(|source| Error::io("inspect device", source))?;
            if !is_device(&metadata) {
                return Err(Error::InvalidInput(
                    "path is not a block or character device",
                ));
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|source| Error::io("open device", source))?;
        Self::from_device_file(file, byte_len)
    }

    /// Wraps a device already opened elsewhere, such as a descriptor that
    /// macOS `authopen` returned or a Windows handle opened with no sharing,
    /// with the same rules as [`ImageDevice::open_device`]. Only reads are
    /// issued through it.
    pub fn from_device_file(file: File, byte_len: u64) -> Result<Self> {
        if !is_device_file(&file)? {
            return Err(Error::InvalidInput(
                "path is not a block or character device",
            ));
        }
        let geometry = Geometry {
            byte_len,
            logical_sector_bytes: 4096,
            physical_sector_bytes: 4096,
        };
        geometry.validate()?;
        Ok(Self {
            file,
            geometry,
            mode: MountMode::ReadOnly,
            chunks: Some(Chunks::new()),
        })
    }

    /// Creates a new sparse regular image and refuses an existing path.
    /// Creation synchronizes the parent directory: `fsync` on Unix,
    /// `FlushFileBuffers` on a directory handle on Windows. Other hosts return
    /// an unsupported-operation error before creating the image.
    pub fn create_new(path: impl AsRef<Path>, bytes: u64) -> Result<Self> {
        let path = path.as_ref();
        let geometry = Geometry {
            byte_len: bytes,
            logical_sector_bytes: 4096,
            physical_sector_bytes: 4096,
        };
        geometry.validate()?;
        let parent = open_parent_directory(path)?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| Error::io("create image", source))?;
        lock_file(&file)?;
        file.set_len(bytes)
            .map_err(|source| Error::io("resize image", source))?;
        file.sync_all()
            .map_err(|source| Error::io("flush new image", source))?;
        sync_parent_directory(&parent)?;
        Ok(Self {
            file,
            geometry,
            mode: MountMode::ReadWrite,
            chunks: None,
        })
    }

    /// Wraps an already opened regular image and takes the same exclusive
    /// image lock as [`ImageDevice::open`]. The descriptor must allow reads,
    /// and writes when `mode` is [`MountMode::ReadWrite`]. Any additional
    /// native lease on the descriptor remains held until the device is dropped.
    pub fn from_image_file(file: File, mode: MountMode) -> Result<Self> {
        let metadata = file
            .metadata()
            .map_err(|source| Error::io("inspect image", source))?;
        if !metadata.file_type().is_file() {
            return Err(Error::InvalidInput("image is not a regular file"));
        }
        lock_file(&file)?;

        let geometry = Geometry {
            byte_len: file
                .metadata()
                .map_err(|source| Error::io("inspect image", source))?
                .len(),
            logical_sector_bytes: 4096,
            physical_sector_bytes: 4096,
        };
        geometry.validate()?;
        Ok(Self {
            file,
            geometry,
            mode,
            chunks: None,
        })
    }

    fn read_raw(&self, offset: u64, out: &mut [u8]) -> Result<()> {
        let mut done = 0_usize;
        while done < out.len() {
            let done_u64 =
                u64::try_from(done).map_err(|_| Error::InvalidInput("I/O progress overflow"))?;
            let position = offset
                .checked_add(done_u64)
                .ok_or(Error::InvalidInput("I/O position overflow"))?;
            match read_at(&self.file, &mut out[done..], position) {
                Ok(0) => {
                    return Err(Error::io(
                        "read_exact_at",
                        io::Error::new(io::ErrorKind::UnexpectedEof, "zero read progress"),
                    ));
                }
                Ok(read) => done += read,
                Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(Error::io("read_exact_at", source)),
            }
        }
        Ok(())
    }

    // Serves `out` from aligned chunks, reading a missing one whole.
    fn read_chunked(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        let byte_len = self.geometry.byte_len;
        let mut done = 0_usize;
        while done < out.len() {
            let position = offset + done as u64;
            let index = position / CHUNK_BYTES as u64;
            let inside = (position % CHUNK_BYTES as u64) as usize;
            if !self
                .chunks
                .as_ref()
                .is_some_and(|c| c.held.contains_key(&index))
            {
                let start = index * CHUNK_BYTES as u64;
                let len = usize::try_from((byte_len - start).min(CHUNK_BYTES as u64))
                    .map_err(|_| Error::InvalidInput("chunk length overflow"))?;
                let mut chunk = vec![0; len].into_boxed_slice();
                self.read_raw(start, &mut chunk)?;
                if let Some(chunks) = self.chunks.as_mut() {
                    if chunks.order.len() >= CHUNK_CAPACITY
                        && let Some(oldest) = chunks.order.pop_front()
                    {
                        chunks.held.remove(&oldest);
                    }
                    chunks.held.insert(index, chunk);
                    chunks.order.push_back(index);
                }
            }
            let chunk = self
                .chunks
                .as_ref()
                .and_then(|c| c.held.get(&index))
                .ok_or(Error::InvalidInput("chunk vanished"))?;
            let n = (out.len() - done).min(chunk.len() - inside);
            out[done..done + n].copy_from_slice(&chunk[inside..inside + n]);
            done += n;
        }
        Ok(())
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<u64> {
        let len = u64::try_from(len).map_err(|_| Error::InvalidInput("I/O length overflow"))?;
        let sector = u64::from(self.geometry.logical_sector_bytes);
        if !offset.is_multiple_of(sector) {
            return Err(Error::InvalidInput("I/O offset is not sector aligned"));
        }
        if !len.is_multiple_of(sector) {
            return Err(Error::InvalidInput("I/O length is not sector aligned"));
        }
        let end = offset
            .checked_add(len)
            .ok_or(Error::InvalidInput("I/O range overflow"))?;
        if offset > self.geometry.byte_len || end > self.geometry.byte_len {
            return Err(Error::InvalidInput("I/O range is outside image"));
        }
        Ok(len)
    }
}

impl BlockDevice for ImageDevice {
    fn mode(&self) -> MountMode {
        self.mode
    }

    fn geometry(&mut self) -> Result<Geometry> {
        Ok(self.geometry)
    }

    fn read_exact_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.check_range(offset, out.len())?;
        if self.chunks.is_some() && out.len() < CHUNK_BYTES {
            return self.read_chunked(offset, out);
        }
        self.read_raw(offset, out)
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.mode == MountMode::ReadOnly {
            return Err(Error::ReadOnly);
        }
        self.check_range(offset, data.len())?;
        let mut done = 0_usize;
        while done < data.len() {
            let done_u64 =
                u64::try_from(done).map_err(|_| Error::InvalidInput("I/O progress overflow"))?;
            let position = offset
                .checked_add(done_u64)
                .ok_or(Error::InvalidInput("I/O position overflow"))?;
            match write_at(&self.file, &data[done..], position) {
                Ok(0) => {
                    return Err(Error::io(
                        "write_all_at",
                        io::Error::new(io::ErrorKind::WriteZero, "zero write progress"),
                    ));
                }
                Ok(written) => done += written,
                Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(Error::io("write_all_at", source)),
            }
        }
        Ok(())
    }

    fn flush_durable(&mut self) -> Result<()> {
        // On Apple targets Rust maps this call to F_FULLFSYNC and retries only
        // EINTR; unsupported or failed barriers are returned to the caller.
        self.file
            .sync_all()
            .map_err(|source| Error::io("flush_durable", source))
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "linux"
))]
fn configure_image_open(options: &mut OpenOptions) {
    options.custom_flags(OPEN_NONBLOCK);
}

#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        target_os = "linux"
    ))
))]
fn configure_image_open(_options: &mut OpenOptions) {}

#[cfg(not(unix))]
fn configure_image_open(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn is_device(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    metadata.file_type().is_block_device() || metadata.file_type().is_char_device()
}

#[cfg(unix)]
fn is_device_file(file: &File) -> Result<bool> {
    let metadata = file
        .metadata()
        .map_err(|source| Error::io("inspect device", source))?;
    Ok(is_device(&metadata))
}

/// Windows reports no file metadata for a raw volume handle, so ask the disk
/// stack instead: a disk or volume answers its length query, and a regular
/// file or directory does not.
#[cfg(windows)]
fn is_device_file(file: &File) -> Result<bool> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    // CTL_CODE(IOCTL_DISK_BASE, 0x17, METHOD_BUFFERED, FILE_READ_ACCESS).
    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405c;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }
    let mut length = 0_i64;
    let mut returned = 0_u32;
    // SAFETY: the handle stays open for the call, the output is a live i64 of
    // the size passed, and the handle is synchronous, so no OVERLAPPED.
    let answered = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            IOCTL_DISK_GET_LENGTH_INFO,
            std::ptr::null(),
            0,
            (&raw mut length).cast(),
            8,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    Ok(answered != 0)
}

#[cfg(not(any(unix, windows)))]
fn is_device_file(_file: &File) -> Result<bool> {
    Ok(false)
}

#[cfg(unix)]
fn read_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(out, offset)
}

#[cfg(windows)]
fn read_at(file: &File, out: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(out, offset)
}

#[cfg(unix)]
fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<usize> {
    file.write_at(data, offset)
}

#[cfg(windows)]
fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<usize> {
    file.seek_write(data, offset)
}

#[cfg(unix)]
fn open_parent_directory(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut options = OpenOptions::new();
    options.read(true);
    configure_image_open(&mut options);
    options
        .open(parent)
        .map_err(|source| Error::io("open image parent directory", source))
}

/// Windows opens a directory only with backup semantics, and
/// `FlushFileBuffers`, which `sync_all` calls, needs write access.
#[cfg(windows)]
fn open_parent_directory(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)
        .map_err(|source| Error::io("open image parent directory", source))
}

fn sync_parent_directory(directory: &File) -> Result<()> {
    directory
        .sync_all()
        .map_err(|source| Error::io("flush image parent directory", source))
}

#[cfg(not(any(unix, windows)))]
fn open_parent_directory(_path: &Path) -> Result<File> {
    Err(Error::io(
        "open image parent directory",
        io::Error::new(
            io::ErrorKind::Unsupported,
            "durable directory synchronization is unavailable on this platform",
        ),
    ))
}

fn lock_file(file: &File) -> Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Error::Busy),
        Err(TryLockError::Error(source)) => Err(Error::io("lock image", source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Write,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    const TEST_BYTES: u64 = MIN_IMAGE_BYTES;

    struct TempPath {
        path: PathBuf,
    }

    impl TempPath {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let counter = NEXT.fetch_add(1, Ordering::Relaxed);
            Self {
                path: std::env::temp_dir().join(format!(
                    "carryfs-device-{label}-{}-{stamp}-{counter}",
                    std::process::id()
                )),
            }
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let Ok(metadata) = fs::symlink_metadata(&self.path) else {
                return;
            };
            if metadata.file_type().is_dir() {
                let _ = fs::remove_dir(&self.path);
            } else {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    #[test]
    fn chunked_reads_match_direct_reads_across_chunk_edges_and_eviction() {
        // Not a multiple of the chunk size, so the last chunk is short.
        let bytes = MIN_IMAGE_BYTES + 3 * BLOCK_BYTES;
        let temp = TempPath::new("chunked");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp.path)
            .expect("create image");
        let pattern: Vec<u8> = (0..bytes)
            .map(|i| (i.wrapping_mul(31) >> 3) as u8)
            .collect();
        file.write_all(&pattern).expect("fill image");
        let geometry = Geometry {
            byte_len: bytes,
            logical_sector_bytes: 4096,
            physical_sector_bytes: 4096,
        };
        let mut device = ImageDevice {
            file,
            geometry,
            mode: MountMode::ReadOnly,
            chunks: Some(Chunks::new()),
        };
        let edge = CHUNK_BYTES as u64;
        let mut offsets = vec![0, 4096, edge - 4096, edge, 3 * edge + 8192, bytes - 4096];
        // Enough distinct chunks to evict the oldest, then revisit the first.
        offsets.extend((0..CHUNK_CAPACITY as u64 + 4).map(|n| n * edge));
        offsets.extend([0, 4096]);
        for offset in offsets {
            for len in [4096_usize, 8192, 65536, 2 * CHUNK_BYTES] {
                let len = len.min((bytes - offset) as usize);
                let mut got = vec![0; len];
                device
                    .read_exact_at(offset, &mut got)
                    .expect("chunked read");
                assert_eq!(got, pattern[offset as usize..offset as usize + len]);
            }
        }
        let held = device.chunks.as_ref().map_or(0, |c| c.held.len());
        assert!(held <= CHUNK_CAPACITY);
        assert!(device.read_exact_at(bytes - 4096, &mut [0; 8192]).is_err());
    }

    #[test]
    fn geometry_accepts_limits_and_rejects_invalid_values() {
        let valid = Geometry {
            byte_len: TEST_BYTES,
            logical_sector_bytes: 512,
            physical_sector_bytes: 4096,
        };
        assert!(valid.validate().is_ok());
        assert!(
            Geometry {
                byte_len: TEST_BYTES - BLOCK_BYTES,
                ..valid
            }
            .validate()
            .is_err()
        );
        assert!(
            Geometry {
                byte_len: MAX_IMAGE_BYTES + BLOCK_BYTES,
                ..valid
            }
            .validate()
            .is_err()
        );
        assert!(
            Geometry {
                logical_sector_bytes: 4096,
                physical_sector_bytes: 512,
                ..valid
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn geometry_block_offsets_are_checked() {
        let geometry = Geometry {
            byte_len: TEST_BYTES,
            logical_sector_bytes: 4096,
            physical_sector_bytes: 4096,
        };
        assert_eq!(geometry.blocks(), 65_536);
        assert_eq!(geometry.block_offset(0).unwrap(), 0);
        assert_eq!(
            geometry.block_offset(65_535).unwrap(),
            TEST_BYTES - BLOCK_BYTES
        );
        assert!(geometry.block_offset(65_536).is_err());
    }

    #[test]
    fn open_rejects_nonregular_paths() {
        let path = TempPath::new("directory");
        fs::create_dir(&path.path).unwrap();
        assert!(matches!(
            ImageDevice::open(&path.path, MountMode::ReadOnly),
            Err(Error::InvalidInput("image is not a regular file"))
        ));
    }

    #[test]
    fn open_device_rejects_regular_images() {
        let path = TempPath::new("not-device");
        drop(ImageDevice::create_new(&path.path, TEST_BYTES).unwrap());
        assert!(matches!(
            ImageDevice::open_device(&path.path, TEST_BYTES),
            Err(Error::InvalidInput(
                "path is not a block or character device"
            ))
        ));
        assert!(matches!(
            ImageDevice::from_device_file(File::open(&path.path).unwrap(), TEST_BYTES),
            Err(Error::InvalidInput(
                "path is not a block or character device"
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn open_device_reads_a_character_device_read_only() {
        // /dev/zero is a character device on every Unix this builds for.
        let mut device = ImageDevice::open_device("/dev/zero", TEST_BYTES).unwrap();
        assert_eq!(device.mode(), MountMode::ReadOnly);
        let mut page = [1u8; 4096];
        device.read_exact_at(TEST_BYTES - 4096, &mut page).unwrap();
        assert_eq!(page, [0u8; 4096]);
        assert!(device.write_all_at(0, &page).is_err());
        assert!(device.read_exact_at(TEST_BYTES, &mut page).is_err());
    }

    #[test]
    fn create_new_refuses_to_overwrite() {
        let path = TempPath::new("create");
        let device = ImageDevice::create_new(&path.path, TEST_BYTES).unwrap();
        drop(device);
        assert!(ImageDevice::create_new(&path.path, TEST_BYTES).is_err());
    }

    #[test]
    fn create_new_rejects_invalid_lengths_before_creation() {
        for (index, bytes) in [
            TEST_BYTES - BLOCK_BYTES,
            TEST_BYTES + 1,
            MAX_IMAGE_BYTES + 1,
            u64::MAX,
        ]
        .into_iter()
        .enumerate()
        {
            let path = TempPath::new(&format!("invalid-{index}"));
            assert!(matches!(
                ImageDevice::create_new(&path.path, bytes),
                Err(Error::InvalidGeometry)
            ));
            assert!(!path.path.exists());
        }
    }

    #[test]
    fn short_backing_file_reports_unexpected_eof() {
        let path = TempPath::new("short-read");
        let mut device = ImageDevice::create_new(&path.path, TEST_BYTES).unwrap();
        device.file.set_len(TEST_BYTES - BLOCK_BYTES).unwrap();
        let mut block = vec![0_u8; BLOCK_BYTES as usize];

        assert!(matches!(
            device.read_exact_at(TEST_BYTES - BLOCK_BYTES, &mut block),
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[cfg(unix)]
    #[test]
    fn aliases_cannot_acquire_a_second_readonly_lease() {
        let path = TempPath::new("aliases");
        let hardlink = TempPath::new("hardlink");
        let symlink = TempPath::new("symlink");
        let device = ImageDevice::create_new(&path.path, TEST_BYTES).unwrap();
        fs::hard_link(&path.path, &hardlink.path).unwrap();
        std::os::unix::fs::symlink(&path.path, &symlink.path).unwrap();

        assert!(matches!(
            ImageDevice::open(&hardlink.path, MountMode::ReadOnly),
            Err(Error::Busy)
        ));
        assert!(matches!(
            ImageDevice::open(&symlink.path, MountMode::ReadOnly),
            Err(Error::Busy)
        ));
        drop(device);

        let readonly = ImageDevice::open(&symlink.path, MountMode::ReadOnly).unwrap();
        assert_eq!(readonly.mode(), MountMode::ReadOnly);
        assert!(matches!(
            ImageDevice::open(&hardlink.path, MountMode::ReadOnly),
            Err(Error::Busy)
        ));
    }

    #[test]
    fn positional_io_survives_reopen_and_checks_ranges() {
        let path = TempPath::new("io");
        let mut device = ImageDevice::create_new(&path.path, TEST_BYTES).unwrap();
        let payload = vec![0x5a; 4096];
        device.write_all_at(4096, &payload).unwrap();
        device.flush_durable().unwrap();
        assert!(device.read_exact_at(1, &mut []).is_err());
        assert!(device.read_exact_at(TEST_BYTES + 4096, &mut []).is_err());
        let mut overflow = vec![0_u8; BLOCK_BYTES as usize];
        assert!(matches!(
            device.read_exact_at(u64::MAX - (BLOCK_BYTES - 1), &mut overflow),
            Err(Error::InvalidInput("I/O range overflow"))
        ));
        drop(device);

        let mut reopened = ImageDevice::open(&path.path, MountMode::ReadOnly).unwrap();
        let mut storage = vec![0_u8; 4097];
        reopened.read_exact_at(4096, &mut storage[1..]).unwrap();
        assert_eq!(&storage[1..], payload.as_slice());
        assert!(matches!(
            reopened.write_all_at(4096, &payload),
            Err(Error::ReadOnly)
        ));
    }
}
