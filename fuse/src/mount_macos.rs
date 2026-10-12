//! `mount_carryfs` helper and `serve` daemon for macFUSE.
//!
//! DiskArbitration runs mount(8), which runs
//! `/Library/Filesystems/carryfs.fs/Contents/Resources/mount_carryfs -o OPTIONS
//! DEVICE DIR`. The helper starts `serve` in a session of its own and returns
//! once `serve` reports the mount, so DiskArbitration finds the volume. `serve`
//! locks and opens the raw disk node, mounts through macFUSE's kernel
//! extension, and answers FUSE requests until unmount. When the helper started
//! it, its stderr goes to a new log file (`log_file`).
//!
//! `serve` runs `mount_macfuse` the way macFUSE's libfuse does
//! (`lib/mount_darwin.c`): the helper passes back the `/dev/macfuseN`
//! descriptor over a socket, calls mount(2), which returns only after the
//! daemon answers `FUSE_INIT`, and then sends its result as an `i32`, 0 for
//! success, over the same socket.

use std::error::Error as StdError;
use std::ffi::{CString, OsString};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use carryfs_core::{MountMode, Volume};
use fuser::{Config, Session, SessionACL};

use crate::device::Disk;
use crate::eject_macos::{approve_unmounts, report_lost_changes};
use crate::fs::{CarryFs, Owner, forward_forgotten};
use crate::options::parse_options;
use crate::signals;

type Result<T> = std::result::Result<T, Box<dyn StdError>>;

const READY: &str = "ready";
const MOUNT_MACFUSE: &str = "/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse";
/// Where a root helper, which is how DiskArbitration mounts, puts the log.
const ROOT_LOGS: &str = "/Library/Logs/CarryFS";

/// `mount_carryfs [-o OPTIONS]... DEVICE DIR`, as mount(8) calls it.
pub fn helper(args: &[OsString]) -> Result<()> {
    let mut positional = Vec::new();
    let mut options = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-o") => options.push(
                args.next()
                    .ok_or("-o requires options")?
                    .to_str()
                    .ok_or("mount options must be UTF-8")?,
            ),
            Some(flag) if flag.starts_with('-') => {
                return Err(format!("unsupported mount flag: {flag}").into());
            }
            _ => positional.push(arg),
        }
    }
    let [device, directory] = positional[..] else {
        return Err("usage: mount_carryfs [-o OPTIONS] DEVICE DIRECTORY".into());
    };
    let options = options.join(",");
    parse_options(&options, default_owner())?;
    let (log, log_path) = log_file()?;
    let mut child = Command::new(std::env::current_exe()?)
        // This binary runs as the helper when named mount_carryfs.
        .arg0("carryfs-fuse")
        .arg("serve")
        .args([device, directory])
        .arg(&options)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        // Outlive mount(8) and DiskArbitration's handling of it.
        .process_group(0)
        .spawn()?;
    let mut line = String::new();
    if let Some(stdout) = child.stdout.take() {
        BufReader::new(stdout).read_line(&mut line)?;
    }
    if line.trim_end() != READY {
        let status = child.wait()?;
        return Err(format!("mount failed ({status}); see {}", log_path.display()).into());
    }
    Ok(())
}

/// A new log for the daemon's stderr, and its path. The eject alert names this
/// path. Root's temporary folder is closed to the console user, so a root
/// helper logs to `ROOT_LOGS`, readable by everyone; any other caller keeps a
/// private log in its own temporary folder.
// Each helper invocation creates a log, even if mounting fails. The helper and
// daemon leave these files in place after exit.
fn log_file() -> Result<(std::fs::File, PathBuf)> {
    let (directory, mode) = if is_root() {
        std::fs::create_dir_all(ROOT_LOGS)?;
        // Set, not requested: the umask may be stricter than the reader needs.
        std::fs::set_permissions(ROOT_LOGS, std::fs::Permissions::from_mode(0o755))?;
        (PathBuf::from(ROOT_LOGS), 0o644)
    } else {
        (std::env::temp_dir(), 0o600)
    };
    let path = directory.join(format!(
        "carryfs-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&path)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok((file, path))
}

/// Mount `device` at `directory` and serve it until unmount.
pub fn serve(device: &Path, directory: &Path, options: &str) -> Result<()> {
    // Before any thread starts, so that every thread inherits the mask.
    let signals = signals::block()?;
    let options = parse_options(options, default_owner())?;
    let mode = if options.read_only {
        MountMode::ReadOnly
    } else {
        MountMode::ReadWrite
    };
    let mut volume = Volume::open(Disk::open(&raw_node(device)?, mode)?)?;
    if options.read_only {
        volume.enable_readonly_metadata_cache()?;
    }
    // `sync` keeps the core's eager publication.
    if mode == MountMode::ReadWrite && !options.sync {
        volume.set_deferred_checkpoints(true)?;
    }
    // The name carryfs.util reports before mounting, so Finder shows one name.
    let name = volume.info()?.name();
    // libfuse's fuse_opt_add_opt_escaped uses these escapes for kernel options.
    let escaped_name = name.replace('\\', "\\\\").replace(',', "\\,");
    let mut mount_options = vec![
        format!("fsname={}", device.display()),
        format!("volname={escaped_name}"),
        "local".into(),
        "nodev".into(),
        "nosuid".into(),
        // Not `noreadahead`, though the kernel reads about 128 MiB past a
        // reader and a file's first 128 MiB so read at 182 MiB/s against 357:
        // without its read-ahead the kernel gives up a file's pages first,
        // some while the file is still being read, and a second read then
        // comes from the device.
        if options.read_only { "ro" } else { "rw" }.into(),
    ];
    if options.noexec {
        mount_options.push("noexec".into());
    }
    // A root mount serves the console user. DiskArbitration mounted a disk
    // image as the user who attached it, which needs neither. The daemon
    // checks no permissions itself, as CarryFS stores none.
    let acl = if is_root() {
        mount_options.push("allow_other".into());
        SessionACL::All
    } else {
        SessionACL::Owner
    };
    let (socket, fuse) = start_mount(directory, &mount_options.join(","))?;
    let (mut fs, forgotten) = CarryFs::new(volume, options.owner, options.strict);
    if options.read_only {
        fs.set_read_only_cache();
    }
    let finisher = fs.finisher();
    approve_unmounts(device, name.clone(), fs.finisher())?;
    // The session then ends as after `umount`, and a failed publication is
    // reported below.
    let stopper = fs.finisher();
    let point = directory.to_path_buf();
    signals::watch(signals, move || {
        stopper.checkpoint();
        force_unmount(&point);
    })?;
    let session = Session::from_fd(fs, fuse, acl, Config::default())?;
    forward_forgotten(session.notifier(), forgotten)?;
    // mount(2) waits for this session's answer to FUSE_INIT, so the result
    // arrives while `run` serves.
    std::thread::Builder::new()
        .name("carryfs-mounted".into())
        .spawn(move || match mount_status(socket) {
            Ok(()) => {
                let mut stdout = io::stdout();
                if let Err(error) = writeln!(stdout, "{READY}").and_then(|()| stdout.flush()) {
                    eprintln!("carryfs: reporting the mount failed: {error}");
                }
            }
            Err(error) => {
                eprintln!("carryfs: {error}");
                // Nothing was mounted, so no request will end `run`.
                std::process::exit(1);
            }
        })?;
    // DiskArbitration's unmount approval publishes and reports a failure
    // (eject_macos.rs). Any unmount then forgets every cached vnode before
    // DESTROY, and `forget` publishes what is left (fs.rs). `run` returns
    // after `destroy`, so a failure no approval reported is final here: a
    // plain umount(8), a change made after approval, or a timer failure.
    let served = session.run();
    if let Some(failure) = finisher.take_failure() {
        report_lost_changes(&name, &failure);
    }
    served?;
    Ok(())
}

/// Unmount `directory` even with files open, whose handles then fail: macOS
/// has no lazy unmount. The daemon is about to stop anyway, which would fail
/// them too.
fn force_unmount(directory: &Path) {
    let Ok(target) = CString::new(directory.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: the string is NUL-terminated and outlives the call.
    if unsafe { libc::unmount(target.as_ptr(), libc::MNT_FORCE) } != 0 {
        eprintln!("carryfs: unmounting: {}", io::Error::last_os_error());
    }
}

/// Run `mount_macfuse` for `directory` and receive its FUSE descriptor. The
/// returned socket later carries the mount result.
fn start_mount(directory: &Path, options: &str) -> Result<(UnixStream, OwnedFd)> {
    let (ours, theirs) = UnixStream::pair()?;
    // The helper finds its end by number, so it must survive exec.
    // SAFETY: F_SETFD takes an integer argument on an open descriptor.
    if unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_SETFD, 0) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut helper = Command::new(MOUNT_MACFUSE)
        .args(["-o", options])
        .arg(directory)
        .env("_FUSE_CALL_BY_LIB", "1")
        .env("_FUSE_COMMFD", theirs.as_raw_fd().to_string())
        .env("_FUSE_COMMVERS", "2")
        .env("_FUSE_DAEMON_PATH", std::env::current_exe()?)
        .stdin(Stdio::null())
        .spawn()?;
    drop(theirs);
    let fuse = receive_fd(&ours);
    // The helper keeps running until the mount completes; reap it then.
    std::thread::Builder::new()
        .name("carryfs-mount-helper".into())
        .spawn(move || {
            if let Err(error) = helper.wait() {
                eprintln!("carryfs: waiting for mount_macfuse failed: {error}");
            }
        })?;
    Ok((ours, fuse?))
}

/// Receive one descriptor sent with `SCM_RIGHTS`.
fn receive_fd(socket: &UnixStream) -> Result<OwnedFd> {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&raw mut byte).cast(),
        iov_len: 1,
    };
    // Room for one descriptor's control message, suitably aligned.
    let mut control = [0_u64; 4];
    // SAFETY: an all-zero msghdr is valid; the fields used are set below.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control) as libc::socklen_t;
    loop {
        // SAFETY: every pointer in `message` names a live local of the length
        // it declares, and recvmsg writes no further.
        let received = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, 0) };
        if received > 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if received == 0 {
            return Err("mount_macfuse sent no descriptor".into());
        }
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    // SAFETY: `message` describes the control buffer recvmsg just filled.
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    // SAFETY: a non-null header points into `control`, and CMSG_LEN is pure.
    let complete = !header.is_null()
        && unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET
                && (*header).cmsg_type == libc::SCM_RIGHTS
                && (*header).cmsg_len == libc::CMSG_LEN(size_of::<libc::c_int>() as u32)
        };
    if !complete {
        return Err("mount_macfuse sent no descriptor".into());
    }
    // SAFETY: the SCM_RIGHTS message holds one descriptor, which the kernel
    // installed for this process and nothing else owns.
    Ok(unsafe {
        OwnedFd::from_raw_fd(std::ptr::read_unaligned(
            libc::CMSG_DATA(header).cast::<libc::c_int>(),
        ))
    })
}

/// Wait for `mount_macfuse`'s result: 0, or an errno.
fn mount_status(mut socket: UnixStream) -> Result<()> {
    let mut status = [0_u8; 4];
    socket
        .read_exact(&mut status)
        .map_err(|error| format!("mount_macfuse sent no result: {error}"))?;
    match i32::from_ne_bytes(status) {
        0 => Ok(()),
        errno => Err(format!("mount failed: {}", io::Error::from_raw_os_error(errno)).into()),
    }
}

/// `/dev/rdiskNsM` for `/dev/diskNsM`.
fn raw_node(device: &Path) -> Result<PathBuf> {
    let name = device
        .to_str()
        .and_then(|path| path.strip_prefix("/dev/disk"))
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        .ok_or("device must be /dev/diskN or /dev/diskNsM")?;
    Ok(PathBuf::from(format!("/dev/rdisk{name}")))
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The console user under a root mount, else the caller, so files show as the
/// user's own when DiskArbitration passes no `uid` or `gid`.
fn default_owner() -> Owner {
    if is_root()
        && let Ok(console) = std::fs::metadata("/dev/console")
    {
        return Owner {
            uid: console.uid(),
            gid: console.gid(),
        };
    }
    // SAFETY: getuid and getgid have no preconditions and cannot fail.
    unsafe {
        Owner {
            uid: libc::getuid(),
            gid: libc::getgid(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_node_maps_disk_nodes_only() {
        assert_eq!(
            raw_node(Path::new("/dev/disk4s2")).unwrap(),
            PathBuf::from("/dev/rdisk4s2")
        );
        for bad in ["/dev/rdisk4s2", "/dev/disk", "disk4s2", "/dev/disk4/../x"] {
            assert!(raw_node(Path::new(bad)).is_err(), "{bad}");
        }
    }
}
