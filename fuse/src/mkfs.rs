//! `mkfs.carryfs [-L LABEL] DEVICE`, which mkfs(8) runs for
//! `mkfs -t carryfs [-L LABEL] DEVICE`, and `mkfs.carryfs --wipe DEVICE`.
//!
//! `Volume::format` refuses a device whose reserved bands hold data, so erasing
//! them first is this tool's job: running it is the explicit request to destroy
//! what the device held. macOS `newfs_carryfs` does the same.
//!
//! `--wipe` erases only those bands. libblkid does not know CarryFS, so wipefs
//! and udisks leave its roots in place, the udev probe keeps reporting it, and
//! GNOME Disks times out formatting it as anything else.

use std::error::Error as StdError;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use carryfs_core::{MountMode, Volume, VolumeLabel, has_root_magic};

use crate::device::Disk;

type Result<T> = std::result::Result<T, Box<dyn StdError>>;

enum Request {
    Format(Option<VolumeLabel>),
    Wipe,
}

/// Arguments as mkfs(8) passes them: options, then the device.
pub fn main(args: &[OsString]) -> Result<()> {
    // Validate before opening or erasing the device.
    let (device, request) = match args {
        // A lone option, such as `--wipe` without its device, is not a path.
        [device] if !device.as_encoded_bytes().starts_with(b"-") => (device, Request::Format(None)),
        [flag, label, device] if flag == "-L" => {
            let label = label.to_str().ok_or("the label must be UTF-8")?;
            (device, Request::Format(Some(VolumeLabel::parse(label)?)))
        }
        [flag, device] if flag == "--wipe" => (device, Request::Wipe),
        _ => {
            return Err(
                "usage: mkfs.carryfs [-L LABEL] DEVICE | mkfs.carryfs --wipe DEVICE".into(),
            );
        }
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
    let mut disk = Disk::open(device, MountMode::ReadWrite)?;
    let partitions = partitions(&claim)?;
    if !partitions.is_empty() {
        return Err(format!(
            "the disk holds partitions {}: use one of them, or erase the partition table with wipefs first",
            partitions.join(", ")
        )
        .into());
    }
    match request {
        Request::Wipe => {
            // Anything else is wipefs's to erase; this keeps a mistyped
            // device name from losing the start of another filesystem.
            if !has_root_magic(&mut disk) {
                return Err("no CarryFS volume found: use wipefs for other filesystems".into());
            }
            carryfs_core::erase_reserved_bands(&mut disk)?;
            Ok(format!(
                "Wiped CarryFS from {}; format it with any tool now",
                device.display()
            ))
        }
        Request::Format(label) => {
            carryfs_core::erase_reserved_bands(&mut disk)?;
            let mut volume = match label {
                Some(label) => Volume::format_with_label(disk, label)?,
                None => Volume::format(disk)?,
            };
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
