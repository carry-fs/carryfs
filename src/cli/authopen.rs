//! Read access to a macOS device node that denies the user.
//!
//! macOS recreates `/dev/diskN` as `root:operator 0640` on every attach.
//! `/usr/libexec/authopen` asks for administrator approval and passes back an
//! open descriptor, so the rest of the CLI runs unprivileged.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};

use carryfs_core::{Error, Result};

// Apple's xnu bsd/sys/socket.h. Control data follows the 12-byte header at
// 4-byte alignment, so one descriptor fills `Rights` exactly.
const SOL_SOCKET: i32 = 0xffff;
const SCM_RIGHTS: i32 = 1;

#[repr(C)]
struct IoVec {
    base: *mut u8,
    len: usize,
}

#[repr(C)]
struct MsgHdr {
    name: *mut u8,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: i32,
    control: *mut Rights,
    control_len: u32,
    flags: i32,
}

#[repr(C)]
struct Rights {
    len: u32,
    level: i32,
    kind: i32,
    fd: i32,
}

unsafe extern "C" {
    fn recvmsg(socket: i32, message: *mut MsgHdr, flags: i32) -> isize;
}

fn io_error(operation: &'static str, source: io::Error) -> Error {
    Error::Io { operation, source }
}

/// Open `path` read-only through `authopen`, which may show an approval dialog.
pub(super) fn open_read_only(path: &Path) -> Result<File> {
    let (ours, theirs) = UnixStream::pair().map_err(|e| io_error("authopen socket", e))?;
    // O_RDONLY is 0.
    let mut child = Command::new("/usr/libexec/authopen")
        .args(["-stdoutpipe", "-o", "0"])
        .arg(path)
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .map_err(|e| io_error("run authopen", e))?;

    let mut byte = 0_u8;
    let mut iov = IoVec {
        base: &mut byte,
        len: 1,
    };
    let mut rights = Rights {
        len: 0,
        level: 0,
        kind: 0,
        fd: -1,
    };
    let mut message = MsgHdr {
        name: std::ptr::null_mut(),
        name_len: 0,
        iov: &mut iov,
        iov_len: 1,
        control: &mut rights,
        control_len: size_of::<Rights>() as u32,
        flags: 0,
    };
    let received = loop {
        // SAFETY: every pointer in `message` names a live local of the length
        // it declares, and recvmsg writes no further.
        let n = unsafe { recvmsg(ours.as_raw_fd(), &mut message, 0) };
        if n >= 0 {
            break Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            break Err(io_error("receive authopen descriptor", error));
        }
    };
    let status = child.wait().map_err(|e| io_error("wait for authopen", e))?;
    received?;

    let complete = message.control_len as usize == size_of::<Rights>()
        && rights.len as usize == size_of::<Rights>()
        && rights.level == SOL_SOCKET
        && rights.kind == SCM_RIGHTS
        && rights.fd >= 0;
    if !complete {
        // A refused or cancelled approval closes the socket with no descriptor.
        return Err(io_error(
            "authopen",
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("no descriptor returned ({status})"),
            ),
        ));
    }
    // SAFETY: SCM_RIGHTS installed a new descriptor that nothing else owns.
    Ok(unsafe { File::from_raw_fd(rights.fd) })
}
