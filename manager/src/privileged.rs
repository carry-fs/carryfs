//! One administrator authorization per session. Unlocking starts this same
//! executable as root through the standard macOS password dialog. The root
//! helper serves one connection from the user who unlocked it, and only:
//! opening a validated disk node (the descriptor is passed back), upgrading a
//! volume with the installed adapter, and installing a CarryFS release. It
//! exits when the manager closes the connection or quits.
#![cfg(target_os = "macos")]

use crate::platform::{Result, valid_id};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::Path,
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

pub const FLAG: &str = "--privileged-helper";
const ADAPTER: &str = "/Library/Filesystems/carryfs.fs/Contents/Resources/mount_carryfs";

static HELPER: Mutex<Option<UnixStream>> = Mutex::new(None);

pub fn unlocked() -> bool {
    HELPER.lock().is_ok_and(|helper| helper.is_some())
}

/// Stop the helper; the next disk operation asks for approval again.
pub fn lock() {
    if let Ok(mut helper) = HELPER.lock() {
        *helper = None;
    }
}

/// Ask for administrator approval once; later calls reuse the helper.
pub fn unlock() -> Result<()> {
    let mut helper = HELPER
        .lock()
        .map_err(|_| "The administrator helper is unavailable")?;
    if helper.is_some() {
        return Ok(());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("cfs-{}-{stamp:x}", std::process::id()));
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
    }
    let socket = directory.join("helper.sock");
    let result = UnixListener::bind(&socket)
        .map_err(Into::into)
        .and_then(|listener| start(&listener, &socket));
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_dir(&directory);
    *helper = Some(result?);
    Ok(())
}

fn start(listener: &UnixListener, socket: &Path) -> Result<UnixStream> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let uid = unsafe { libc::getuid() };
    let pid = std::process::id();
    let command = format!(
        "{} {FLAG} {} {uid} {pid} >/dev/null 2>&1 &",
        shell_quote(&exe),
        shell_quote(socket)
    );
    let script = format!(
        "do shell script \"{}\" with prompt \"CarryFS Manager needs your permission to work with disks until you quit it.\" with administrator privileges",
        command.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(if error.contains("-128") {
            "Unlock cancelled. Operations on disks need administrator permission.".into()
        } else {
            format!("Could not unlock: {}", error.trim()).into()
        });
    }
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if peer_uid(&stream)? == 0 {
                    stream.set_nonblocking(false)?;
                    return Ok(stream);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err("The administrator helper did not connect.".into());
        }
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn peer_uid(stream: &UnixStream) -> Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(uid)
}

fn peer_pid(stream: &UnixStream) -> Result<libc::pid_t> {
    let mut pid: libc::pid_t = 0;
    let mut size = size_of::<libc::pid_t>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    if size as usize != size_of::<libc::pid_t>() || pid <= 0 {
        return Err("The administrator connection has no peer process ID.".into());
    }
    Ok(pid)
}

/// Send one request; return the reply and any descriptor sent with it.
fn request(message: Value) -> Result<(Value, Option<OwnedFd>)> {
    let mut helper = HELPER
        .lock()
        .map_err(|_| "The administrator helper is unavailable")?;
    let stream = helper.as_mut().ok_or("Unlock to work with disks.")?;
    let result = (|| -> Result<(Value, Option<OwnedFd>)> {
        stream.write_all(format!("{message}\n").as_bytes())?;
        let (line, fd) = receive(stream)?;
        let reply: Value = serde_json::from_str(&line)?;
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(error.to_owned().into());
        }
        Ok((reply, fd))
    })();
    if result.is_err() && stream.peer_addr().is_err() {
        *helper = None;
    }
    result
}

/// Open `/dev/rDISK` as root and hand the descriptor to this process.
pub fn open_device(id: &str) -> Result<File> {
    unlock()?;
    let (_, fd) = request(json!({"op": "open", "device": id}))?;
    Ok(File::from(
        fd.ok_or("The administrator helper sent no device")?,
    ))
}

/// Let the volume on `id` store permissions with the installed adapter.
pub fn upgrade(id: &str) -> Result<String> {
    unlock()?;
    output(request(json!({"op": "upgrade", "device": id}))?.0)
}

/// Install a CarryFS release with the published installer.
pub fn install(version: &str) -> Result<String> {
    unlock()?;
    output(request(json!({"op": "install", "version": version}))?.0)
}

fn output(reply: Value) -> Result<String> {
    let text = reply["output"].as_str().unwrap_or("").trim().to_owned();
    if reply["success"].as_bool() == Some(true) {
        Ok(text)
    } else {
        Err(if text.is_empty() {
            "The operation failed".into()
        } else {
            text.into()
        })
    }
}

/// Run as root: `carryfs-manager --privileged-helper SOCKET UID PID`.
pub fn serve(args: &[String]) -> Result<()> {
    let [socket, uid, pid] = args else {
        return Err("usage: --privileged-helper SOCKET UID PID".into());
    };
    let uid: u32 = uid.parse()?;
    let pid: libc::pid_t = pid.parse()?;
    if unsafe { libc::geteuid() } != 0 {
        return Err("The helper must run as root".into());
    }
    let stream = UnixStream::connect(socket)?;
    if peer_uid(&stream)? != uid || peer_pid(&stream)? != pid {
        return Err("Refusing a connection to another process".into());
    }
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let reply = handle(&line);
        line.clear();
        match reply {
            Ok((reply, Some(file))) => send(&stream, &format!("{reply}\n"), Some(&file))?,
            Ok((reply, None)) => stream.write_all(format!("{reply}\n").as_bytes())?,
            Err(error) => {
                stream.write_all(format!("{}\n", json!({"error": error.to_string()})).as_bytes())?
            }
        }
    }
    Ok(())
}

fn handle(line: &str) -> Result<(Value, Option<File>)> {
    let request: Value = serde_json::from_str(line)?;
    let device = || -> Result<String> {
        let id = request["device"].as_str().ok_or("Missing device")?;
        if !valid_id(id) {
            return Err("Invalid device identifier".into());
        }
        Ok(id.to_owned())
    };
    match request["op"].as_str() {
        Some("open") => {
            let file = File::open(format!("/dev/r{}", device()?))?;
            Ok((json!({"ok": true}), Some(file)))
        }
        Some("upgrade") => {
            use std::os::unix::process::CommandExt;
            // The adapter dispatches on argv[0]; as mount_carryfs it only mounts.
            let output = Command::new(ADAPTER)
                .arg0("carryfs-fuse")
                .arg("upgrade")
                .arg(format!("/dev/{}", device()?))
                .output()?;
            Ok((reply(output), None))
        }
        Some("install") => {
            let version = request["version"].as_str().ok_or("Missing version")?;
            if !version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                || version.split('.').count() != 3
            {
                return Err("Invalid release version".into());
            }
            let output = Command::new("/bin/bash")
                .args(["-c", "set -eu; installer=$(mktemp -t carryfs-install); trap 'rm -f \"$installer\"' EXIT; /usr/bin/curl --fail --silent --show-error --location https://carryfs.com/install.sh -o \"$installer\"; /bin/bash \"$installer\" --version \"$1\" --yes", "install", version])
                .output()?;
            Ok((reply(output), None))
        }
        _ => Err("Unknown request".into()),
    }
}

fn reply(output: std::process::Output) -> Value {
    json!({
        "success": output.status.success(),
        "output": format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    })
}

/// Write `line`, passing `file`'s descriptor with it.
fn send(stream: &UnixStream, line: &str, file: Option<&File>) -> Result<()> {
    let mut iov = libc::iovec {
        iov_base: line.as_ptr() as *mut libc::c_void,
        iov_len: line.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if let Some(file) = file {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast(), file.as_raw_fd());
        }
    }
    // The reply is one short line, so one message carries it whole.
    if unsafe { libc::sendmsg(stream.as_raw_fd(), &message, 0) } != line.len() as isize {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Read one reply line and any descriptor that came with it. The helper sends
/// one reply per request, so nothing follows the newline.
fn receive(stream: &UnixStream) -> Result<(String, Option<OwnedFd>)> {
    let mut line = Vec::new();
    let mut descriptor = None;
    loop {
        let mut buffer = [0u8; 64 * 1024];
        let mut iov = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
        let mut control = vec![0u8; space];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if n == 0 {
            return Err("The administrator helper stopped".into());
        }
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            if !header.is_null()
                && (*header).cmsg_level == libc::SOL_SOCKET
                && (*header).cmsg_type == libc::SCM_RIGHTS
            {
                let fd: libc::c_int = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast());
                descriptor = Some(OwnedFd::from_raw_fd(fd));
            }
        }
        line.extend_from_slice(&buffer[..n as usize]);
        if line.ends_with(b"\n") {
            return Ok((String::from_utf8(line)?, descriptor));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek};

    #[test]
    fn a_descriptor_and_its_reply_cross_the_socket_together() {
        let (left, right) = UnixStream::pair().unwrap();
        let path = std::env::temp_dir().join(format!("carryfs-fd-{}", std::process::id()));
        fs::write(&path, b"device bytes").unwrap();
        let file = File::open(&path).unwrap();
        send(&left, "{\"ok\":true}\n", Some(&file)).unwrap();
        let (line, fd) = receive(&right).unwrap();
        assert_eq!(line, "{\"ok\":true}\n");
        let mut received = File::from(fd.unwrap());
        received.rewind().unwrap();
        let mut text = String::new();
        received.read_to_string(&mut text).unwrap();
        assert_eq!(text, "device bytes");
        // A reply larger than the socket buffer arrives in several reads.
        let long = format!("{}\n", json!({"output": "x".repeat(200_000)}));
        let mut writer = left.try_clone().unwrap();
        let sender = std::thread::spawn(move || writer.write_all(long.as_bytes()).unwrap());
        let (line, fd) = receive(&right).unwrap();
        sender.join().unwrap();
        assert!(fd.is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["output"]
                .as_str()
                .unwrap()
                .len(),
            200_000
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "Requires permission to bind a local socket"]
    fn connected_peers_expose_user_and_process_ids() {
        let socket = std::env::temp_dir().join(format!("cfs-peer-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&socket).unwrap();
        let stream = UnixStream::connect(&socket).unwrap();
        fs::remove_file(&socket).unwrap();

        assert_eq!(peer_uid(&stream).unwrap(), unsafe { libc::geteuid() });
        assert_eq!(peer_pid(&stream).unwrap(), unsafe { libc::getpid() });
        let (accepted, _) = listener.accept().unwrap();
        assert_eq!(peer_uid(&accepted).unwrap(), unsafe { libc::geteuid() });
    }
}
