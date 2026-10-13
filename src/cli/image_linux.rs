//! Keep CLI image access stable even when a loop device bypasses image flock.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use carryfs_core::{ImageDevice, MountMode};

use super::Result;

unsafe extern "C" {
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
}

pub(super) fn open(path: &Path, mode: MountMode) -> Result<ImageDevice> {
    // Linux asm-generic constants, as used by the raw-device open in cli.rs.
    const O_NONBLOCK: c_int = 0o4000;
    const F_SETSIG: c_int = 10;
    const F_SETLEASE: c_int = 1024;
    const F_RDLCK: c_int = 0;
    const F_WRLCK: c_int = 1;
    const SIGKILL: c_int = 9;

    let file = OpenOptions::new()
        .read(true)
        .write(mode == MountMode::ReadWrite)
        .custom_flags(O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err("image is not a regular file".into());
    }
    // A conflicting open breaks a lease, eventually even if we are stopped.
    // SIGKILL cannot be blocked or ignored: this command ends before the other
    // opener proceeds, rather than resuming without ownership of the image.
    // SAFETY: fcntl receives an open fd and integer arguments for these commands.
    if unsafe { fcntl(file.as_raw_fd(), F_SETSIG, SIGKILL) } == -1 {
        return Err(format!(
            "set image lease-break signal: {}",
            io::Error::last_os_error()
        )
        .into());
    }
    let lease = match mode {
        MountMode::ReadOnly => F_RDLCK,
        MountMode::ReadWrite => F_WRLCK,
    };
    // SAFETY: the fd allows the chosen access and F_SETLEASE takes an integer lock type.
    if unsafe { fcntl(file.as_raw_fd(), F_SETLEASE, lease) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(
                "the image is in use: close conflicting programs or detach its loop devices first"
                    .into(),
            );
        }
        return Err(format!("hold image stable: {error}").into());
    }
    Ok(ImageDevice::from_image_file(file, mode)?)
}
