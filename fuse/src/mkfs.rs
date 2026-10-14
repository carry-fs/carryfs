//! `mkfs.carryfs DEVICE`, which mkfs(8) runs for `mkfs -t carryfs DEVICE`.
//!
//! `Volume::format` refuses a device whose reserved bands hold data, so erasing
//! them first is this tool's job: running it is the explicit request to destroy
//! what the device held. macOS `newfs_carryfs` does the same.

use std::error::Error as StdError;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use carryfs_core::{BlockDevice, MountMode, Volume};

use crate::device::Disk;

type Result<T> = std::result::Result<T, Box<dyn StdError>>;

/// FORMAT.md: the first and last 512 blocks must be zero before format.
const BAND_BYTES: u64 = 512 * 4096;

pub fn mkfs(device: &Path) -> Result<()> {
    // The kernel's exclusive claim, which a mounted filesystem holds: it fails
    // while the device is mounted and keeps a mount from starting until the
    // format ends. `Disk` opens without it, as it does behind a mount's claim.
    let _claim = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EXCL)
        .open(device)
    {
        Err(error) if error.kind() == ErrorKind::ResourceBusy => {
            return Err(format!(
                "{}: the device is in use: unmount it first",
                device.display()
            )
            .into());
        }
        result => result.map_err(|error| format!("{}: {error}", device.display()))?,
    };
    let disk = Disk::open(device, MountMode::ReadWrite)
        .map_err(|error| format!("{}: {error}", device.display()))?;
    let bytes = erase_and_format(disk).map_err(|error| format!("{}: {error}", device.display()))?;
    println!("Formatted {} as CarryFS, {bytes} bytes", device.display());
    Ok(())
}

fn erase_and_format<D: BlockDevice>(mut device: D) -> carryfs_core::Result<u64> {
    let geometry = device.geometry()?;
    // Before the erase: a device that cannot be formatted keeps what it holds.
    geometry.validate()?;
    let zeros = vec![0; BAND_BYTES as usize];
    device.write_all_at(0, &zeros)?;
    device.write_all_at(geometry.byte_len - BAND_BYTES, &zeros)?;
    device.flush_durable()?;
    let mut volume = Volume::format(device)?;
    volume.eject()?;
    Ok(geometry.byte_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use carryfs_core::ImageDevice;

    #[test]
    fn formats_over_a_previous_filesystem() {
        let path = std::env::temp_dir().join(format!("carryfs-mkfs-{}.img", std::process::id()));
        let open = |mode| ImageDevice::open(&path, mode).unwrap();
        ImageDevice::create_new(&path, 256 << 20).unwrap();
        let mut device = open(MountMode::ReadWrite);
        device.write_all_at(0, &[0xaa; 4096]).unwrap();
        device
            .write_all_at((256 << 20) - 4096, &[0xaa; 4096])
            .unwrap();
        device.flush_durable().unwrap();
        assert!(Volume::format(device).is_err());

        assert_eq!(
            erase_and_format(open(MountMode::ReadWrite)).unwrap(),
            256 << 20
        );
        Volume::open(open(MountMode::ReadOnly))
            .unwrap()
            .check()
            .unwrap();
        std::fs::remove_file(&path).unwrap();
    }
}
