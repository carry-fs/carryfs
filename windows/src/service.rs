//! The `CarryFS` Windows service: mount inserted CarryFS volumes on a free
//! drive letter and eject them when Windows asks to remove the device.
//!
//! CarryFS partitions are hidden with GPT attributes 62 and 63, so Windows
//! gives them no drive letter, volume name, or format prompt. The service
//! finds them through disk arrivals from `CM_Register_Notification` and probes
//! each partition. Each mount also registers for its partition handle, so "Safely Remove Hardware" delivers a
//! query-remove. The service closes the handle before allowing removal, and
//! vetoes it while changes await publication, unless publishing has failed:
//! a faulted volume is released and the users are told what was lost. A
//! surprise removal can lose changes since the last completed publication.
//! Operations and device I/O can delay the one-second checkpoint trigger.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::windows::io::IntoRawHandle;
use std::sync::{Condvar, Mutex, OnceLock};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE,
    CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED, CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE,
    CM_NOTIFY_ACTION_DEVICEREMOVEPENDING, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
    CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
    CM_Register_Notification, CM_Unregister_Notification, CR_SUCCESS, HCMNOTIFICATION,
};
use windows_sys::Win32::Foundation::{ERROR_CANCELLED, HANDLE, NO_ERROR};
use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_DISK;
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_SESSION_INFOW, WTSActive, WTSDisconnected, WTSEnumerateSessionsW, WTSFreeMemory,
    WTSSendMessageW,
};
#[cfg(feature = "fault-injection")]
use windows_sys::Win32::System::Services::SERVICE_CONTROL_DEVICEEVENT;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
#[cfg(feature = "fault-injection")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BROADCAST_QUERY_DENY, DBT_DEVICEQUERYREMOVE, DBT_DEVICEQUERYREMOVEFAILED,
    DBT_DEVICEREMOVECOMPLETE, DBT_DEVICEREMOVEPENDING, DBT_DEVTYP_HANDLE, DEV_BROADCAST_HANDLE,
    DEV_BROADCAST_HDR, DEVICE_NOTIFY_SERVICE_HANDLE, RegisterDeviceNotificationW,
    UnregisterDeviceNotification,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK};

use crate::fs::Removal;
use crate::{Result, device, ffi, wide};

const NAME: &str = "CarryFS";

/// A mounted volume and its device-handle notification.
struct Mounted {
    mount: ffi::Mount,
    notification: Notification,
    release: device::Release,
}

struct Notification {
    cm: HCMNOTIFICATION,
    /// An `HDEVNOTIFY` registered through the service control handler, another
    /// route for the removal query. Test builds only.
    #[cfg(feature = "fault-injection")]
    device: isize,
}

// SAFETY: a notification handle is an opaque token usable from any thread.
unsafe impl Send for Notification {}

// All mounts run in this service process, so a process crash unmounts every disk.
static MOUNTS: Mutex<Option<HashMap<String, Mounted>>> = Mutex::new(None);
static STOP: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static STATUS: OnceLock<usize> = OnceLock::new();
/// Volumes being opened, which can take minutes, with whether the disk went
/// away meanwhile. Opening happens outside `MOUNTS` so one large volume does
/// not hold up other disks, removal queries, or a service stop. Lock order is
/// `MOUNTS`, then this.
static MOUNTING: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
/// Volumes whose user has been told that changes were lost, so a retried
/// removal does not add a second message. Cleared when the volume mounts.
static NOTIFIED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// Unmounts still finishing after a query-remove released their disks.
static STOPPING: Mutex<Vec<(String, std::thread::JoinHandle<()>)>> = Mutex::new(Vec::new());

pub fn run() -> Result<()> {
    let name = wide(NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_ptr().cast_mut(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: the table is null-terminated and outlives the dispatcher, which
    // returns only after the service stops.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(format!(
            "not started by the service manager: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    log_to_file();
    let name = wide(NAME);
    // SAFETY: the name is NUL-terminated and the handler is a static function.
    let status =
        unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), std::ptr::null()) };
    if status.is_null() {
        eprintln!(
            "carryfs: RegisterServiceCtrlHandlerExW: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let _ = STATUS.set(status as usize);
    *MOUNTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(HashMap::new());
    report(SERVICE_RUNNING);

    let filter = interface_filter();
    let mut arrivals: HCMNOTIFICATION = std::ptr::null_mut();
    // SAFETY: the filter is initialized for interface notifications and the
    // callback is a static function.
    let result = unsafe {
        CM_Register_Notification(&filter, std::ptr::null(), Some(on_arrival), &mut arrivals)
    };
    if result != CR_SUCCESS {
        eprintln!("carryfs: CM_Register_Notification for disks failed: CONFIGRET {result}");
    }
    // Disks present at startup raise no arrival notification.
    // Probing only disk numbers 0-63 misses higher numbers at startup.
    for number in 0..64 {
        attach_disk(&format!("\\\\.\\PhysicalDrive{number}"));
    }

    let (lock, wake) = &STOP;
    let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
    while !*stopped {
        stopped = wake.wait(stopped).unwrap_or_else(|e| e.into_inner());
    }
    drop(stopped);

    if !arrivals.is_null() {
        // SAFETY: registered above and not inside its own callback.
        unsafe { CM_Unregister_Notification(arrivals) };
    }
    let mounted: Vec<String> = MOUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|mounts| mounts.keys().cloned().collect())
        .unwrap_or_default();
    for volume in mounted {
        detach(&volume, true);
    }
    report(SERVICE_STOPPED);
}

unsafe extern "system" fn control(
    control: u32,
    event: u32,
    data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    #[cfg(feature = "fault-injection")]
    if control == SERVICE_CONTROL_DEVICEEVENT {
        return device_event(event, data);
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = (event, data);
    if matches!(control, SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN) {
        report(SERVICE_STOP_PENDING);
        let (lock, wake) = &STOP;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
    }
    NO_ERROR
}

fn report(state: u32) {
    let Some(&status) = STATUS.get() else {
        return;
    };
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    };
    let status_record = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: NO_ERROR,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        // Ejecting every mounted volume can take a while.
        dwWaitHint: 60_000,
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW.
    unsafe { SetServiceStatus(status as SERVICE_STATUS_HANDLE, &status_record) };
}

/// Send diagnostics to `%ProgramData%\CarryFS\service.log`; a service has no console.
fn log_to_file() {
    let directory = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "C:\\ProgramData".into())
        .join(NAME);
    let file = std::fs::create_dir_all(&directory).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("service.log"))
    });
    if let Ok(file) = file {
        // SAFETY: the handle is leaked on purpose and stays valid for the process.
        unsafe { SetStdHandle(STD_ERROR_HANDLE, file.into_raw_handle() as HANDLE) };
    }
}

fn interface_filter() -> CM_NOTIFY_FILTER {
    // SAFETY: CM_NOTIFY_FILTER is plain old data; all-zero is valid.
    let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
    filter.cbSize = size_of::<CM_NOTIFY_FILTER>() as u32;
    filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
    filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_DISK;
    filter
}

unsafe extern "system" fn on_arrival(
    _notification: HCMNOTIFICATION,
    _context: *const c_void,
    action: CM_NOTIFY_ACTION,
    data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    if action != CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL {
        return NO_ERROR;
    }
    // SAFETY: interface events carry a NUL-terminated symbolic link.
    let link = unsafe {
        let start = std::ptr::addr_of!((*data).u.DeviceInterface.SymbolicLink).cast::<u16>();
        let mut length = 0;
        while *start.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(start, length))
    };
    // Mounting reads the whole structure; keep the notification thread free.
    std::thread::spawn(move || {
        // The disk interface can arrive before its partitions can be opened.
        // Allow 20 half-second waits; a disk with no accessible partitions uses
        // the full retry budget.
        for _ in 0..20 {
            if !device::partitions(link.as_ref()).is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        attach_disk(&link);
    });
    NO_ERROR
}

/// Mount every CarryFS partition of `disk`, each on its own thread so that a
/// large volume does not delay its neighbors.
fn attach_disk(disk: &str) {
    for partition in device::partitions(disk.as_ref()) {
        std::thread::spawn(move || attach(&partition));
    }
}

/// Mount `volume` if it is CarryFS and not already mounted or being mounted.
/// The open, which runs the full structural check, holds no lock.
fn attach(volume: &str) {
    {
        let guard = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mounts) = guard.as_ref() else {
            return;
        };
        let mut mounting = MOUNTING.lock().unwrap_or_else(|e| e.into_inner());
        if mounts.contains_key(volume) || mounting.iter().any(|(name, _)| name == volume) {
            return;
        }
        mounting.push((volume.to_string(), false));
    }
    let opened = crate::is_carryfs(volume).then(|| crate::mount(volume, None, false));

    let mut guard = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let removed = {
        let mut mounting = MOUNTING.lock().unwrap_or_else(|e| e.into_inner());
        mounting
            .iter()
            .position(|(name, _)| name == volume)
            .map(|index| mounting.remove(index).1)
            .unwrap_or(false)
    };
    let (mount, (handle, release)) = match opened {
        None => return,
        Some(Ok(mounted)) => mounted,
        Some(Err(error)) => {
            eprintln!("carryfs: {volume}: {error}");
            return;
        }
    };
    // The stop flag is set before the stop path lists the mounts, so a mount
    // that sees it clear here is inserted in time to be listed.
    let stopping = *STOP.0.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mounts) = guard.as_mut().filter(|_| !stopping && !removed) else {
        let why = if removed {
            "disk removed"
        } else {
            "service stopping"
        };
        eprintln!("carryfs: {volume}: {why} while mounting; discarding the mount");
        drop(guard);
        if removed {
            drop(mount.stop());
        } else if let Err(error) = crate::eject(mount) {
            eprintln!("carryfs: {volume}: {error}");
        }
        return;
    };
    // Test builds can answer the removal query through the service control
    // handler instead (CARRYFS_QUERY_REMOVE_ROUTE=service).
    #[cfg(feature = "fault-injection")]
    let service_route =
        std::env::var("CARRYFS_QUERY_REMOVE_ROUTE").is_ok_and(|route| route == "service");
    #[cfg(not(feature = "fault-injection"))]
    let service_route = false;
    // SAFETY: CM_NOTIFY_FILTER is plain old data; all-zero is valid.
    let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
    filter.cbSize = size_of::<CM_NOTIFY_FILTER>() as u32;
    filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE;
    filter.u.DeviceHandle.hTarget = handle;
    // Successful registrations keep this callback context for the process lifetime,
    // leaking one boxed volume path per registration, even after unregistration.
    let context = Box::into_raw(Box::new(volume.to_string()));
    let mut notification: HCMNOTIFICATION = std::ptr::null_mut();
    let result = if service_route {
        CR_SUCCESS
    } else {
        // SAFETY: the filter names our open volume handle; the context stays
        // allocated until the notification is unregistered.
        unsafe {
            CM_Register_Notification(&filter, context.cast(), Some(on_device), &mut notification)
        }
    };
    if result != CR_SUCCESS {
        eprintln!(
            "carryfs: {volume}: removal notifications unavailable (CONFIGRET {result}); not mounting"
        );
        // SAFETY: registration failed, so nothing else references the context.
        drop(unsafe { Box::from_raw(context) });
        if let Err(error) = crate::eject(mount) {
            eprintln!("carryfs: {volume}: {error}");
        }
        return;
    }
    #[cfg(feature = "fault-injection")]
    let device = if service_route {
        match register_service_route(handle) {
            Some(device) => device,
            None => {
                eprintln!("carryfs: {volume}: device notification unavailable; not mounting");
                if let Err(error) = crate::eject(mount) {
                    eprintln!("carryfs: {volume}: {error}");
                }
                return;
            }
        }
    } else {
        0
    };
    #[cfg(feature = "fault-injection")]
    if service_route {
        eprintln!("carryfs: {volume}: removal query through the service control handler");
    }
    eprintln!("carryfs: {volume}: mounted {}", mount.mount_point);
    if let Some(notified) = NOTIFIED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        notified.remove(volume);
    }
    mounts.insert(
        volume.to_string(),
        Mounted {
            mount,
            notification: Notification {
                cm: notification,
                #[cfg(feature = "fault-injection")]
                device,
            },
            release,
        },
    );
}

/// Unmount `volume`; `eject` syncs first, while a vanished device is only dropped.
fn detach(volume: &str, eject: bool) {
    let mounted = {
        let mut guard = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
        let found = guard.as_mut().and_then(|mounts| mounts.remove(volume));
        if found.is_none() {
            // Still opening: tell `attach` to discard the mount when it finishes.
            let mut mounting = MOUNTING.lock().unwrap_or_else(|e| e.into_inner());
            for (_, removed) in mounting.iter_mut().filter(|(name, _)| name == volume) {
                *removed = true;
            }
        }
        found
    };
    let Some(mounted) = mounted else {
        return;
    };
    let point = mounted.mount.mount_point.clone();
    let result = if eject {
        crate::eject(mounted.mount)
    } else {
        drop(mounted.mount.stop());
        Ok(())
    };
    match result {
        Ok(()) => eprintln!("carryfs: {volume}: unmounted {point}"),
        Err(error) => {
            eprintln!("carryfs: {volume}: {error}");
            notify_lost(volume, &point, true);
        }
    }
    unregister(mounted.notification);
}

/// Answer a query-remove: close the disk handle and unmount on another thread,
/// or return false to veto. Device I/O and unmounting inside the callback both
/// blocked until Windows timed out after 60 s and vetoed the removal, so
/// changes not yet published cannot be published here. Instead the removal is
/// vetoed, a thread publishes them once the veto lets I/O through, and the
/// user's next attempt succeeds. A request in progress also vetoes.
fn release_for_removal(volume: &str) -> bool {
    let mut mounts = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    if MOUNTING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(name, _)| name == volume)
    {
        eprintln!("carryfs: {volume}: removal vetoed: the volume is still mounting");
        return false;
    }
    let Some(mounted) = mounts.as_mut().and_then(|mounts| mounts.get(volume)) else {
        return true;
    };
    let started = std::time::Instant::now();
    let mut removal = Removal::Busy;
    // A request that is not itself waiting on the device finishes quickly.
    for _ in 0..10 {
        // Windows allows about 60 s for the whole query.
        removal = mounted
            .mount
            .carry()
            .release_for_removal(|| mounted.release.release(std::time::Duration::from_secs(20)));
        if removal != Removal::Busy {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    match removal {
        Removal::Released(true) | Removal::Lost(true) => eprintln!(
            "carryfs: {volume}: released for removal in {:?}",
            started.elapsed()
        ),
        Removal::Released(false) | Removal::Lost(false) => {
            eprintln!(
                "carryfs: {volume}: device I/O did not stop within 20 s; removal may be vetoed"
            )
        }
        Removal::Busy => {
            eprintln!("carryfs: {volume}: removal vetoed: a request is in progress");
            return false;
        }
        Removal::Unsynced => {
            eprintln!("carryfs: {volume}: removal vetoed: publishing pending changes");
            notify_saving(volume, &mounted.mount.mount_point);
            let name = volume.to_string();
            std::thread::spawn(move || {
                let mounts = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(mounted) = mounts.as_ref().and_then(|mounts| mounts.get(&name)) {
                    match mounted.mount.carry().flush(None) {
                        Ok(_) => eprintln!("carryfs: {name}: published; removal can be retried"),
                        Err(code) => {
                            eprintln!(
                                "carryfs: {name}: publishing failed: NTSTATUS {code:#010x}; \
                                 the next removal releases the disk"
                            );
                            notify_lost(&name, &mounted.mount.mount_point, false);
                        }
                    }
                }
            });
            return false;
        }
    }
    let Some(mounted) = mounts.as_mut().and_then(|mounts| mounts.remove(volume)) else {
        return true;
    };
    drop(mounts);
    if matches!(removal, Removal::Lost(_)) {
        eprintln!(
            "carryfs: {volume}: the volume is faulted; changes since its last successful \
             publication are not on the disk. Removing it anyway"
        );
        notify_lost(volume, &mounted.mount.mount_point, true);
    }
    let name = volume.to_string();
    let stopping = std::thread::spawn(move || {
        let point = mounted.mount.mount_point.clone();
        drop(mounted.mount.stop());
        unregister(mounted.notification);
        eprintln!("carryfs: {name}: unmounted {point} for removal");
        // Windows sends no query-remove-failed notification to a closed handle,
        // so a vetoed removal is noticed by the partition still being present.
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if device::partition_present(&name) == Some(false) {
                return;
            }
        }
        eprintln!("carryfs: {name}: still present 10 s after removal was requested; remounting");
        attach(&name);
    });
    STOPPING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((volume.to_string(), stopping));
    true
}

/// Tell every logged-on user that changes to `point` never reached the disk,
/// once per mount. A service has no desktop, so the message goes to each
/// active or disconnected user session through the session manager and
/// returns without waiting for anyone; a disconnected user sees it on
/// reconnecting. With nobody logged on it is only logged.
fn notify_lost(volume: &str, point: &str, removed: bool) {
    let first = NOTIFIED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(volume.to_string());
    if !first {
        return;
    }
    // Test builds only: fault-injection runs fail on purpose and would leave
    // a message box on the desktop for each.
    #[cfg(feature = "fault-injection")]
    if std::env::var_os("CARRYFS_NO_NOTICE").is_some() {
        eprintln!("carryfs: {volume}: notice suppressed");
        return;
    }
    let advice = if removed {
        "The disk was removed."
    } else {
        "Choose Safely Remove again to remove the disk anyway."
    };
    let text = format!(
        "CarryFS could not write the latest changes to {point}.\n\nChanges made since its last \
         successful save are not on the disk. {advice} Before relying on the disk, check it with \
         “carryfs check” and “carryfs scrub”."
    );
    send_message(volume, &text, true, 0);
}

/// Show `text` in every active user session, and in disconnected ones too when
/// `disconnected` is set, without waiting. A box closes itself after `timeout`
/// seconds, and stays until dismissed when that is zero.
fn send_message(volume: &str, text: &str, disconnected: bool, timeout: u32) {
    let title = wide("CarryFS");
    let message = wide(text);
    let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: the out parameters are valid; the list is freed below.
    if unsafe { WTSEnumerateSessionsW(std::ptr::null_mut(), 0, 1, &mut sessions, &mut count) } == 0
    {
        eprintln!(
            "carryfs: {volume}: could not list sessions to notify: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    // SAFETY: the call returned `count` entries at `sessions`, valid until freed.
    let active: Vec<u32> = unsafe { std::slice::from_raw_parts(sessions, count as usize) }
        .iter()
        .filter(|session| {
            session.SessionId != 0
                && (session.State == WTSActive
                    || (disconnected && session.State == WTSDisconnected))
        })
        .map(|session| session.SessionId)
        .collect();
    // SAFETY: allocated by WTSEnumerateSessionsW and not used again.
    unsafe { WTSFreeMemory(sessions.cast()) };
    if active.is_empty() {
        eprintln!("carryfs: {volume}: no logged-on user to notify");
    }
    for session in active {
        let mut response = 0;
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call,
        // lengths are in bytes without the NUL, and the call does not wait.
        let sent = unsafe {
            WTSSendMessageW(
                std::ptr::null_mut(),
                session,
                title.as_ptr(),
                ((title.len() - 1) * 2) as u32,
                message.as_ptr(),
                ((message.len() - 1) * 2) as u32,
                MB_OK | MB_ICONWARNING,
                timeout,
                &mut response,
                0,
            )
        };
        if sent == 0 {
            eprintln!(
                "carryfs: {volume}: could not notify session {session}: {}",
                std::io::Error::last_os_error()
            );
        } else {
            eprintln!("carryfs: {volume}: notified session {session}");
        }
    }
}

/// How long a "saving" box stays, and the least time between two for one volume.
const SAVING_BOX_SECONDS: u32 = 10;
const SAVING_GAP: std::time::Duration = std::time::Duration::from_secs(30);

/// Volumes and when their user was last told that a refused Safely Remove is
/// waiting for a save, so repeated clicks during a copy do not stack boxes.
static SAVING: Mutex<Option<HashMap<String, std::time::Instant>>> = Mutex::new(None);

/// Whether a "saving" box for `volume` is due at `now`, recording it when so.
fn saving_due(
    last: &mut HashMap<String, std::time::Instant>,
    volume: &str,
    now: std::time::Instant,
) -> bool {
    if last
        .get(volume)
        .is_some_and(|before| now.duration_since(*before) < SAVING_GAP)
    {
        return false;
    }
    last.insert(volume.to_string(), now);
    true
}

/// Tell the user why a Safely Remove was just refused: Windows' own dialog
/// says to close the application, which is wrong advice. Only active
/// sessions get it, since a disconnected user cannot be clicking, and it closes
/// itself.
fn notify_saving(volume: &str, point: &str) {
    #[cfg(feature = "fault-injection")]
    if std::env::var_os("CARRYFS_NO_NOTICE").is_some() {
        return;
    }
    let due = saving_due(
        SAVING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new),
        volume,
        std::time::Instant::now(),
    );
    if due {
        send_message(
            volume,
            &format!(
                "CarryFS is saving the last changes to {point}.\n\nChoose Safely Remove again in a moment."
            ),
            false,
            SAVING_BOX_SECONDS,
        );
    }
}

/// Wait for a pending unmount of `volume`, if any.
fn finish_stopping(volume: &str) {
    let pending: Vec<_> = {
        let mut stopping = STOPPING.lock().unwrap_or_else(|e| e.into_inner());
        let (matching, rest) = stopping.drain(..).partition(|(name, _)| name == volume);
        *stopping = rest;
        matching
    };
    for (_, handle) in pending {
        let _ = handle.join();
    }
}

fn unregister(notification: Notification) {
    // A notification cannot be unregistered from inside its own callback.
    std::thread::spawn(move || {
        let notification = notification;
        if !notification.cm.is_null() {
            // SAFETY: registered in `attach` and unregistered exactly once.
            unsafe { CM_Unregister_Notification(notification.cm) };
        }
        #[cfg(feature = "fault-injection")]
        if notification.device != 0 {
            // SAFETY: registered in `attach` and unregistered exactly once.
            unsafe { UnregisterDeviceNotification(notification.device as _) };
        }
    });
}

/// Register the disk's volume handle for device events delivered to the
/// service control handler, which answers `DBT_DEVICEQUERYREMOVE`.
#[cfg(feature = "fault-injection")]
fn register_service_route(handle: HANDLE) -> Option<isize> {
    let status = *STATUS.get()?;
    // SAFETY: DEV_BROADCAST_HANDLE is plain old data; all-zero is valid.
    let mut filter: DEV_BROADCAST_HANDLE = unsafe { std::mem::zeroed() };
    filter.dbch_size = size_of::<DEV_BROADCAST_HANDLE>() as u32;
    filter.dbch_devicetype = DBT_DEVTYP_HANDLE;
    filter.dbch_handle = handle;
    // SAFETY: the filter names our open volume handle and the recipient is the
    // service status handle, as DEVICE_NOTIFY_SERVICE_HANDLE requires.
    let registered = unsafe {
        RegisterDeviceNotificationW(
            status as HANDLE,
            std::ptr::addr_of!(filter).cast(),
            DEVICE_NOTIFY_SERVICE_HANDLE,
        )
    };
    (!registered.is_null()).then_some(registered as isize)
}

/// The volume whose service-route registration is `device`.
#[cfg(feature = "fault-injection")]
fn volume_of_device(device: isize) -> Option<String> {
    let guard = MOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()?
        .iter()
        .find(|(_, mounted)| mounted.notification.device == device)
        .map(|(name, _)| name.clone())
}

/// A device event delivered through the service control handler.
#[cfg(feature = "fault-injection")]
fn device_event(event: u32, data: *mut c_void) -> u32 {
    if data.is_null() {
        return NO_ERROR;
    }
    // SAFETY: device events carry a DEV_BROADCAST_HDR.
    let header = unsafe { &*data.cast::<DEV_BROADCAST_HDR>() };
    if header.dbch_devicetype != DBT_DEVTYP_HANDLE {
        return NO_ERROR;
    }
    // SAFETY: the type says this is a DEV_BROADCAST_HANDLE.
    let device = unsafe { (*data.cast::<DEV_BROADCAST_HANDLE>()).dbch_hdevnotify } as isize;
    let Some(volume) = volume_of_device(device) else {
        return NO_ERROR;
    };
    match event {
        DBT_DEVICEQUERYREMOVE => {
            if !release_for_removal(&volume) {
                return BROADCAST_QUERY_DENY;
            }
        }
        DBT_DEVICEQUERYREMOVEFAILED => {
            std::thread::spawn(move || {
                finish_stopping(&volume);
                attach(&volume);
            });
        }
        DBT_DEVICEREMOVEPENDING | DBT_DEVICEREMOVECOMPLETE => {
            std::thread::spawn(move || {
                detach(&volume, false);
                finish_stopping(&volume);
            });
        }
        _ => {}
    }
    NO_ERROR
}

unsafe extern "system" fn on_device(
    _notification: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    // SAFETY: `attach` leaves the boxed volume path allocated for the process lifetime.
    let volume = unsafe { &*context.cast::<String>() };
    match action {
        // Close the handle before answering, or Windows vetoes the removal.
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE => {
            if !release_for_removal(volume) {
                return ERROR_CANCELLED;
            }
        }
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED => {
            let volume = volume.clone();
            std::thread::spawn(move || {
                finish_stopping(&volume);
                attach(&volume);
            });
        }
        // A surprise removal: the device is gone, so unpublished changes are
        // lost as in a crash, and reopening recovers the last published state.
        CM_NOTIFY_ACTION_DEVICEREMOVEPENDING | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE => {
            let volume = volume.clone();
            std::thread::spawn(move || {
                detach(&volume, false);
                finish_stopping(&volume);
            });
        }
        _ => {}
    }
    NO_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_saving_box_is_due_once_per_gap_for_each_volume() {
        let mut last = HashMap::new();
        let start = Instant::now();
        assert!(saving_due(&mut last, "a", start));
        assert!(!saving_due(&mut last, "a", start + Duration::from_secs(5)));
        assert!(saving_due(&mut last, "b", start + Duration::from_secs(5)));
        assert!(!saving_due(
            &mut last,
            "a",
            start + SAVING_GAP - Duration::from_millis(1)
        ));
        assert!(saving_due(&mut last, "a", start + SAVING_GAP));
        // A refusal inside the gap does not push the next box back.
        assert!(!saving_due(
            &mut last,
            "a",
            start + SAVING_GAP + Duration::from_secs(1)
        ));
    }
}
