//! BSD mount(8) helper and foreground FUSE daemon.

use std::error::Error as StdError;
use std::ffi::{CString, OsString};
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use carryfs_core::{MountMode, Volume};
use fuser::{Config, MountOption, Session, SessionACL};

use crate::device::Disk;
use crate::fs::{CarryFs, Owner};
use crate::options::parse_options;
use crate::signals;

type Result<T> = std::result::Result<T, Box<dyn StdError>>;

const READY: &str = "ready";
/// The helper names the daemon's log here, so a daemon that exits cleanly
/// without writing to it can remove it.
const LOG: &str = "CARRYFS_LOG";

fn default_owner() -> Owner {
    // SAFETY: these functions take no pointers and cannot fail.
    unsafe {
        Owner {
            uid: libc::getuid(),
            gid: libc::getgid(),
        }
    }
}

/// Start the daemon, returning only after its mount and FUSE handshake succeed.
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
    let log_path = std::env::temp_dir().join(format!(
        "carryfs-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)?;
    let mut child = Command::new(std::env::current_exe()?)
        .env(LOG, &log_path)
        .arg0("carryfs-fuse")
        .arg("serve")
        .args([device, directory])
        .arg(options)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
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

/// Serve a BSD disk until unmount, publishing before replying to DESTROY.
pub fn serve(device: &Path, directory: &Path, text: &str) -> Result<()> {
    let options = parse_options(text, default_owner())?;
    let signals = signals::block()?;
    let mode = if options.read_only {
        MountMode::ReadOnly
    } else {
        MountMode::ReadWrite
    };
    let mut volume = Volume::open(Disk::open(device, mode)?)?;
    #[cfg(target_os = "netbsd")]
    let root_mode = volume.attributes(carryfs_core::ROOT)?.mode.bits();
    // libperfuse can omit FSYNCDIR after creation. NetBSD publishes every
    // mutation synchronously so directory fsync still guarantees durability.
    if options.read_only {
        volume.enable_readonly_metadata_cache()?;
    } else if !cfg!(target_os = "netbsd") && !options.sync {
        volume.set_deferred_checkpoints(true)?;
    }
    let (mut fs, _) = CarryFs::new(volume, options.owner, options.strict);
    if options.read_only {
        fs.set_read_only_cache();
    }
    let stopper = fs.finisher();
    let mut config = Config::default();
    config.acl = SessionACL::All;
    // FreeBSD has no nodev mount flag; CarryFS rejects device-node creation.
    // The daemon checks no permissions: with allow_other, FreeBSD checks them
    // only under default_permissions, as Linux does.
    config.mount_options = vec![
        MountOption::FSName(device.to_string_lossy().into_owned()),
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        if options.read_only {
            MountOption::RO
        } else {
            MountOption::RW
        },
    ];
    if options.noexec {
        config.mount_options.push(MountOption::NoExec);
    }
    #[cfg(target_os = "freebsd")]
    let session = Session::new(fs, directory, &config)?;
    #[cfg(target_os = "netbsd")]
    let (session, mut relay, channel) =
        netbsd_session(fs, device, directory, &options, root_mode, config)?;
    let point = directory.to_path_buf();
    signals::watch(signals, move || {
        stopper.checkpoint();
        if let Err(error) = detach(&point) {
            eprintln!("carryfs: unmounting: {error}");
        }
    })?;
    writeln!(io::stdout(), "{READY}")?;
    io::stdout().flush()?;
    let result = session.run();
    #[cfg(target_os = "netbsd")]
    {
        if result.is_err() {
            relay.kill()?;
        }
        let status = relay.wait()?;
        drop(channel);
        result?;
        if !status.success() {
            return Err(format!("perfused exited with {status}").into());
        }
    }
    #[cfg(target_os = "freebsd")]
    result?;
    // An empty log records nothing; one with warnings stays for the user. A
    // failed removal leaves an empty file behind, so it is not an error.
    if let Some(log) = std::env::var_os(LOG)
        && std::fs::metadata(&log).is_ok_and(|log| log.len() == 0)
    {
        let _ = std::fs::remove_file(log);
    }
    Ok(())
}

fn detach(directory: &Path) -> io::Result<()> {
    let point = CString::new(directory.as_os_str().as_bytes())?;
    // SAFETY: point is NUL-terminated and outlives the unmount call.
    if unsafe { libc::unmount(point.as_ptr(), libc::MNT_FORCE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// NetBSD transports FUSE packets over a libperfuse socket to perfused/PUFFS.
#[cfg(target_os = "netbsd")]
fn netbsd_session(
    fs: CarryFs<Disk>,
    device: &Path,
    directory: &Path,
    options: &crate::options::Options,
    root_mode: u16,
    config: Config,
) -> io::Result<(
    Session<CarryFs<Disk>>,
    std::process::Child,
    std::os::fd::OwnedFd,
)> {
    use std::os::fd::AsRawFd;

    #[link(name = "perfuse")]
    unsafe extern "C" {
        fn perfuse_mount(
            source: *const libc::c_char,
            target: *const libc::c_char,
            kind: *const libc::c_char,
            flags: libc::c_long,
            data: *const libc::c_void,
        ) -> libc::c_int;
    }
    let source = CString::new(device.as_os_str().as_bytes())?;
    let target = CString::new(directory.as_os_str().as_bytes())?;
    let (fd, mut relay, library) = netbsd_relay(device, options.owner, root_mode)?;
    let data = CString::new(format!("fd={},default_permissions", fd.as_raw_fd()))?;
    let mut flags = libc::MNT_NOSUID | libc::MNT_NODEV;
    if options.read_only {
        flags |= libc::MNT_RDONLY;
    }
    if options.noexec {
        flags |= libc::MNT_NOEXEC;
    }
    // SAFETY: all strings and the socket remain live throughout the call.
    if unsafe {
        perfuse_mount(
            source.as_ptr(),
            target.as_ptr(),
            c"carryfs".as_ptr(),
            flags as libc::c_long,
            data.as_ptr().cast(),
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        relay.kill()?;
        relay.wait()?;
        return Err(error);
    }
    // perfused exits as soon as it reads EOF. fuser closes its channel right
    // after replying to DESTROY, before perfused has answered the kernel's
    // UNMOUNT. This duplicate keeps the socket open until perfused has exited.
    let channel = fd.try_clone()?;
    let session = Session::from_fd(fs, fd, SessionACL::All, config);
    drop(library); // The relay has loaded the library before sending INIT.
    match session {
        Ok(session) => Ok((session, relay, channel)),
        Err(error) => {
            // Closing /dev/puffs on the relay's exit force-unmounts; a
            // concurrent unmount(2) would race it.
            relay.kill()?;
            relay.wait()?;
            Err(error)
        }
    }
}

// Embed the native shim so installs need no separate shared-library artifact.
#[cfg(target_os = "netbsd")]
struct RelayLibrary(std::path::PathBuf);

#[cfg(target_os = "netbsd")]
impl Drop for RelayLibrary {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("carryfs: removing relay library: {error}");
        }
    }
}

#[cfg(target_os = "netbsd")]
fn netbsd_relay(
    device: &Path,
    owner: Owner,
    root_mode: u16,
) -> io::Result<(std::os::fd::OwnedFd, std::process::Child, RelayLibrary)> {
    use std::fs::DirBuilder;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let library = loop {
        let path = std::env::temp_dir().join(format!(
            "carryfs-perfuse-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => break RelayLibrary(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let path = library.0.join("relay.so");
    std::fs::write(
        &path,
        include_bytes!(concat!(env!("OUT_DIR"), "/libcarryfs-perfuse.so")),
    )?;
    let mut pair = [-1; 2];
    // SAFETY: pair has room for two descriptors, which are owned on success.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair returned two newly owned descriptors.
    let (client, server) =
        unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    for fd in [&client, &server] {
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            let bytes: libc::c_int = 131072;
            // SAFETY: setsockopt reads a live int of the supplied size.
            if unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&bytes as *const libc::c_int).cast(),
                    size_of_val(&bytes) as libc::socklen_t,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
    }
    let credentials: libc::c_int = 1;
    // SAFETY: SOL_LOCAL is 0 on NetBSD; the option reads one live int.
    if unsafe {
        libc::setsockopt(
            server.as_raw_fd(),
            0,
            libc::LOCAL_CREDS,
            (&credentials as *const libc::c_int).cast(),
            size_of_val(&credentials) as libc::socklen_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let server_fd = server.as_raw_fd();
    let mut command = Command::new("/usr/sbin/perfused");
    command
        .args(["-i", &server_fd.to_string()])
        .env("LD_PRELOAD", &path)
        .env(
            "CARRYFS_PERFUSE_ROOT",
            format!("{}:{}:{root_mode:o}", owner.uid, owner.gid),
        )
        .env("CARRYFS_PERFUSE_SOURCE", device);
    // SAFETY: pre_exec only calls the async-signal-safe fcntl operation. The
    // server descriptor remains live until the spawn completes.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(server_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    Ok((client, child, library))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_arguments_before_opening_a_device() {
        for args in [
            vec![],
            vec!["-o"],
            vec!["-bad", "device", "mount"],
            vec!["device"],
            vec!["-o", "unknown", "device", "mount"],
        ] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            assert!(!helper(&args).unwrap_err().to_string().contains("os error"));
        }
    }
}
