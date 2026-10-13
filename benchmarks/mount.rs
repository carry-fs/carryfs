// The build command prefixes this with modules from the selected source tree.
// No host paths are compiled in here.
use carryfs_core::{MountMode, Volume};
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, path, point] = args.as_slice() else {
        return Err("usage: carryfs-bench-mount ro|rw DEVICE MOUNTPOINT".into());
    };
    let read_only = match mode.as_str() {
        "ro" => true,
        "rw" => false,
        _ => return Err("mount mode must be ro or rw".into()),
    };
    let mode = if read_only {
        MountMode::ReadOnly
    } else {
        MountMode::ReadWrite
    };
    let _claim = OpenOptions::new()
        .read(true)
        .write(!read_only)
        .custom_flags(libc::O_EXCL)
        .open(path)?;
    // As `serve` does before it opens the volume.
    fs::run_at_full_speed();
    let mut disk = device::Disk::open(Path::new(path), mode)?;
    if read_only {
        disk.enable_metadata_prefetch(Path::new(path))?;
    }
    let mut volume = Volume::open(disk)?;
    if read_only {
        volume.enable_readonly_metadata_cache()?;
    } else {
        volume.set_deferred_checkpoints(true)?;
    }
    // SAFETY: getuid and getgid take no arguments and cannot fail.
    let owner = unsafe {
        fs::Owner {
            uid: libc::getuid(),
            gid: libc::getgid(),
        }
    };
    let (mut filesystem, _) = fs::CarryFs::new(volume, owner, false);
    if read_only {
        filesystem.set_read_only_cache();
    }
    let names = filesystem.finisher();
    let finisher = filesystem.finisher();
    if read_only {
        // As `serve` does for a read-only mount.
        let reader = device::Disk::open(Path::new(path), mode)?;
        if !reader.seeks() {
            names.read_ahead(reader)?;
        }
    }
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::FSName("carryfs".into()),
    ];
    if read_only {
        config.mount_options.push(fuser::MountOption::RO);
    }
    let session = fuser::Session::new(filesystem, point, &config)?;
    names.set_notifier(session.notifier())?;
    let background = session.spawn()?;
    println!("ready");
    std::io::stdout().flush()?;
    for line in std::io::stdin().lock().lines() {
        if line? == "quit" {
            break;
        }
    }
    finisher.checkpoint();
    background.umount_and_join()?;
    Ok(())
}
