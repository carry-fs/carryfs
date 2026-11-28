//! The service's control pipe, `\\.\pipe\CarryFS`, through which CarryFS
//! Manager lists the mounts and mounts or unmounts one partition.
//!
//! A client sends one request line and reads the reply until the service
//! closes the pipe. The reply's first line is `ok` or `error MESSAGE`; a
//! `list` reply then has `mounted\tVOLUME\tPOINT` and `mounting\tVOLUME` lines.
//! Interactive users may connect, as they may Safely Remove a disk; remote
//! clients may not.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::FromRawHandle;

use windows_sys::Win32::Foundation::{
    ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::{Result, wide};

pub const PIPE: &str = r"\\.\pipe\CarryFS";

/// SYSTEM and administrators get full access; interactive users read and write.
const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

/// Requests are short; a longer line is refused rather than buffered.
const MAX_REQUEST: u64 = 512;

/// Serve requests on a thread each, since mounting can take minutes, until
/// the process exits. Fails when the pipe cannot be created, for example
/// because another process already owns its name.
pub fn serve(handle: fn(&str) -> String) -> Result<()> {
    let sddl = wide(SDDL);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the string is NUL-terminated and the descriptor is an out parameter.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(format!(
            "control pipe security: {}",
            std::io::Error::last_os_error()
        ));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name = wide(PIPE);
    // The first instance claims the name, so a process that created the pipe
    // earlier cannot pose as the service.
    let mut first = FILE_FLAG_FIRST_PIPE_INSTANCE;
    let result = loop {
        // SAFETY: the name is NUL-terminated and the attributes, with their
        // descriptor, outlive the call.
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | first,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                &attributes,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            break Err(format!(
                "create control pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        first = 0;
        // SAFETY: the pipe handle is open; a client that connected before
        // this call reports ERROR_PIPE_CONNECTED, which is a connection too.
        let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        // SAFETY: the handle is open and owned by nothing else; the File closes it.
        let pipe = unsafe { File::from_raw_handle(pipe) };
        if connected {
            std::thread::spawn(move || {
                if let Err(error) = answer(pipe, handle) {
                    eprintln!("carryfs: control pipe: {error}");
                }
            });
        }
    };
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(descriptor) };
    result
}

/// Read one request line and write the reply; closing the pipe ends it.
fn answer(pipe: File, handle: fn(&str) -> String) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new((&pipe).take(MAX_REQUEST)).read_line(&mut line)?;
    let reply = if line.ends_with('\n') {
        handle(line.trim_end())
    } else {
        "error request too long or incomplete\n".into()
    };
    (&pipe).write_all(reply.as_bytes())?;
    // FlushFileBuffers: wait until the client has read the reply, which
    // closing the pipe would otherwise discard.
    pipe.sync_all()
}

/// The partition a request names, when it is `\\.\HarddiskNPartitionM`.
/// Nothing else is accepted, so a client cannot make the service open a file.
pub fn partition(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(r"\\.\Harddisk")?;
    let (disk, partition) = rest.split_once("Partition")?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
    (digits(disk) && digits(partition) && partition != "0").then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_partition_paths_are_accepted() {
        assert!(partition(r"\\.\Harddisk1Partition2").is_some());
        assert!(partition(r"\\.\Harddisk12Partition10").is_some());
        assert!(partition(r"\\.\Harddisk1Partition0").is_none());
        assert!(partition(r"\\.\HarddiskPartition2").is_none());
        assert!(partition(r"\\.\Harddisk1Partition").is_none());
        assert!(partition(r"\\.\Harddisk1Partition2\x").is_none());
        assert!(partition(r"C:\image.cfs").is_none());
        assert!(partition(r"\\.\PhysicalDrive1").is_none());
        assert!(partition(r"\\.\Harddisk-1Partition2").is_none());
    }
}
