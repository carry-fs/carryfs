//! `fsck_carryfs`, the check helper storagekitd runs for
//! `diskutil verifyVolume`, `diskutil repairVolume`, and Disk Utility's First Aid.
//!
//!     fsck_carryfs -q /dev/rdiskNsM    quick check DiskArbitration runs before mounting
//!     fsck_carryfs -n /dev/rdiskNsM    verify
//!     fsck_carryfs -y /dev/rdiskNsM    "repair"
//!
//! `-q` only selects the root and validates its tree-root pages: the writable
//! open that follows runs the complete structural check anyway, so repeating
//! it here doubled mount time. `-n` and `-y` run that complete check on a
//! read-only device. CarryFS has no
//! in-place repair, so `-y` reports damage the same way and points to
//! recovery by extraction. Exit status 0 means the check passed, 8 that it
//! did not, as with the other fsck tools.

use std::process::ExitCode;

#[cfg(target_os = "macos")]
mod raw;

#[cfg(target_os = "macos")]
const FAILED: u8 = 8;

#[cfg(target_os = "macos")]
fn check(path: &str, quick: bool) -> carryfs_core::Result<Option<carryfs_core::CheckSummary>> {
    use carryfs_core::{MountMode, Volume};

    let mut volume = Volume::open(raw::RawDevice::open(path, MountMode::ReadOnly)?)?;
    let summary = if quick { None } else { Some(volume.check()?) };
    volume.eject()?;
    Ok(summary)
}

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    // `std::env::args` panics on an argument that is not UTF-8.
    let Ok(arguments) = std::env::args_os()
        .skip(1)
        .map(std::ffi::OsString::into_string)
        .collect::<Result<Vec<String>, _>>()
    else {
        eprintln!("usage: fsck_carryfs -q|-n|-y [-x] /dev/rdiskNsM");
        return ExitCode::from(FAILED);
    };
    // -x asks for the progress plist Apple's fsck tools emit. It is not
    // implemented, which is why the personality declares no
    // FSXMLOutputArgument: storagekitd reports a verify that answers -x with
    // plain text as failed, whatever the exit status. The flag is tolerated
    // for a hand-run tool.
    let (flags, rest): (Vec<&String>, Vec<&String>) =
        arguments.iter().partition(|a| a.starts_with('-'));
    let repair = flags.iter().any(|f| *f == "-y");
    let quick = flags.iter().any(|f| *f == "-q") && !repair && !flags.iter().any(|f| *f == "-n");
    let [path] = rest.as_slice() else {
        eprintln!("usage: fsck_carryfs -q|-n|-y [-x] /dev/rdiskNsM");
        return ExitCode::from(FAILED);
    };
    if flags
        .iter()
        .any(|f| !["-n", "-y", "-q", "-x"].contains(&f.as_str()))
    {
        eprintln!("usage: fsck_carryfs -q|-n|-y [-x] /dev/rdiskNsM");
        return ExitCode::from(FAILED);
    }
    println!("** {path}");
    match check(path, quick) {
        Ok(None) => {
            println!("** Root check passed; mounting writable runs the structural check");
            ExitCode::SUCCESS
        }
        Ok(Some(summary)) => {
            println!(
                "** Structural check passed: {} objects, {} metadata pages, {} data blocks, {} free blocks",
                summary.objects, summary.metadata_pages, summary.data_blocks, summary.free_blocks
            );
            println!("** File data checksums were not scrubbed; run carryfs scrub for that");
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!("** Check failed: {error}");
            if repair {
                println!(
                    "** CarryFS does not repair in place. Copy files off with carryfs get, then erase"
                );
            }
            ExitCode::from(FAILED)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    eprintln!("fsck_carryfs requires macOS");
    ExitCode::FAILURE
}
