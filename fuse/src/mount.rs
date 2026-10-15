//! `mount.carryfs` helper, `serve` daemon, and udev `probe`.
//!
//! udisks runs mount(8) as root, which runs `mount.carryfs DEVICE DIR -o OPTIONS`.
//! The helper starts `serve` as a transient systemd service and waits for its
//! readiness. That gives the daemon a journal and its own cgroup, outside
//! udisks. `serve` claims the device with a `fuseblk` mount, opens the volume
//! behind that claim, and answers FUSE requests until unmount.

use std::env;
use std::error::Error as StdError;
use std::ffi::{CString, OsString};
use std::fs::{File, OpenOptions, TryLockError};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use carryfs_core::{BlockDevice, MountMode, Volume, has_root_magic};
use fuser::{Config, Session, SessionACL};

use crate::device::Disk;
use crate::fs::{CarryFs, Owner};
use crate::options::{Options, parse_options};
use crate::signals;

type Result<T> = std::result::Result<T, Box<dyn StdError>>;

/// The owner reported when udisks passes no `uid` or `gid`.
const ROOT: Owner = Owner { uid: 0, gid: 0 };

/// `mount.carryfs DEVICE DIR [-s] [-n] [-v] [-f] [-o OPTIONS]`, as mount(8) calls it.
pub fn helper(args: &[OsString]) -> Result<()> {
    let mut positional = Vec::new();
    let mut options = OsString::new();
    let mut fake = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-o") => options = args.next().ok_or("-o requires options")?.clone(),
            Some("-s" | "-n" | "-v") => {}
            Some("-f") => fake = true,
            Some(flag) if flag.starts_with('-') => {
                return Err(format!("unsupported mount flag: {flag}").into());
            }
            _ => positional.push(arg),
        }
    }
    let [device, directory] = positional[..] else {
        return Err("usage: mount.carryfs DEVICE DIRECTORY [-o OPTIONS]".into());
    };
    parse_options(options.to_str().ok_or("mount options must be UTF-8")?, ROOT)?;
    if fake {
        return Ok(());
    }
    let unit = unit_name(Path::new(device))?;
    let status = Command::new("systemd-run")
        .arg("--quiet")
        .arg("--collect")
        .arg(format!("--unit={unit}"))
        .arg("--property=Type=notify")
        .arg("--property=NotifyAccess=main")
        // Writable open checks the complete structure, which scales with metadata size.
        .arg("--property=TimeoutStartSec=infinity")
        .arg(env::current_exe()?)
        .arg("serve")
        .args([device, directory, &options])
        .status()?;
    if !status.success() {
        return Err(format!("mount failed; see journalctl -u {unit}").into());
    }
    Ok(())
}

fn unit_name(device: &Path) -> Result<String> {
    let name = device
        .canonicalize()?
        .file_name()
        .ok_or("device path has no name")?
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    // The helper's PID keeps the name unique while a previous daemon for the
    // same device is still exiting after unmount.
    Ok(format!("carryfs-{name}-{}", std::process::id()))
}

/// Mount `device` at `directory` and serve it until unmount.
pub fn serve(device: &Path, directory: &Path, options: &str) -> Result<()> {
    let options = parse_options(options, ROOT)?;
    let mode = if options.read_only {
        MountMode::ReadOnly
    } else {
        MountMode::ReadWrite
    };
    // Before any thread starts, so that every thread inherits the mask.
    let signals = signals::block()?;
    // Serialize startup with an earlier daemon that is still exiting.
    let _lock = lock_device(device)?;
    let fuse = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")?;
    mount_fuseblk(device, directory, &fuse, &options)?;
    // Signal watchers can retain Disk until process exit. Keep this guard in
    // serve so the loop's queue limit is restored when the session finishes.
    let _loop_limit = match (|| -> Result<_> {
        if mode == MountMode::ReadWrite {
            Ok(crate::device::limit_loop_requests(&File::open(device)?)?)
        } else {
            Ok(None)
        }
    })() {
        Ok(limit) => limit,
        Err(error) => {
            detach(directory);
            return Err(error);
        }
    };
    // The writable open's check is CPU work, on a thread that has only started.
    crate::fs::run_at_full_speed();
    // The mount holds the exclusive device claim; open behind it.
    let volume = Disk::open(device, mode)
        .and_then(|mut disk| {
            if options.read_only {
                disk.enable_metadata_prefetch(device)?;
            }
            Volume::open(disk)
        })
        .and_then(|mut volume| {
            if options.read_only {
                volume.enable_readonly_metadata_cache()?;
            }
            // `sync` keeps the core's eager publication.
            if mode == MountMode::ReadWrite && !options.sync {
                volume.set_deferred_checkpoints(true)?;
            }
            Ok(volume)
        });
    let session = volume.map_err(Into::into).and_then(|volume| {
        let (mut fs, _forgotten) = CarryFs::new(volume, options.owner, options.strict);
        if options.read_only {
            fs.set_read_only_cache();
        }
        let names = fs.finisher();
        if options.read_only {
            // A seeking disk would make the first requests wait behind this.
            let reader = Disk::open(device, mode)?;
            if !reader.seeks() {
                names.read_ahead(reader)?;
            }
        }
        // The session then ends as after `umount`, once open files are
        // closed, and before_destroy publishes their remaining changes.
        let stopper = fs.finisher();
        let point = directory.to_path_buf();
        signals::watch(signals, move || {
            stopper.checkpoint();
            detach(&point);
        })?;
        let config = Config::default();
        let session = Session::from_fd(fs, OwnedFd::from(fuse), SessionACL::All, config)?;
        names.set_notifier(session.notifier())?;
        Ok(session)
    });
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            detach(directory);
            return Err(error);
        }
    };
    if let Err(error) = notify_ready() {
        drop(session);
        detach(directory);
        return Err(error);
    }
    let result = session.run();
    // Hold the daemon after unmount for native tests of removal and a
    // competing claim. Publication has already completed before the reply.
    #[cfg(feature = "fault-injection")]
    if let Some(path) = env::var_os("CARRYFS_PAUSE_AFTER_UNMOUNT")
        && Path::new(&path).exists()
    {
        eprintln!("carryfs: paused after unmount");
        while Path::new(&path).exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    result?;
    Ok(())
}

/// Wait until no other daemon serves `device`, and keep it that way while
/// the returned lock is open. This also serializes startup with older
/// daemons that publish after unmount. A lock file rather than a lock on
/// the device node, because udev uses those locks to hold off probing.
fn lock_device(device: &Path) -> Result<File> {
    let rdev = std::fs::metadata(device)?.rdev();
    std::fs::create_dir_all("/run/carryfs")?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(format!("/run/carryfs/{rdev:x}.lock"))?;
    // A minute without the earlier daemon exiting means it is stuck.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                return Err("an earlier CarryFS daemon still serves this device".into());
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

fn mount_fuseblk(device: &Path, directory: &Path, fuse: &File, options: &Options) -> Result<()> {
    let source = CString::new(device.as_os_str().as_bytes())?;
    let target = CString::new(directory.as_os_str().as_bytes())?;
    let data = CString::new(format!(
        "fd={},rootmode=40000,user_id=0,group_id=0,blksize=4096,allow_other,default_permissions",
        fuse.as_raw_fd()
    ))?;
    let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
    if options.read_only {
        flags |= libc::MS_RDONLY;
    }
    if options.noexec {
        flags |= libc::MS_NOEXEC;
    }
    // SAFETY: the strings are NUL-terminated and outlive the call; the kernel
    // copies them and does not retain the pointers.
    let status = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            c"fuseblk.carryfs".as_ptr(),
            flags,
            data.as_ptr().cast(),
        )
    };
    if status != 0 {
        return Err(format!("mount: {}", std::io::Error::last_os_error()).into());
    }
    Ok(())
}

/// Detach the mount, which ends the session once nothing uses it. Callers
/// report their own reason; a failure here is only logged.
fn detach(directory: &Path) {
    let Ok(target) = CString::new(directory.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: the string is NUL-terminated and outlives the call.
    if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } != 0 {
        eprintln!(
            "carryfs: detaching the mount: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn notify_ready() -> Result<()> {
    let Some(path) = env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let address = match path.as_bytes().strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name)?,
        None => SocketAddr::from_pathname(&path)?,
    };
    UnixDatagram::unbound()?.send_to_addr(b"READY=1", &address)?;
    Ok(())
}

/// Print udev properties for a CarryFS device, or nothing for anything else.
pub fn probe(device: &Path) -> Result<()> {
    // Devices without media are simply not CarryFS.
    if let Ok(disk) = Disk::open(device, MountMode::ReadOnly) {
        print!("{}", properties(disk));
    }
    Ok(())
}

/// udev properties for a device whose root A or B slot carries the root magic.
fn properties<D: BlockDevice>(mut disk: D) -> String {
    if !has_root_magic(&mut disk) {
        return String::new();
    }
    // A damaged volume still identifies as CarryFS, so mounting reports its error.
    let mut text = "ID_FS_USAGE=filesystem\nID_FS_TYPE=carryfs\n".to_string();
    if let Ok(info) = Volume::open(disk).and_then(|volume| volume.info()) {
        let uuid = format_uuid(&info.uuid);
        text.push_str(&format!("ID_FS_UUID={uuid}\nID_FS_UUID_ENC={uuid}\n"));
        // udev strips one pair of matching quotes and nothing else, so a
        // label that starts with a quote still imports intact.
        if let Some(label) = &info.label {
            let label = label.as_str();
            text.push_str(&format!(
                "ID_FS_LABEL=\"{}\"\nID_FS_LABEL_ENC=\"{}\"\n",
                safe_label(label),
                encoded_label(label)
            ));
        }
    }
    text
}

/// `ID_FS_LABEL` as udev's blkid builtin writes it with libblkid's
/// `blkid_safe_string`: whitespace trimmed, each run of it one `_`.
fn safe_label(label: &str) -> String {
    let mut safe = String::new();
    for c in label
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join("_")
        .chars()
    {
        if blkid_accepts(c) {
            safe.push(c);
        } else {
            safe.extend(std::iter::repeat_n('_', c.len_utf8()));
        }
    }
    safe
}

/// `ID_FS_LABEL_ENC` as libblkid's `blkid_encode_string` writes it, which
/// names the `/dev/disk/by-label` link: other bytes become `\xNN`.
fn encoded_label(label: &str) -> String {
    let mut encoded = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric()
            || "#+-.:=@_".contains(c)
            || (!c.is_ascii() && blkid_accepts(c))
        {
            encoded.push(c);
        } else {
            for byte in c.to_string().bytes() {
                encoded.push_str(&format!("\\x{byte:02x}"));
            }
        }
    }
    encoded
}

/// libblkid treats the noncharacters U+FDD0 to U+FDEF and U+xFFFF as
/// invalid UTF-8.
fn blkid_accepts(c: char) -> bool {
    let code = u32::from(c);
    !(0xfdd0..=0xfdef).contains(&code) && code & 0xffff != 0xffff
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_identifies_formatted_and_damaged_volumes_only() {
        use carryfs_core::ImageDevice;

        let path = env::temp_dir().join(format!("carryfs-probe-{}.img", std::process::id()));
        let open = |mode| ImageDevice::open(&path, mode).unwrap();

        ImageDevice::create_new(&path, 256 << 20).unwrap();
        assert_eq!(properties(open(MountMode::ReadOnly)), "");

        let volume = Volume::format(open(MountMode::ReadWrite)).unwrap();
        let uuid = format_uuid(&volume.info().unwrap().uuid);
        drop(volume);
        assert_eq!(
            properties(open(MountMode::ReadOnly)),
            format!(
                "ID_FS_USAGE=filesystem\nID_FS_TYPE=carryfs\nID_FS_UUID={uuid}\nID_FS_UUID_ENC={uuid}\n"
            )
        );

        // Break both root checksums but keep the magic: still CarryFS, no UUID.
        let blocks = std::fs::metadata(&path).unwrap().len() / 4096;
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        for block in [256, blocks - 257] {
            std::os::unix::fs::FileExt::write_all_at(&file, &[0xff], block * 4096 + 100).unwrap();
        }
        assert_eq!(
            properties(open(MountMode::ReadOnly)),
            "ID_FS_USAGE=filesystem\nID_FS_TYPE=carryfs\n"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn probe_reports_the_label() {
        use carryfs_core::{ImageDevice, VolumeLabel};

        let path = env::temp_dir().join(format!("carryfs-probe-label-{}.img", std::process::id()));
        let open = |mode| ImageDevice::open(&path, mode).unwrap();
        ImageDevice::create_new(&path, 256 << 20).unwrap();
        let label = VolumeLabel::parse("Test Drive").unwrap();
        drop(Volume::format_with_label(open(MountMode::ReadWrite), label).unwrap());
        let text = properties(open(MountMode::ReadOnly));
        assert!(
            text.ends_with("ID_FS_LABEL=\"Test_Drive\"\nID_FS_LABEL_ENC=\"Test\\x20Drive\"\n"),
            "{text}"
        );
        std::fs::remove_file(&path).unwrap();
    }

    /// Expected values come from libblkid 2.42.3's `blkid_safe_string` and
    /// `blkid_encode_string`.
    #[test]
    fn labels_match_libblkid() {
        for (label, safe, encoded) in [
            ("Test Drive", "Test_Drive", r"Test\x20Drive"),
            (
                "  two  spaces  ",
                "two_spaces",
                r"\x20\x20two\x20\x20spaces\x20\x20",
            ),
            (r"a\b", r"a\b", r"a\x5cb"),
            ("it's \"q\"", "it's_\"q\"", r"it\x27s\x20\x22q\x22"),
            ("Фото 2024", "Фото_2024", r"Фото\x202024"),
            ("ok#+-.:=@_", "ok#+-.:=@_", "ok#+-.:=@_"),
            ("\u{fdd0}x", "___x", r"\xef\xb7\x90x"),
            ("\u{10ffff}", "____", r"\xf4\x8f\xbf\xbf"),
            ("\u{fffe}", "\u{fffe}", "\u{fffe}"),
        ] {
            assert_eq!(safe_label(label), safe, "{label:?}");
            assert_eq!(encoded_label(label), encoded, "{label:?}");
        }
    }

    #[test]
    fn uuid_uses_rfc_byte_order() {
        let bytes = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0x4d, 0xef, 0x80, 0x01, 0x02, 0x03, 0x04, 0x05,
            0x06, 0x07,
        ];
        assert_eq!(format_uuid(&bytes), "12345678-9abc-4def-8001-020304050607");
    }
}
