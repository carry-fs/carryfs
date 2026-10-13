//! `carryfs.util`, the DiskArbitration probe helper.
//!
//! macFUSE (`mount_carryfs`, fuse/) mounts the volume, but
//! DiskArbitration identifies a disk by running the `FSProbeExecutable` named
//! in a `/Library/Filesystems/carryfs.fs` bundle. Without it a
//! CarryFS disk has no filesystem in DiskArbitration's description, and
//! `diskutil unmount` and Finder's Eject both refuse to act on it.
//!
//!     carryfs.util -p disk4s1 removable writable
//!     carryfs.util -k disk4s1
//!
//! `diskarbitrationd` runs `-k` right after a successful `-p` to learn the
//! volume's persistent UUID (`DAFileSystem.c`'s `__DAFileSystemProbeCallbackStage1`).
//! Without a `DAVolumeUUID`, storagekitd files the disk under
//! `kSKDiskRoleUnknown` and Finder's sidebar, desktop, and `every disk`
//! AppleScript enumeration omit it, even though `diskutil` and Disk Utility,
//! which read DiskArbitration's disk list directly, still show it mounted.
//!
//! Exit codes and stdout follow `<sys/loadable_fs.h>`; `-k` is Apple's
//! undocumented extension to that protocol, and its output must be a
//! standard 8-4-4-4-12 UUID string (`___CFUUIDCreateFromString` rejects
//! anything else).

use std::process::ExitCode;

#[cfg(target_os = "macos")]
use carryfs_core::{MountMode, Volume};

#[cfg(target_os = "macos")]
mod raw;

/// `FSUR_RECOGNIZED`, as an unsigned exit status.
#[cfg(target_os = "macos")]
const RECOGNIZED: u8 = 255;
/// `FSUR_UNRECOGNIZED`.
#[cfg(target_os = "macos")]
const UNRECOGNIZED: u8 = 254;
/// `FSUR_IO_SUCCESS`, the `-k` success status.
#[cfg(target_os = "macos")]
const IO_SUCCESS: u8 = 253;
/// `FSUR_INVAL`.
#[cfg(target_os = "macos")]
const INVALID: u8 = 250;

/// Open `bsd_name`'s raw device and read its selected-root information.
#[cfg(target_os = "macos")]
fn volume_info(bsd_name: &str) -> Option<carryfs_core::VolumeInfo> {
    if bsd_name.contains('/') {
        return None;
    }
    let device = raw::RawDevice::open(format!("/dev/r{bsd_name}"), MountMode::ReadOnly).ok()?;
    Volume::open(device).ok()?.info().ok()
}

/// Report the volume name a CarryFS device carries.
///
/// The probe and mount helper use the same stored label or UUID fallback.
#[cfg(target_os = "macos")]
fn probe(bsd_name: &str) -> Option<String> {
    Some(volume_info(bsd_name)?.name())
}

/// Report the volume UUID as the 8-4-4-4-12 string DiskArbitration requires.
#[cfg(target_os = "macos")]
fn probe_uuid(bsd_name: &str) -> Option<String> {
    let uuid = volume_info(bsd_name)?.uuid;
    Some(format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        uuid[0],
        uuid[1],
        uuid[2],
        uuid[3],
        uuid[4],
        uuid[5],
        uuid[6],
        uuid[7],
        uuid[8],
        uuid[9],
        uuid[10],
        uuid[11],
        uuid[12],
        uuid[13],
        uuid[14],
        uuid[15],
    ))
}

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    // `std::env::args` panics on an argument that is not UTF-8.
    let Ok(arguments) = std::env::args_os()
        .skip(1)
        .map(std::ffi::OsString::into_string)
        .collect::<Result<Vec<String>, _>>()
    else {
        return ExitCode::from(INVALID);
    };
    match arguments.split_first() {
        Some((command, rest)) if command == "-p" => match rest.first() {
            Some(bsd_name) => match probe(bsd_name) {
                Some(name) => {
                    println!("{name}");
                    ExitCode::from(RECOGNIZED)
                }
                None => ExitCode::from(UNRECOGNIZED),
            },
            None => ExitCode::from(INVALID),
        },
        Some((command, rest)) if command == "-k" => match rest.first() {
            Some(bsd_name) => match probe_uuid(bsd_name) {
                Some(uuid) => {
                    println!("{uuid}");
                    ExitCode::from(IO_SUCCESS)
                }
                None => ExitCode::from(UNRECOGNIZED),
            },
            None => ExitCode::from(INVALID),
        },
        // Mounting and unmounting belong to mount_carryfs; only
        // probing is served here.
        _ => ExitCode::from(INVALID),
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("carryfs.util requires macOS");
    ExitCode::FAILURE
}
