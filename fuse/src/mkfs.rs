//! `mkfs.carryfs [-f] [-L LABEL] DEVICE`, which mkfs(8) runs for
//! `mkfs -t carryfs [-f] [-L LABEL] DEVICE`, and `mkfs.carryfs --wipe DEVICE`.
//!
//! `Volume::format` refuses a device whose reserved bands hold data, and
//! every common file system keeps its start there. Like mkfs.xfs and mkfs.btrfs,
//! this tool keeps that refusal, so a mistyped device name cannot destroy a
//! file system; `-f` erases the bands first. Windows' `mkfs ... force` does
//! the same. macOS `newfs_carryfs` erases unasked: Disk Utility's Erase is
//! the confirmation there.
//!
//! `--wipe` erases only those bands. libblkid does not know CarryFS, so wipefs
//! and udisks leave its roots in place, the udev probe keeps reporting it, and
//! GNOME Disks times out formatting it as anything else.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::ErrorKind;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use carryfs_core::{Error, MountMode, Volume, VolumeLabel, has_root_magic};

use crate::device::Disk;

type Result<T> = std::result::Result<T, Box<dyn StdError>>;

enum Request {
    Format {
        label: Option<VolumeLabel>,
        /// Erase what the device holds first.
        force: bool,
    },
    Wipe,
}

#[cfg(target_os = "linux")]
const USAGE: &str = "usage: mkfs.carryfs [-f] [-L LABEL] DEVICE | mkfs.carryfs --wipe DEVICE";

#[cfg(target_os = "freebsd")]
const USAGE: &str = "usage: newfs_carryfs [-f] [-L LABEL] DEVICE | newfs_carryfs --wipe DEVICE";

/// Format options, then the device.
pub fn main(args: &[OsString]) -> Result<()> {
    // Validate before opening or erasing the device.
    let Some((device, options)) = args.split_last() else {
        return Err(USAGE.into());
    };
    // A lone option, such as `--wipe` without its device, is not a path.
    if device.as_encoded_bytes().starts_with(b"-") {
        return Err(USAGE.into());
    }
    let request = if options == ["--wipe"] {
        Request::Wipe
    } else {
        let (mut label, mut force) = (None, false);
        let mut options = options.iter();
        while let Some(option) = options.next() {
            if option == "-f" && !force {
                force = true;
            } else if option == "-L" && label.is_none() {
                let value = options.next().ok_or(USAGE)?;
                let value = value.to_str().ok_or("the label must be UTF-8")?;
                label = Some(VolumeLabel::parse(value)?);
            } else {
                return Err(USAGE.into());
            }
        }
        Request::Format { label, force }
    };
    let device = Path::new(device);
    let done = run(device, request).map_err(|error| format!("{}: {error}", device.display()))?;
    println!("{done}");
    Ok(())
}

fn run(device: &Path, request: Request) -> Result<String> {
    // The kernel's exclusive claim, which a mounted filesystem holds: it fails
    // while the device is mounted and keeps a mount from starting until the
    // erase ends. `Disk` opens without it, as it does behind a mount's claim.
    #[cfg(target_os = "linux")]
    let claim = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EXCL)
        .open(device)
    {
        Err(error) if error.kind() == ErrorKind::ResourceBusy => {
            return Err("the device is in use: unmount it first".into());
        }
        result => result?,
    };
    #[cfg(target_os = "freebsd")]
    let claim = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(device)?;
    let partitions = partitions(&claim)?;
    if !partitions.is_empty() {
        return Err(format!(
            "the disk holds partitions {}: use one of them, or erase the partition table with the platform disk tools first",
            partitions.join(", ")
        )
        .into());
    }
    let mut disk = Disk::open(device, MountMode::ReadWrite)?;
    match request {
        Request::Wipe => {
            // Anything else is wipefs's to erase; this keeps a mistyped
            // device name from losing the start of another filesystem.
            if !has_root_magic(&mut disk) {
                return Err(
                    "no CarryFS volume found: use the platform disk tools for other filesystems"
                        .into(),
                );
            }
            carryfs_core::erase_reserved_bands(&mut disk)?;
            Ok(format!(
                "Wiped CarryFS from {}; format it with any tool now",
                device.display()
            ))
        }
        Request::Format { label, force } => {
            if force {
                carryfs_core::erase_reserved_bands(&mut disk)?;
            }
            let formatted = match label {
                Some(label) => Volume::format_with_label(disk, label),
                None => Volume::format(disk),
            };
            let mut volume = formatted.map_err(|error| match error {
                Error::InvalidInput(reason)
                    if reason.starts_with("existing or unfinished format") =>
                {
                    "the device holds data, perhaps a file system; add -f to erase it and format"
                        .into()
                }
                error => Box::<dyn StdError>::from(error),
            })?;
            let bytes = volume.info()?.byte_len;
            volume.eject()?;
            Ok(format!(
                "Formatted {} as CarryFS, {bytes} bytes",
                device.display()
            ))
        }
    }
}

/// The partitions the kernel found on a whole disk, whose table formatting it
/// would erase. A partition has none.
#[cfg(target_os = "linux")]
fn partitions(device: &File) -> Result<Vec<String>> {
    let number = device.metadata()?.rdev();
    let disk = format!(
        "/sys/dev/block/{}:{}",
        libc::major(number),
        libc::minor(number)
    );
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&disk).map_err(|error| format!("{disk}: {error}"))? {
        let entry = entry?;
        if entry.path().join("partition").exists() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// A provider holds partitions when gpart knows a partition table on it.
/// Asking about this provider alone works on a host with no tables at all.
#[cfg(target_os = "freebsd")]
fn partitions(device: &File) -> Result<Vec<String>> {
    let name = carryfs_core::freebsd::provider_name(device)?;
    let output = std::process::Command::new("/sbin/gpart")
        .args(["show", "-p", &name])
        .output()?;
    if output.status.success() {
        return Ok(vec![name]);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such geom") {
        return Ok(Vec::new());
    }
    Err(format!("gpart show {name} failed: {stderr}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_arguments_before_opening_the_device() {
        let missing = "/nonexistent/carryfs-mkfs";
        for args in [
            &[][..],
            &["-L", missing][..],
            &["-L", "a/b", missing][..],
            &["-n", "name", missing][..],
            &["--wipe"][..],
            &["--wipe", "-L", missing][..],
            &["--wipe", "-f", missing][..],
            &["-f", "-f", missing][..],
            &["-f"][..],
        ] {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            let error = main(&args).unwrap_err().to_string();
            // None of these paths exist, so an open fails with an OS error.
            assert!(
                !error.contains("os error"),
                "{args:?} opened a device: {error}"
            );
        }
    }
}
