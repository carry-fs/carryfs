//! Offline CLI transfer. Keep host paths out of the filesystem core.

use std::ffi::{OsStr, OsString};
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
use std::{fs, io::Read, os::unix::ffi::OsStrExt};

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
use carryfs_core::{AttributeChange, Mode, Name, NewAttributes, ObjectId, ObjectKind, ROOT};
use carryfs_core::{Finding, ImageDevice, MountMode, Volume};

#[cfg(target_os = "macos")]
mod authopen;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
mod host;
#[cfg(target_os = "linux")]
mod image_linux;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) fn report(finding: Finding<'_>) {
    let path = finding
        .path
        .iter()
        .map(|n| String::from_utf8_lossy(n.stored()))
        .collect::<Vec<_>>()
        .join("/");
    match finding.offset {
        Some(offset) => eprintln!(
            "carryfs: {path:?} {:?} bytes {offset}..{}: {}",
            finding.object,
            offset + finding.length,
            finding.error
        ),
        None => eprintln!("carryfs: {path:?} {:?}: {}", finding.object, finding.error),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn names(path: &OsStr) -> Result<Vec<Name>> {
    let path = path.to_str().ok_or("CarryFS path must be UTF-8")?;
    if !path.starts_with('/') {
        return Err("CarryFS paths must be absolute".into());
    }
    if path == "/" {
        return Ok(Vec::new());
    }
    path[1..]
        .split('/')
        .map(|part| Name::parse(part).map_err(Into::into))
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn resolve(volume: &mut Volume<ImageDevice>, path: &[Name]) -> Result<ObjectId> {
    let mut object = ROOT;
    for name in path {
        object = volume.lookup(object, name)?;
    }
    Ok(object)
}

/// Open an image read-only, or a raw device of `device_bytes` bytes.
///
/// A device is held exclusively for as long as it is open, so a mount can
/// neither be in place nor start while it is read. That also keeps `get` from
/// writing into the volume it reads.
pub(super) fn open_read_only(path: &Path, device_bytes: Option<u64>) -> Result<ImageDevice> {
    let Some(bytes) = device_bytes else {
        #[cfg(target_os = "linux")]
        return image_linux::open(path, MountMode::ReadOnly);
        #[cfg(not(target_os = "linux"))]
        return Ok(ImageDevice::open(path, MountMode::ReadOnly)?);
    };
    Ok(ImageDevice::from_device_file(open_exclusive(path)?, bytes)?)
}

/// The message when a mount or another program already holds the device.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
const IN_USE: &str = "the device is in use: unmount or eject it first";

/// Open the raw node a mounted daemon locks, and take the same lock: resolve
/// symlinks, and read `/dev/diskN` through `/dev/rdiskN`, since each node has
/// a lock of its own.
#[cfg(target_os = "macos")]
fn open_exclusive(path: &Path) -> Result<fs::File> {
    let path = fs::canonicalize(path)?;
    let path = match (path.parent(), path.file_name().and_then(OsStr::to_str)) {
        (Some(dev), Some(name)) if dev == Path::new("/dev") && name.starts_with("disk") => {
            dev.join(format!("r{name}"))
        }
        _ => path,
    };
    let file = match fs::File::open(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            authopen::open_read_only(&path)?
        }
        result => result?,
    };
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(IN_USE.into()),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Open the device with the kernel's exclusive claim, which a mounted
/// filesystem holds and a mount takes: it fails while either is in place.
#[cfg(target_os = "linux")]
fn open_exclusive(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_EXCL in asm-generic, which x86, Arm, and RISC-V use.
    const O_EXCL: i32 = 0o200;
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_EXCL)
        .open(path)
    {
        Err(error) if error.kind() == std::io::ErrorKind::ResourceBusy => Err(IN_USE.into()),
        result => Ok(result?),
    }
}

/// Open the partition with no sharing, which is how the CarryFS service owns
/// a mounted one: the open fails while any other handle has the volume, and
/// every later open, the service's included, fails until this one closes.
#[cfg(windows)]
fn open_exclusive(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
    {
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Err(IN_USE.into()),
        result => Ok(result?),
    }
}

#[cfg(target_os = "freebsd")]
fn open_exclusive(path: &Path) -> Result<std::fs::File> {
    Ok(carryfs_core::freebsd::open_device(
        path,
        MountMode::ReadOnly,
    )?)
}

/// Other platforms open the device without excluding a mount.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    windows
)))]
fn open_exclusive(path: &Path) -> Result<std::fs::File> {
    Ok(std::fs::File::open(path)?)
}

pub(super) fn scrub(image: &Path, device_bytes: Option<u64>) -> Result<()> {
    let mut volume = Volume::open(open_read_only(image, device_bytes)?)?;
    // Findings carry the original cause; avoid logging the terminal error twice.
    let mut reported = false;
    let summary = volume.scrub(&mut |finding| {
        reported = true;
        report(finding)
    });
    let summary = summary.map_err(|error| -> Box<dyn std::error::Error> {
        if reported {
            "scrub stopped; see reported failures".into()
        } else {
            error.into()
        }
    })?;
    volume.eject()?;
    println!(
        "Scrub: {} files, {} verified units, {} unreadable ranges",
        summary.inspected_files, summary.verified_units, summary.unreadable_ranges
    );
    if summary.unreadable_ranges != 0 {
        return Err("scrub found unreadable data".into());
    }
    Ok(())
}

/// Open an image for writing.
fn open_writable(image: &Path) -> Result<ImageDevice> {
    #[cfg(target_os = "linux")]
    return image_linux::open(image, MountMode::ReadWrite);
    #[cfg(not(target_os = "linux"))]
    return Ok(ImageDevice::open(image, MountMode::ReadWrite)?);
}

/// Let the image store modes and the hidden attribute.
pub(super) fn upgrade(image: &Path) -> Result<()> {
    let mut volume = Volume::open(open_writable(image)?)?;
    let changed = volume.upgrade()?;
    volume.eject()?;
    println!(
        "{}",
        if changed {
            "Image now stores permissions and the hidden attribute; CarryFS 0.1.0 can no longer open it"
        } else {
            "Image already stores permissions and the hidden attribute"
        }
    );
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
pub(super) fn get(
    image: &Path,
    device_bytes: Option<u64>,
    source: &OsStr,
    output: &Path,
) -> Result<()> {
    let mut volume = Volume::open(open_read_only(image, device_bytes)?)?;
    let object = resolve(&mut volume, &names(source)?)?;
    let mut sink = host::Sink::new(output, volume.stores_attributes())?;
    let summary = volume.extract_to(object, &mut sink, &mut report)?;
    sink.finish()?;
    volume.eject()?;
    println!(
        "Extraction: {} objects, {} verified bytes, {} unreadable ranges, traversal complete: {}",
        summary.objects, summary.verified_bytes, summary.unreadable_ranges, summary.complete
    );
    if !summary.complete || summary.unreadable_ranges != 0 {
        return Err("incomplete extraction; partial output retained".into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
pub(super) fn get(_: &Path, _: Option<u64>, _: &OsStr, _: &Path) -> Result<()> {
    Err("secure extraction is implemented only on Linux, macOS, and FreeBSD".into())
}

/// The mode and, on macOS, the Finder hidden flag of a host object. A volume
/// that does not store attributes ignores them.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn host_attributes(metadata: &fs::Metadata) -> NewAttributes {
    use std::os::unix::fs::PermissionsExt;
    #[cfg(target_os = "macos")]
    let hidden = {
        use std::os::macos::fs::MetadataExt;
        metadata.st_flags() & host::UF_HIDDEN != 0
    };
    #[cfg(not(target_os = "macos"))]
    let hidden = false;
    NewAttributes {
        mode: Some(Mode::from_host(metadata.permissions().mode())),
        hidden,
    }
}

/// Print the mode and hidden state of `path` in `image`, after applying
/// `changes`: `mode=OCTAL` and `hidden=yes` or `hidden=no`.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
pub(super) fn attr(image: &Path, path: &OsStr, changes: &[OsString]) -> Result<()> {
    const USAGE: &str = "expected mode=OCTAL (0 through 777) or hidden=yes|no";
    let mut change = AttributeChange::default();
    for text in changes {
        match text.to_str().ok_or(USAGE)?.split_once('=') {
            Some(("mode", octal)) => {
                let bits = u32::from_str_radix(octal, 8).map_err(|_| USAGE)?;
                if bits > 0o777 {
                    return Err(USAGE.into());
                }
                change.mode = Some(Mode::from_host(bits));
            }
            Some(("hidden", "yes")) => change.hidden = Some(true),
            Some(("hidden", "no")) => change.hidden = Some(false),
            _ => return Err(USAGE.into()),
        }
    }
    let path = names(path)?;
    let device = if change == AttributeChange::default() {
        open_read_only(image, None)?
    } else {
        open_writable(image)?
    };
    let mut volume = Volume::open(device)?;
    let object = resolve(&mut volume, &path)?;
    if change != AttributeChange::default() {
        if !volume.stores_attributes() {
            return Err(
                "the image does not store modes or the hidden attribute; run carryfs upgrade first"
                    .into(),
            );
        }
        let handle = volume.retain_object(object)?;
        let result = volume.set_attributes(&handle, change);
        volume.close_handle(handle)?;
        result?;
    }
    let attributes = volume.attributes(object)?;
    volume.eject()?;
    let kind = match attributes.kind {
        ObjectKind::File => "file",
        ObjectKind::Directory => "directory",
        ObjectKind::Symlink => "symlink",
    };
    println!(
        "{kind} {:04o}{}",
        attributes.mode.bits(),
        if attributes.hidden { " hidden" } else { "" }
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
pub(super) fn attr(_: &Path, _: &OsStr, _: &[OsString]) -> Result<()> {
    Err("attr is implemented only on Linux, macOS, and FreeBSD".into())
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn import_file(
    volume: &mut Volume<ImageDevice>,
    source: &Path,
    parent: ObjectId,
    name: Name,
    image: &fs::Metadata,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // A Linux write lease stops even this process from reopening its image.
    #[cfg(target_os = "linux")]
    {
        let metadata = fs::metadata(source)?;
        if metadata.dev() == image.dev() && metadata.ino() == image.ino() {
            return Err("cannot import the open image into itself".into());
        }
    }
    let mut input = host::open_source(source)?;
    let metadata = input.metadata()?;
    if metadata.dev() == image.dev() && metadata.ino() == image.ino() {
        return Err("cannot import the open image into itself".into());
    }
    let handle = volume.create_file_with(parent, name, host_attributes(&metadata))?;
    let result = (|| -> Result<()> {
        let mut buffer = vec![0; 65_536];
        let mut offset = 0_u64;
        loop {
            let len = match input.read(&mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if len == 0 {
                break;
            }
            // Omit zero runs; truncate below establishes EOF even for trailing holes.
            if buffer[..len].iter().any(|byte| *byte != 0) {
                let mut written = 0;
                while written < len {
                    let n =
                        volume.write(&handle, offset + written as u64, &buffer[written..len])?;
                    if n == 0 {
                        return Err("file write made no progress".into());
                    }
                    written += n;
                }
            }
            offset = offset
                .checked_add(len as u64)
                .ok_or("import file is too large")?;
        }
        volume.truncate(&handle, offset)?;
        Ok(())
    })();
    let close = volume.close_handle(handle);
    result?;
    close?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
pub(super) fn put(image: &Path, source: &Path, destination: &OsStr) -> Result<()> {
    let path = names(destination)?;
    let (name, parents) = path
        .split_last()
        .ok_or("put requires a new destination name")?;
    let image_metadata = fs::metadata(image)?;
    let mut volume = Volume::open(open_writable(image)?)?;
    let parent = resolve(&mut volume, parents)?;
    match volume.lookup(parent, name) {
        Ok(_) => return Err("import destination already exists".into()),
        Err(carryfs_core::Error::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    let mut pending = Some((source.to_path_buf(), parent, name.clone()));
    let mut stack: Vec<(fs::ReadDir, ObjectId)> = Vec::new();
    let result = (|| -> Result<()> {
        loop {
            if let Some((source, parent, name)) = pending.take() {
                let metadata = fs::symlink_metadata(&source)?;
                if metadata.is_dir() {
                    let entries = fs::read_dir(&source)?;
                    let directory =
                        volume.create_directory_with(parent, name, host_attributes(&metadata))?;
                    stack.push((entries, directory));
                } else if metadata.is_file() {
                    import_file(&mut volume, &source, parent, name, &image_metadata)?;
                } else if metadata.is_symlink() {
                    let target = fs::read_link(&source)?;
                    let object =
                        volume.create_symlink(parent, name, target.as_os_str().as_bytes())?;
                    if volume.stores_attributes() && host_attributes(&metadata).hidden {
                        let handle = volume.retain_object(object)?;
                        let result = volume.set_attributes(
                            &handle,
                            AttributeChange {
                                hidden: Some(true),
                                ..Default::default()
                            },
                        );
                        volume.close_handle(handle)?;
                        result?;
                    }
                } else {
                    return Err(format!("unsupported import entry: {}", source.display()).into());
                }
            }
            let Some((entries, parent)) = stack.last_mut() else {
                break;
            };
            if let Some(entry) = entries.next() {
                let entry = entry?;
                let name = entry.file_name();
                pending = Some((
                    entry.path(),
                    *parent,
                    Name::parse(name.to_str().ok_or("import name is not UTF-8")?)?,
                ));
            } else {
                stack.pop();
            }
        }
        volume.eject()?;
        Ok(())
    })();
    result.map_err(|error| format!("import failed; partial destination may remain: {error}").into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
pub(super) fn put(_: &Path, _: &Path, _: &OsStr) -> Result<()> {
    Err("offline import is implemented only on Linux, macOS, and FreeBSD".into())
}
