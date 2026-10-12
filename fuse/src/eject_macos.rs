//! Publish before DiskArbitration unmounts, and tell the user when that fails.
//!
//! macFUSE's unmount sends no request whose answer can report an error:
//! `umount` and `diskutil unmount` deliver only FORGETs and then DESTROY,
//! which fuser answers before calling `destroy`. Finder's Eject and `diskutil`
//! first ask DiskArbitration's unmount approvers, so the daemon registers as
//! one, publishes, and waits for the device before the unmount starts.
//!
//! The approver never refuses. A failed publication faults the volume, so no
//! later attempt can save those changes, and refusing would only strand the
//! disk: Finder reports a refusal as the disk being in use by the app that
//! started the daemon, offers no Force Eject, and never shows the reason. The
//! daemon approves and puts up an alert that says what was lost and how to
//! recover. A plain umount(8) or a forced unmount asks no approver; for those
//! `forget` still publishes, and `serve` reports a failure nobody has reported
//! once the session ends (`report_lost_changes`).

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;

use carryfs_core::{BlockDevice, Error};

use crate::fs::Finisher;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFRunLoopRef = *const c_void;
type DASessionRef = *const c_void;
type DADiskRef = *const c_void;
type DADissenterRef = *const c_void;
type Approval = extern "C" fn(DADiskRef, *mut c_void) -> DADissenterRef;

const UTF8: u32 = 0x0800_0100;
/// `kCFUserNotificationStopAlertLevel`.
const STOP_ALERT: usize = 0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();
    fn CFStringCreateWithBytes(
        allocator: CFTypeRef,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> CFStringRef;
    fn CFRelease(object: CFTypeRef);
    fn CFUserNotificationDisplayNotice(
        timeout: f64,
        flags: usize,
        icon: CFTypeRef,
        sound: CFTypeRef,
        localization: CFTypeRef,
        header: CFStringRef,
        message: CFStringRef,
        button: CFStringRef,
    ) -> i32;
}

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    fn DASessionCreate(allocator: CFTypeRef) -> DASessionRef;
    fn DASessionScheduleWithRunLoop(
        session: DASessionRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn DARegisterDiskUnmountApprovalCallback(
        session: DASessionRef,
        matching: CFTypeRef,
        callback: Approval,
        context: *mut c_void,
    );
    fn DADiskGetBSDName(disk: DADiskRef) -> *const c_char;
}

/// Why changes could not be published: the error for the log, and a sentence
/// for the alert.
pub struct Failure {
    detail: String,
    plain: &'static str,
}

impl From<&Error> for Failure {
    fn from(error: &Error) -> Self {
        let plain = match error {
            Error::Io {
                operation: "flush_durable",
                ..
            } => {
                "The disk did not confirm that it saved the data. It may have been \
                 unplugged, or it may be failing."
            }
            Error::Io { .. } => {
                "The disk stopped accepting data. It may have been unplugged, or it \
                 may be failing."
            }
            Error::NoSpace => "The disk is full.",
            Error::Corrupt(_) | Error::Corruption { .. } => {
                "CarryFS found damaged data on the disk."
            }
            Error::Faulted => "An earlier error stopped CarryFS from saving.",
            _ => "An unexpected error stopped CarryFS from saving.",
        };
        Self {
            detail: error.to_string(),
            plain,
        }
    }
}

struct Approver<D: BlockDevice> {
    /// The BSD name DiskArbitration gives the device, such as `disk4s2`.
    disk: CString,
    /// The volume name Finder shows.
    volume: String,
    /// The daemon's log, which has the technical error.
    log: Option<String>,
    finisher: Finisher<D>,
    /// Whether the alert went up. The volume stays faulted, so an unmount that
    /// something else blocks would otherwise add an alert on every retry.
    noticed: AtomicBool,
}

/// Publish before every unmount of `device` that DiskArbitration handles, from
/// a thread that serves it until the process exits.
pub fn approve_unmounts<D: BlockDevice + Send + 'static>(
    device: &Path,
    volume: String,
    finisher: Finisher<D>,
) -> std::io::Result<()> {
    let name = device
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::other("device path has no name"))?;
    // DiskArbitration names the block node, not the raw one.
    let disk =
        CString::new(name.strip_prefix('r').unwrap_or(name)).map_err(std::io::Error::other)?;
    let (registered, result) = sync_channel(1);
    std::thread::Builder::new()
        .name("carryfs-eject".into())
        .spawn(move || {
            // The approver lives as long as the thread, which is the process.
            let approver = Box::leak(Box::new(Approver {
                disk,
                volume,
                log: log_path(),
                finisher,
                noticed: AtomicBool::new(false),
            }));
            // SAFETY: a null allocator selects the default one.
            let session = unsafe { DASessionCreate(ptr::null()) };
            if session.is_null() {
                let _ = registered.send(Err(std::io::Error::other(
                    "cannot open a DiskArbitration session",
                )));
                return;
            }
            // SAFETY: the session is live and never released, the run loop is
            // this thread's own, and the context outlives every callback.
            unsafe {
                DASessionScheduleWithRunLoop(session, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
                DARegisterDiskUnmountApprovalCallback(
                    session,
                    ptr::null(),
                    approve::<D>,
                    ptr::from_mut(approver).cast(),
                );
            }
            let _ = registered.send(Ok(()));
            // SAFETY: runs this thread's run loop, which has the session.
            unsafe { CFRunLoopRun() };
        })?;
    result
        .recv()
        .map_err(|_| std::io::Error::other("the DiskArbitration thread ended"))?
}

extern "C" fn approve<D: BlockDevice>(disk: DADiskRef, context: *mut c_void) -> DADissenterRef {
    // SAFETY: `approve_unmounts` registered a leaked `Approver<D>` as context.
    let approver = unsafe { &*context.cast::<Approver<D>>() };
    // SAFETY: DiskArbitration passes a live disk; its name, when present, is a
    // C string it owns for the duration of the callback.
    let ours = unsafe {
        let name = DADiskGetBSDName(disk);
        !name.is_null() && CStr::from_ptr(name) == approver.disk.as_c_str()
    };
    if !ours {
        return ptr::null();
    }
    if let Err(failure) = approver.finisher.sync() {
        eprintln!(
            "carryfs: unmounting after a failed publication: {}",
            failure.detail
        );
        if !approver.noticed.swap(true, Ordering::Relaxed) {
            notice(&approver.volume, approver.log.as_deref(), failure.plain);
        }
    }
    ptr::null()
}

/// Report a failure no unmount approval reported, after the session ended.
pub fn report_lost_changes(volume: &str, failure: &Failure) {
    eprintln!(
        "carryfs: unmounted after a failed publication: {}",
        failure.detail
    );
    notice(volume, log_path().as_deref(), failure.plain);
}

/// Tell the user that the changes to `volume` since its last successful save
/// were lost, and how to check and recover the disk.
fn notice(volume: &str, log: Option<&str>, cause: &str) {
    // Test builds only: fault-injection runs fail on purpose and would
    // leave an alert per daemon on the desktop.
    #[cfg(feature = "fault-injection")]
    if std::env::var_os("CARRYFS_NO_NOTICE").is_some() {
        return;
    }
    let mut message = format!(
        "{cause}\n\nChanges made to “{volume}” since its last successful save are \
         not on the disk. Before relying on the disk, check it with \
         “carryfs check” and “carryfs scrub”, and copy files off with \
         “carryfs get”."
    );
    if let Some(log) = log {
        message.push_str(&format!("\n\nDetails are in {log}"));
    }
    show_notice("CarryFS could not save recent changes", &message);
}

/// Put up an alert with an OK button for the console user. It returns without
/// waiting for the alert to be dismissed, so DiskArbitration's callback is not
/// held.
fn show_notice(header: &str, message: &str) {
    let strings = [cf_string(header), cf_string(message), cf_string("OK")];
    // SAFETY: every string is a live CFString or null, which the function
    // accepts; null URLs select the defaults.
    let status = unsafe {
        CFUserNotificationDisplayNotice(
            0.0,
            STOP_ALERT,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            strings[0],
            strings[1],
            strings[2],
        )
    };
    if status != 0 {
        eprintln!("carryfs: showing the eject notice failed: {status}");
    }
    for string in strings {
        release(string);
    }
}

/// The file stderr writes to, when the mount helper pointed it at one; not a
/// terminal or a pipe.
fn log_path() -> Option<String> {
    let mut path = [0 as c_char; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX bytes.
    if unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETPATH, path.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so the buffer holds a NUL-terminated string.
    let path = unsafe { CStr::from_ptr(path.as_ptr()) }.to_str().ok()?;
    (path.starts_with('/') && !path.starts_with("/dev/")).then(|| path.to_owned())
}

/// A new CFString, or null if CoreFoundation cannot make one.
fn cf_string(text: &str) -> CFStringRef {
    let Ok(length) = isize::try_from(text.len()) else {
        return ptr::null();
    };
    // SAFETY: the bytes are valid UTF-8 of the given length, copied by the
    // call; a null allocator selects the default one.
    unsafe { CFStringCreateWithBytes(ptr::null(), text.as_ptr(), length, UTF8, 0) }
}

fn release(object: CFTypeRef) {
    if !object.is_null() {
        // SAFETY: `object` is a CF object this module created and still owns.
        unsafe { CFRelease(object) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(operation: &'static str) -> Error {
        Error::Io {
            operation,
            source: std::io::Error::from_raw_os_error(libc::EIO),
        }
    }

    #[test]
    fn failures_read_as_plain_sentences_and_keep_the_detail() {
        let flush = Failure::from(&io("flush_durable"));
        assert!(flush.plain.starts_with("The disk did not confirm"));
        assert!(flush.detail.starts_with("flush_durable: "));
        let write = Failure::from(&io("write_all_at"));
        assert!(write.plain.starts_with("The disk stopped accepting data"));
        assert_eq!(Failure::from(&Error::NoSpace).plain, "The disk is full.");
        assert!(
            Failure::from(&Error::Faulted)
                .plain
                .starts_with("An earlier error")
        );
    }
}
