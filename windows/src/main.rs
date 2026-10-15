//! CarryFS adapter for Windows 10: WinFsp operations, a volume backend, and an
//! automount service.

// Platform-independent so it tests on any host.
#[cfg_attr(not(windows), allow(dead_code))]
mod cache;
#[cfg(windows)]
mod device;
#[cfg(windows)]
mod ffi;
#[cfg(windows)]
mod service;
// Platform-independent so it tests on any host.
#[cfg_attr(not(windows), allow(dead_code))]
mod writer;
// Platform-independent so it tests on any host.
#[cfg_attr(not(windows), allow(dead_code))]
mod waits;
// The operations are platform-independent so they test on any host.
#[cfg_attr(not(windows), allow(dead_code))]
mod fs;

use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["mkfs", volume] => mkfs(volume, false),
        ["mkfs", volume, "force"] => mkfs(volume, true),
        ["wipe", volume] => wipe(volume),
        ["probe", volume] => probe(volume),
        ["service"] => service::run(),
        ["serve", volume] => serve(volume, None, false),
        ["serve", volume, letter] => serve(volume, Some(letter), false),
        ["serve", volume, letter, "ro"] => serve(volume, Some(letter), true),
        _ => Err(
            "usage: carryfs-windows mkfs VOLUME [force] | carryfs-windows wipe VOLUME | carryfs-windows probe VOLUME | carryfs-windows serve VOLUME [X: [ro]] | carryfs-windows service".into(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("carryfs: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("carryfs-windows supports only Windows");
    ExitCode::FAILURE
}

#[cfg(windows)]
type Result<T> = std::result::Result<T, String>;

/// Open a partition to rewrite its reserved bands. That owns the volume, so
/// it fails while anything, the service included, has it open. It refuses a
/// file system Windows mounts: the bands hold that file system's start.
#[cfg(windows)]
fn open_owned(volume_path: &str, action: &str) -> Result<device::Disk> {
    use carryfs_core::{Error, MountMode};
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    let disk = device::Disk::open(volume_path.as_ref(), MountMode::ReadWrite, true).map_err(
        |e| match e {
            Error::Io { source, .. }
                if source.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) =>
            {
                format!(
                    "{volume_path} is in use; if the CarryFS service mounted it, stop the service first (sc stop CarryFS)"
                )
            }
            e => format!("{volume_path}: {e}"),
        },
    )?;
    if let Some(name) = disk.file_system() {
        return Err(format!(
            "{volume_path} holds {name}; refusing to {action} it"
        ));
    }
    Ok(disk)
}

/// Format a whole partition. The core formats only when the 2 MiB reserved
/// bands at each end are zero (FORMAT.md), which a used partition's are not;
/// `force` zeroes them first, erasing whatever the partition held.
#[cfg(windows)]
fn mkfs(volume_path: &str, force: bool) -> Result<()> {
    use carryfs_core::{Error, Volume};

    let mut disk = open_owned(volume_path, "format")?;
    if force {
        carryfs_core::erase_reserved_bands(&mut disk)
            .map_err(|e| format!("erase reserved bands: {e}"))?;
    }
    let mut volume = Volume::format(disk).map_err(|e| match e {
        Error::InvalidInput(reason) if reason.starts_with("existing or unfinished format") => {
            format!("{volume_path} is not blank; add `force` to erase it")
        }
        e => format!("format: {e}"),
    })?;
    let info = volume.info().map_err(|e| e.to_string())?;
    volume.eject().map_err(|e| format!("eject: {e}"))?;
    println!(
        "carryfs {} formatted, {} bytes",
        format_uuid(&info.uuid),
        info.byte_len
    );
    Ok(())
}

/// Erase CarryFS so any tool can format the partition, as `mkfs.carryfs
/// --wipe` does on Linux. Zeroing both reserved bands removes every root and
/// workspace bank. A Windows format alone leaves root B, which revives the
/// volume once the partition is recreated where it was.
#[cfg(windows)]
fn wipe(volume_path: &str) -> Result<()> {
    let mut disk = open_owned(volume_path, "wipe")?;
    // Anything else is Windows' to format; this keeps a mistyped volume
    // path from losing the start of another file system.
    if !carryfs_core::has_root_magic(&mut disk) {
        return Err(format!("{volume_path}: no CarryFS volume found"));
    }
    carryfs_core::erase_reserved_bands(&mut disk)
        .map_err(|e| format!("erase reserved bands: {e}"))?;
    println!("wiped CarryFS from {volume_path}; format it with any tool now");
    Ok(())
}

/// Print `carryfs UUID` when the volume carries a CarryFS root record.
#[cfg(windows)]
fn probe(volume: &str) -> Result<()> {
    use carryfs_core::{MountMode, Volume};

    let mut disk = device::Disk::open(volume.as_ref(), MountMode::ReadOnly, false)
        .map_err(|e| format!("{volume}: {e}"))?;
    if !carryfs_core::has_root_magic(&mut disk) {
        println!("not carryfs");
        return Ok(());
    }
    match Volume::open(disk).and_then(|volume| volume.info()) {
        Ok(info) => println!("carryfs {}", format_uuid(&info.uuid)),
        Err(error) => println!("carryfs damaged: {error}"),
    }
    Ok(())
}

/// Mount in the foreground until standard input closes, then eject.
#[cfg(windows)]
fn serve(volume: &str, letter: Option<&str>, read_only: bool) -> Result<()> {
    let (mount, _) = mount(volume, letter, read_only)?;
    println!("mounted {}", mount.mount_point);
    std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).map_err(|e| e.to_string())?;
    eject(mount)
}

/// Whether `volume` carries CarryFS roots and no file system Windows mounts.
/// A Windows quick format leaves root B behind, and mounting those roots
/// would take the partition from under the new file system and overwrite it.
/// The check runs on a shared handle, before the mount's exclusive open.
#[cfg(windows)]
fn is_carryfs(volume: &str) -> bool {
    let Ok(mut disk) =
        device::Disk::open(volume.as_ref(), carryfs_core::MountMode::ReadOnly, false)
    else {
        return false;
    };
    if !carryfs_core::has_root_magic(&mut disk) {
        return false;
    }
    if let Some(name) = disk.file_system() {
        eprintln!("carryfs: {volume}: {name} is mounted over old CarryFS roots; not mounting");
        return false;
    }
    true
}

/// Held from choosing a drive letter until the mount owns it.
#[cfg(windows)]
static LETTERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock and mount a volume on `letter`, or on the highest free drive letter.
/// Also returns the volume handle for device notifications and its release.
#[cfg(windows)]
fn mount(
    volume: &str,
    letter: Option<&str>,
    read_only: bool,
) -> Result<(
    ffi::Mount,
    (windows_sys::Win32::Foundation::HANDLE, device::Release),
)> {
    use carryfs_core::{MountMode, Volume};

    let mode = if read_only {
        MountMode::ReadOnly
    } else {
        MountMode::ReadWrite
    };
    let disk =
        device::Disk::open(volume.as_ref(), mode, true).map_err(|e| format!("{volume}: {e}"))?;
    let handle = (
        disk.handle().ok_or("volume handle is closed")?,
        disk.release(),
    );
    // Opening runs the full check, which reads metadata in runs of adjacent
    // pages; later requests are scattered, so the chunked reads stop after it.
    let scan = disk.scan();
    scan.set(true);
    let waits = disk.waits();
    let volume = Volume::open(disk).map_err(|e| format!("open volume: {e}"));
    // A read-only open checks only the root pages, so its first listings are
    // what reads the metadata, in the same runs; it keeps the chunked reads.
    scan.set(read_only);
    let mut volume = volume?;
    if read_only {
        // As on Linux and macOS: nothing else can change the partition while
        // this mount owns it, so verified metadata is reused between requests.
        volume
            .enable_readonly_metadata_cache()
            .map_err(|e| format!("open volume: {e}"))?;
    }
    let info = volume.info().map_err(|e| e.to_string())?;
    let serial = u32::from_le_bytes([info.uuid[0], info.uuid[1], info.uuid[2], info.uuid[3]]);
    let carry = fs::CarryFs::new(volume, read_only)
        .map_err(|e| format!("open volume: {e}"))?
        .with_waits(waits);
    // Opening is slow and runs concurrently; choosing a letter and taking it
    // must not, or two volumes finishing together pick the same one.
    let _choosing = LETTERS.lock().unwrap_or_else(|e| e.into_inner());
    let letter = match letter {
        Some(letter) => letter.to_string(),
        None => free_drive_letter().ok_or("no free drive letter")?,
    };
    let mount = ffi::Mount::start(carry, &letter, read_only, serial)?;
    Ok((mount, handle))
}

/// Unmount, close what WinFsp left open, and release the volume after a sync.
#[cfg(windows)]
fn eject(mount: ffi::Mount) -> Result<()> {
    let carry = mount.stop();
    carry
        .close_all()
        .map_err(|code| format!("closing handles failed: NTSTATUS {code:#010x}"))?;
    let mut volume = carry
        .into_volume()
        .map_err(|code| format!("releasing volume failed: NTSTATUS {code:#010x}"))?;
    volume.eject().map_err(|e| format!("eject: {e}"))?;
    println!("ejected");
    Ok(())
}

/// The lowest unused drive letter from D:, as Windows assigns to removable disks.
/// The service cannot see a user's mapped network drives, which shadow a global
/// letter in that user's session; those usually take letters counting down from Z:.
#[cfg(windows)]
fn free_drive_letter() -> Option<String> {
    // SAFETY: GetLogicalDrives takes no arguments.
    let used = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (b'D'..=b'Z')
        .find(|letter| used & (1 << (letter - b'A')) == 0)
        .map(|letter| format!("{}:", letter as char))
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[cfg(windows)]
fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
