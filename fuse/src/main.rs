//! CarryFS FUSE adapter. On Ubuntu: a FUSE daemon, udisks mount helper, udev
//! probe, and mkfs(8) helper. On macOS: a macFUSE daemon and its mount(8) helper.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod device;
#[cfg(target_os = "macos")]
mod eject_macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod fs;
#[cfg(target_os = "linux")]
mod mkfs;
#[cfg(target_os = "linux")]
mod mount;
#[cfg(target_os = "macos")]
mod mount_macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod options;
#[cfg(target_os = "macos")]
mod read_cache;
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
            [command, device] if command == "probe" => mount::probe(Path::new(device)),
            [command, rest @ ..] if command == "mkfs" => mkfs::main(rest),
            [command, device, directory, options] if command == "serve" => options
                .to_str()
                .ok_or_else(|| "mount options must be UTF-8".into())
                .and_then(|options| {
                    mount::serve(Path::new(device), Path::new(directory), options)
                }),
            [command, rest @ ..] if command == "mount" => mount::helper(rest),
            _ => Err("usage: carryfs-fuse probe DEVICE | carryfs-fuse mkfs [-L LABEL | --wipe] DEVICE | carryfs-fuse mount DEVICE DIRECTORY [-o OPTIONS] | carryfs-fuse serve DEVICE DIRECTORY OPTIONS".into()),
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

#[cfg(target_os = "macos")]
fn main() -> ExitCode {
    use std::path::Path;

    let args: Vec<_> = std::env::args_os().collect();
    let helper = args
        .first()
        .is_some_and(|argv0| options::is_helper_name(argv0));
    let result = if helper {
        mount_macos::helper(&args[1..])
    } else {
        match &args[1..] {
            [command, device, directory, options] if command == "serve" => options
                .to_str()
                .ok_or_else(|| "mount options must be UTF-8".into())
                .and_then(|options| {
                    mount_macos::serve(Path::new(device), Path::new(directory), options)
                }),
            [command, rest @ ..] if command == "mount" => mount_macos::helper(rest),
            _ => Err("usage: carryfs-fuse mount [-o OPTIONS] DEVICE DIRECTORY | carryfs-fuse serve DEVICE DIRECTORY OPTIONS".into()),
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> ExitCode {
    eprintln!("carryfs-fuse supports only Linux and macOS");
    ExitCode::FAILURE
}
