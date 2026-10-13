//! `newfs_carryfs`, the format helper storagekitd runs for
//! `diskutil eraseVolume CarryFS NAME diskNsM` and Disk Utility's Erase.
//!
//!     newfs_carryfs -v NAME /dev/rdiskNsM
//!
//! It runs as root after the volume is unmounted. `Volume::format` refuses a
//! device whose reserved bands hold data, so erasing them first is this tool's
//! job: that is the explicit request to destroy what the device held.

use std::process::ExitCode;

#[cfg(target_os = "macos")]
mod raw;

#[cfg(target_os = "macos")]
fn format(path: &str, label: Option<carryfs_core::VolumeLabel>) -> carryfs_core::Result<u64> {
    use carryfs_core::{BlockDevice, MountMode, Volume};

    // FORMAT.md: the first and last 512 blocks must be zero before format.
    const BAND_BYTES: u64 = 512 * 4096;
    let mut device = raw::RawDevice::open(path, MountMode::ReadWrite)?;
    let geometry = device.geometry()?;
    // Before the erase: a device that cannot be formatted keeps what it holds.
    geometry.validate()?;
    let byte_len = geometry.byte_len;
    let tail = byte_len
        .checked_sub(BAND_BYTES)
        .ok_or(carryfs_core::Error::InvalidGeometry)?;
    let zeros = vec![0; BAND_BYTES as usize];
    device.write_all_at(0, &zeros)?;
    device.write_all_at(tail, &zeros)?;
    device.flush_durable()?;
    let mut volume = match label {
        Some(label) => Volume::format_with_label(device, label)?,
        None => Volume::format(device)?,
    };
    volume.eject()?;
    Ok(byte_len)
}

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    // `std::env::args` panics on an argument that is not UTF-8.
    let Ok(arguments) = std::env::args_os()
        .skip(1)
        .map(std::ffi::OsString::into_string)
        .collect::<Result<Vec<String>, _>>()
    else {
        eprintln!("usage: newfs_carryfs [-v NAME] /dev/rdiskNsM");
        return ExitCode::FAILURE;
    };
    // Validate before opening or erasing the device.
    let (path, label) = match arguments.as_slice() {
        [flag, name, path] if flag == "-v" => match carryfs_core::VolumeLabel::parse(name) {
            Ok(label) => (path, Some(label)),
            Err(error) => {
                eprintln!("newfs_carryfs: {error}");
                return ExitCode::FAILURE;
            }
        },
        [path] => (path, None),
        _ => {
            eprintln!("usage: newfs_carryfs [-v NAME] /dev/rdiskNsM");
            return ExitCode::FAILURE;
        }
    };
    match format(path, label) {
        Ok(bytes) => {
            println!("Formatted {path} as CarryFS, {bytes} bytes");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("newfs_carryfs: {path}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("newfs_carryfs requires macOS");
    ExitCode::FAILURE
}
