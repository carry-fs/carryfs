//! FreeBSD mount(8) helper and foreground FUSE daemon.

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

/// Serve a GEOM provider until unmount, publishing before replying to DESTROY.
pub fn serve(device: &Path, directory: &Path, text: &str) -> Result<()> {
    let options = parse_options(text, default_owner())?;
    let signals = signals::block()?;
    let mode = if options.read_only {
        MountMode::ReadOnly
    } else {
        MountMode::ReadWrite
    };
    let mut volume = Volume::open(Disk::open(device, mode)?)?;
    if options.read_only {
        volume.enable_readonly_metadata_cache()?;
    } else if !options.sync {
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
    let session = Session::new(fs, directory, &config)?;
    let point = directory.to_path_buf();
    signals::watch(signals, move || {
        stopper.checkpoint();
        if let Err(error) = detach(&point) {
            eprintln!("carryfs: unmounting: {error}");
        }
    })?;
    writeln!(io::stdout(), "{READY}")?;
    io::stdout().flush()?;
    session.run()?;
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
