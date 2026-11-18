//! CarryFS FUSE adapter. On Ubuntu: a FUSE daemon, udisks mount helper, udev
//! probe, and mkfs(8) helper. On macOS: a macFUSE daemon and its mount(8) helper.

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
))]
mod device;
#[cfg(target_os = "macos")]
mod eject_macos;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
))]
mod fs;
#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
mod mkfs;
#[cfg(target_os = "linux")]
mod mount;
#[cfg(any(target_os = "freebsd", target_os = "netbsd"))]
mod mount_bsd;
#[cfg(target_os = "macos")]
mod mount_macos;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
))]
mod options;
#[cfg(target_os = "macos")]
mod read_cache;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
))]
mod signals;

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use std::path::Path;

    let args: Vec<_> = std::env::args_os().collect();
    let helper = args
        .first()
        .is_some_and(|argv0| options::is_helper_name(argv0));
    let mkfs_helper = args
        .first()
        .is_some_and(|argv0| Path::new(argv0).file_name() == Some("mkfs.carryfs".as_ref()));
    let result = if helper {
        mount::helper(&args[1..])
    } else if mkfs_helper {
        mkfs::main(&args[1..])
    } else {
        match &args[1..] {
            [flag] if flag == "--version" => {
                println!("{}", carryfs_core::version_line("carryfs-fuse"));
                Ok(())
            }
            [command, device] if command == "probe" => mount::probe(Path::new(device)),
            [command, rest @ ..] if command == "mkfs" => mkfs::main(rest),
            [command, device] if command == "upgrade" => upgrade(Path::new(device)),
            [command, device, directory, options] if command == "serve" => options
                .to_str()
                .ok_or_else(|| "mount options must be UTF-8".into())
                .and_then(|options| {
                    mount::serve(Path::new(device), Path::new(directory), options)
                }),
            [command, rest @ ..] if command == "mount" => mount::helper(rest),
            _ => Err("usage: carryfs-fuse probe DEVICE | carryfs-fuse mkfs [-f] [-L LABEL] DEVICE | carryfs-fuse mkfs --wipe DEVICE | carryfs-fuse upgrade DEVICE | carryfs-fuse mount DEVICE DIRECTORY [-o OPTIONS] | carryfs-fuse serve DEVICE DIRECTORY OPTIONS".into()),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("carryfs: {error}");
            // mount(8) helpers report mount failure as 32.
            ExitCode::from(if helper { 32 } else { 1 })
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "netbsd"))]
fn main() -> ExitCode {
    #[cfg(any(target_os = "freebsd", target_os = "netbsd"))]
    use mount_bsd as platform;
    #[cfg(target_os = "macos")]
    use mount_macos as platform;
    use std::path::Path;

    let args: Vec<_> = std::env::args_os().collect();
    let helper = args
        .first()
        .is_some_and(|argv0| options::is_helper_name(argv0));
    #[cfg(any(target_os = "freebsd", target_os = "netbsd"))]
    if args
        .first()
        .is_some_and(|arg| Path::new(arg).file_name() == Some("newfs_carryfs".as_ref()))
    {
        return match mkfs::main(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("carryfs: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let result = if helper {
        platform::helper(&args[1..])
    } else {
        match &args[1..] {
            [flag] if flag == "--version" => {
                println!("{}", carryfs_core::version_line("carryfs-fuse"));
                Ok(())
            }
            #[cfg(any(target_os = "freebsd", target_os = "netbsd"))]
            [command, rest @ ..] if command == "mkfs" => mkfs::main(rest),
            [command, device, directory, options] if command == "serve" => options
                .to_str()
                .ok_or_else(|| "mount options must be UTF-8".into())
                .and_then(|options| {
                    platform::serve(Path::new(device), Path::new(directory), options)
                }),
            [command, rest @ ..] if command == "mount" => platform::helper(rest),
            [command, device] if command == "upgrade" => upgrade(Path::new(device)),
            _ => Err("usage: carryfs-fuse mount [-o OPTIONS] DEVICE DIRECTORY | carryfs-fuse upgrade DEVICE | carryfs-fuse serve DEVICE DIRECTORY OPTIONS".into()),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("carryfs: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Let a volume CarryFS 0.1.0 formatted on `path` take writes. CarryFS 0.1.0
/// cannot mount it afterwards.
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
))]
fn upgrade(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use carryfs_core::{MountMode, Volume};
    #[cfg(target_os = "macos")]
    let raw = mount_macos::raw_node(path)?;
    #[cfg(target_os = "macos")]
    let path = raw.as_path();
    // On Linux the kernel's exclusive claim fails while the device is
    // mounted. On macOS and the BSDs `Disk::open` takes the lock a mounted
    // daemon holds.
    #[cfg(target_os = "linux")]
    let _claim = {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_EXCL)
            .open(path)
        {
            Err(error) if error.kind() == std::io::ErrorKind::ResourceBusy => {
                return Err("the device is in use: unmount it first".into());
            }
            result => result?,
        }
    };
    let changed = Volume::upgrade(device::Disk::open(path, MountMode::ReadWrite)?)?;
    println!(
        "{}: {}",
        path.display(),
        if changed {
            "upgraded; CarryFS 0.1.0 can no longer mount it"
        } else {
            "already up to date"
        }
    );
    Ok(())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
fn main() -> ExitCode {
    eprintln!("carryfs-fuse supports only Linux, macOS, FreeBSD, and NetBSD");
    ExitCode::FAILURE
}
