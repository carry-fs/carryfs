//! Hand-written WinFsp 2.x bindings and callback trampolines.
//!
//! The GPL-3.0 `winfsp` and `winfsp-sys` crates do not match this project's
//! licenses, so the few structs and functions needed are declared here.
//! Each `layout!` line checks a size or offset at compile time against the
//! value in the official headers. The DLL is loaded at runtime from WinFsp's
//! registered install directory, as WinFsp's own `FspLoad` does, because it is
//! not on the default DLL search path.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

use crate::device::Disk;
use crate::fs::{
    CarryFs, FileInfo, NtStatus, PACING, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL,
    STATUS_INVALID_DEVICE_REQUEST, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_REPARSE, STATUS_SUCCESS,
    STATUS_UNEXPECTED_IO_ERROR,
};
use crate::wide;

/// Compile-time layout check against the value the C headers give.
macro_rules! layout {
    ($name:literal, $actual:expr, $expected:literal) => {
        const _: () = assert!($actual == $expected, $name);
    };
}

layout!("FILE_INFO", size_of::<FileInfo>(), 72);
layout!("VOLUME_PARAMS", size_of::<VolumeParams>(), 504);
layout!(
    "VP.FileInfoTimeout",
    std::mem::offset_of!(VolumeParams, file_info_timeout),
    32
);
layout!("VP.Prefix", std::mem::offset_of!(VolumeParams, prefix), 40);
layout!(
    "VP.FileSystemName",
    std::mem::offset_of!(VolumeParams, file_system_name),
    424
);
layout!("OPEN_FILE_INFO", size_of::<OpenFileInfo>(), 88);
layout!(
    "OFI.NormalizedName",
    std::mem::offset_of!(OpenFileInfo, normalized_name),
    72
);
layout!(
    "OFI.NormalizedNameSize",
    std::mem::offset_of!(OpenFileInfo, normalized_name_size),
    80
);
layout!("VOLUME_INFO", size_of::<VolumeInfo>(), 88);
layout!(
    "DIR_INFO.FileNameBuf",
    std::mem::offset_of!(DirInfo, name),
    104
);
layout!("INTERFACE", size_of::<Interface>(), 512);
layout!(
    "INTERFACE.ReadDirectory",
    std::mem::offset_of!(Interface, read_directory),
    144
);
layout!(
    "INTERFACE.ResolveReparsePoints",
    std::mem::offset_of!(Interface, resolve_reparse_points),
    152
);
layout!(
    "INTERFACE.DeleteReparsePoint",
    std::mem::offset_of!(Interface, delete_reparse_point),
    176
);
layout!("REQ.TargetOnFileSystem", TARGET_ON_FILE_SYSTEM, 40);
layout!("CONTEXT.Response", RESPONSE, 8);
layout!("RSP.OpenedFlags", OPENED_FLAGS, 124);
layout!("RSP.DisableCache", DISABLE_CACHE, 1);
layout!("NOTIFY_INFO", NOTIFY_INFO, 12);
layout!(
    "FILE_SYSTEM.UserContext",
    std::mem::offset_of!(FileSystem, user_context),
    8
);

/// Byte offset of `Req.FileSystemControl.TargetOnFileSystem` in
/// `FSP_FSCTL_TRANSACT_REQ`, the one request field `SetReparsePoint` needs.
const TARGET_ON_FILE_SYSTEM: usize = 40;
/// Byte offset of `Response` in `FSP_FILE_SYSTEM_OPERATION_CONTEXT`.
const RESPONSE: usize = 8;
/// Byte offset in `FSP_FSCTL_TRANSACT_RSP` of the bit-field word that ends
/// `Rsp.Create.Opened`, and its `DisableCache` bit.
const OPENED_FLAGS: usize = 124;
const DISABLE_CACHE: u32 = 1 << 0;
/// `FSP_FSCTL_NOTIFY_INFO` before its file name: a 16-bit size with the name,
/// then 32-bit `Filter` and `Action` at offsets 4 and 8.
const NOTIFY_INFO: usize = 12;
/// The most bytes of such records sent to WinFsp in one call.
const NOTIFY_BATCH: usize = 32 << 10;

// `FSP_FSCTL_VOLUME_PARAMS` flag bits, in declaration order from bit 0.
const CASE_PRESERVED_NAMES: u32 = 1 << 1;
const UNICODE_ON_DISK: u32 = 1 << 2;
/// Serve symlinks as `IO_REPARSE_TAG_SYMLINK` reparse points.
const REPARSE_POINTS: u32 = 1 << 4;
const READ_ONLY_VOLUME: u32 = 1 << 9;
/// Pass each open's own context. Without it WinFsp shares the first open's
/// context among all opens of a file, and the first close invalidates the rest.
const UM_FILE_CONTEXT_IS_USER_CONTEXT2: u32 = 1 << 16;
layout!("FLAG.CasePreservedNames", CASE_PRESERVED_NAMES, 2);
layout!("FLAG.UnicodeOnDisk", UNICODE_ON_DISK, 4);
layout!("FLAG.ReparsePoints", REPARSE_POINTS, 16);
layout!("FLAG.ReadOnlyVolume", READ_ONLY_VOLUME, 512);
layout!(
    "FLAG.UmFileContextIsUserContext2",
    UM_FILE_CONTEXT_IS_USER_CONTEXT2,
    65536
);
/// Ask `CanDelete` only for directories and read-only files. Files never report
/// the read-only attribute, so a writable mount skips a round trip per delete;
/// a read-only mount keeps asking, because `CanDelete` is where it refuses.
const POST_DISPOSITION_WHEN_NECESSARY_ONLY: u32 = 1 << 30;
layout!(
    "FLAG.PostDispositionWhenNecessaryOnly",
    POST_DISPOSITION_WHEN_NECESSARY_ONLY,
    1073741824
);
layout!(
    "VP.VolumeInfoTimeout",
    std::mem::offset_of!(VolumeParams, timeouts),
    460
);
layout!(
    "VP.SecurityTimeout",
    std::mem::offset_of!(VolumeParams, timeouts) + 8,
    468
);
layout!("FLAG1.VolumeInfoTimeoutValid", VOLUME_INFO_TIMEOUT_VALID, 1);
layout!("FLAG1.DirInfoTimeoutValid", DIR_INFO_TIMEOUT_VALID, 2);
layout!("FLAG1.SecurityTimeoutValid", SECURITY_TIMEOUT_VALID, 4);

const FILE_DIRECTORY_FILE: u32 = 0x1;
/// A create option, from `FILE_FLAG_WRITE_THROUGH`.
const FILE_WRITE_THROUGH: u32 = 0x2;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;
/// Allow the drain after `NoSpace` to outlast WinFsp's shorter default.
const IRP_TIMEOUT_MILLIS: u32 = 10 * 60 * 1000;
/// Kernel lifetime of directory, security, and volume information on a
/// writable mount.
const METADATA_TIMEOUT_MILLIS: u32 = 1000;
/// The same on a read-only mount, where nothing changes while the mount owns
/// the partition: one hour, the Linux adapter's read-only lifetime.
const READ_ONLY_TIMEOUT_MILLIS: u32 = 60 * 60 * 1000;
/// A `FileInfoTimeout` that never expires, which is how a WinFsp file system
/// asks for Windows' cache manager.
const FILE_DATA_CACHED: u32 = u32::MAX;
/// `FSP_FSCTL_VOLUME_PARAMS` V1 bits: the matching entry of `timeouts` is valid.
const VOLUME_INFO_TIMEOUT_VALID: u32 = 1 << 0;
const DIR_INFO_TIMEOUT_VALID: u32 = 1 << 1;
const SECURITY_TIMEOUT_VALID: u32 = 1 << 2;
/// Everyone gets full access: CarryFS stores no permissions.
const SECURITY_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)";

#[repr(C)]
struct VolumeParams {
    version: u16,
    sector_size: u16,
    sectors_per_allocation_unit: u16,
    max_component_length: u16,
    volume_creation_time: u64,
    volume_serial_number: u32,
    transact_timeout: u32,
    irp_timeout: u32,
    irp_capacity: u32,
    file_info_timeout: u32,
    flags: u32,
    prefix: [u16; 192],
    file_system_name: [u16; 16],
    additional_flags: u32,
    timeouts: [u32; 5],
    fsext_control_code: u32,
    reserved32: u32,
    reserved64: [u64; 2],
}

/// `FSP_FSCTL_OPEN_FILE_INFO`: what Create and Open really receive, so the
/// file system can report the stored spelling of the path it opened.
#[repr(C)]
struct OpenFileInfo {
    file_info: FileInfo,
    normalized_name: *mut u16,
    normalized_name_size: u16,
}

#[repr(C)]
struct VolumeInfo {
    total_size: u64,
    free_size: u64,
    volume_label_length: u16,
    volume_label: [u16; 32],
}

#[repr(C)]
struct DirInfo {
    size: u16,
    file_info: FileInfo,
    next_offset: [u8; 24],
    name: [u16; 256],
}

/// The leading fields of `FSP_FILE_SYSTEM`; WinFsp owns the rest.
#[repr(C)]
struct FileSystem {
    version: u16,
    user_context: *mut c_void,
}

type Fs = *mut FileSystem;
type Wstr = *const u16;
type Context = *mut c_void;
/// WinFsp's `GetReparsePointByName` callback for its reparse point helpers.
type ByName =
    unsafe extern "system" fn(Fs, *mut c_void, Wstr, u8, *mut c_void, *mut usize) -> NtStatus;

#[repr(C)]
struct Interface {
    get_volume_info: unsafe extern "system" fn(Fs, *mut VolumeInfo) -> NtStatus,
    set_volume_label: unsafe extern "system" fn(Fs, Wstr, *mut VolumeInfo) -> NtStatus,
    get_security_by_name:
        unsafe extern "system" fn(Fs, Wstr, *mut u32, PSECURITY_DESCRIPTOR, *mut usize) -> NtStatus,
    create: unsafe extern "system" fn(
        Fs,
        Wstr,
        u32,
        u32,
        u32,
        PSECURITY_DESCRIPTOR,
        u64,
        *mut Context,
        *mut FileInfo,
    ) -> NtStatus,
    open: unsafe extern "system" fn(Fs, Wstr, u32, u32, *mut Context, *mut FileInfo) -> NtStatus,
    overwrite: unsafe extern "system" fn(Fs, Context, u32, u8, u64, *mut FileInfo) -> NtStatus,
    cleanup: unsafe extern "system" fn(Fs, Context, Wstr, u32),
    close: unsafe extern "system" fn(Fs, Context),
    read: unsafe extern "system" fn(Fs, Context, *mut c_void, u64, u32, *mut u32) -> NtStatus,
    write: unsafe extern "system" fn(
        Fs,
        Context,
        *const c_void,
        u64,
        u32,
        u8,
        u8,
        *mut u32,
        *mut FileInfo,
    ) -> NtStatus,
    flush: unsafe extern "system" fn(Fs, Context, *mut FileInfo) -> NtStatus,
    get_file_info: unsafe extern "system" fn(Fs, Context, *mut FileInfo) -> NtStatus,
    set_basic_info:
        unsafe extern "system" fn(Fs, Context, u32, u64, u64, u64, u64, *mut FileInfo) -> NtStatus,
    set_file_size: unsafe extern "system" fn(Fs, Context, u64, u8, *mut FileInfo) -> NtStatus,
    can_delete: unsafe extern "system" fn(Fs, Context, Wstr) -> NtStatus,
    rename: unsafe extern "system" fn(Fs, Context, Wstr, Wstr, u8) -> NtStatus,
    get_security:
        unsafe extern "system" fn(Fs, Context, PSECURITY_DESCRIPTOR, *mut usize) -> NtStatus,
    set_security: usize,
    read_directory:
        unsafe extern "system" fn(Fs, Context, Wstr, Wstr, *mut c_void, u32, *mut u32) -> NtStatus,
    resolve_reparse_points: unsafe extern "system" fn(
        Fs,
        Wstr,
        u32,
        u8,
        *mut c_void,
        *mut c_void,
        *mut usize,
    ) -> NtStatus,
    get_reparse_point:
        unsafe extern "system" fn(Fs, Context, Wstr, *mut c_void, *mut usize) -> NtStatus,
    set_reparse_point: unsafe extern "system" fn(Fs, Context, Wstr, *mut c_void, usize) -> NtStatus,
    delete_reparse_point:
        unsafe extern "system" fn(Fs, Context, Wstr, *mut c_void, usize) -> NtStatus,
    /// Streams, EAs, and the 31 reserved slots stay null.
    unused: [usize; 41],
}

struct Api {
    create:
        unsafe extern "system" fn(Wstr, *const VolumeParams, *const Interface, *mut Fs) -> NtStatus,
    delete: unsafe extern "system" fn(Fs),
    set_mount_point: unsafe extern "system" fn(Fs, Wstr) -> NtStatus,
    start_dispatcher: unsafe extern "system" fn(Fs, u32) -> NtStatus,
    stop_dispatcher: unsafe extern "system" fn(Fs),
    add_dir_info: unsafe extern "system" fn(*const DirInfo, *mut c_void, u32, *mut u32) -> u8,
    notify: unsafe extern "system" fn(Fs, *const c_void, usize) -> NtStatus,
    find_reparse_point: unsafe extern "system" fn(Fs, ByName, *mut c_void, Wstr, *mut u32) -> u8,
    /// Returns `FSP_FILE_SYSTEM_OPERATION_CONTEXT`: the request, then the response.
    operation_context: unsafe extern "system" fn() -> *const *const u8,
    resolve_reparse_points: unsafe extern "system" fn(
        Fs,
        ByName,
        *mut c_void,
        Wstr,
        u32,
        u8,
        *mut c_void,
        *mut c_void,
        *mut usize,
    ) -> NtStatus,
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();
static SECURITY: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn load() -> Result<Api, String> {
    let mut directory = [0_u16; 260];
    let mut bytes = u32::try_from(size_of_val(&directory)).map_err(|e| e.to_string())?;
    // SAFETY: the key and value names are NUL-terminated, and `directory`
    // is writable for `bytes` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            wide("SOFTWARE\\WOW6432Node\\WinFsp").as_ptr(),
            wide("InstallDir").as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            directory.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return Err(format!("WinFsp is not installed (registry error {status})"));
    }
    let end = directory.iter().position(|&unit| unit == 0).unwrap_or(0);
    let path = String::from_utf16_lossy(&directory[..end]) + "bin\\winfsp-x64.dll";
    // SAFETY: the path is NUL-terminated.
    let module = unsafe { LoadLibraryW(wide(&path).as_ptr()) };
    if module.is_null() {
        return Err(format!(
            "cannot load {path}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let symbol = |name: &str| {
        let name = format!("{name}\0");
        // SAFETY: the module is loaded for the life of the process and the name is NUL-terminated.
        unsafe { GetProcAddress(module, name.as_ptr()) }
            .map(|function| function as usize)
            .ok_or_else(|| format!("{path} does not export {}", name.trim_end_matches('\0')))
    };
    // SAFETY: each export has the C signature declared in winfsp.h for the
    // field it is transmuted into.
    unsafe {
        Ok(Api {
            create: function(symbol("FspFileSystemCreate")?),
            delete: function(symbol("FspFileSystemDelete")?),
            set_mount_point: function(symbol("FspFileSystemSetMountPoint")?),
            start_dispatcher: function(symbol("FspFileSystemStartDispatcher")?),
            stop_dispatcher: function(symbol("FspFileSystemStopDispatcher")?),
            add_dir_info: function(symbol("FspFileSystemAddDirInfo")?),
            notify: function(symbol("FspFileSystemNotify")?),
            find_reparse_point: function(symbol("FspFileSystemFindReparsePoint")?),
            operation_context: function(symbol("FspFileSystemGetOperationContext")?),
            resolve_reparse_points: function(symbol("FspFileSystemResolveReparsePoints")?),
        })
    }
}

/// # Safety
/// `F` must be the function pointer type of the export at `address`.
unsafe fn function<F: Copy>(address: usize) -> F {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    // SAFETY: same size, and the caller guarantees the signature.
    unsafe { std::mem::transmute_copy(&address) }
}

fn security() -> Result<&'static [u8], String> {
    SECURITY
        .get_or_init(|| {
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: the SDDL string is NUL-terminated and `descriptor` receives a
            // LocalAlloc'd self-relative descriptor, freed below.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide(SECURITY_SDDL).as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(format!(
                    "cannot build the security descriptor: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: `descriptor` is a valid self-relative descriptor of the
            // returned length until it is freed.
            unsafe {
                let length = GetSecurityDescriptorLength(descriptor) as usize;
                let bytes = std::slice::from_raw_parts(descriptor.cast::<u8>(), length).to_vec();
                LocalFree(descriptor as HLOCAL);
                Ok(bytes)
            }
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

/// A mounted WinFsp file system serving one volume.
pub struct Mount {
    fs: Fs,
    carry: *const CarryFs<Disk>,
    pub mount_point: String,
    /// The checkpoint timer: dropping the sender stops it.
    ticker: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

/// How often the checkpoint timer asks whether publication is due.
const TICK: Duration = Duration::from_millis(100);

/// The file system for the timer thread, as the adapter and WinFsp know it.
struct Shared(*const CarryFs<Disk>, Fs);

// SAFETY: `CarryFs` serializes all access behind its mutex, WinFsp accepts
// notifications from any thread, and `Mount::stop` joins the timer before it
// deletes or frees either.
unsafe impl Send for Shared {}

// SAFETY: WinFsp file systems may be stopped from any thread, and `CarryFs`
// serializes all access behind its mutex.
unsafe impl Send for Mount {}

impl Mount {
    /// Mount on `mount_point` (such as `X:`) and start dispatching.
    pub fn start(
        carry: CarryFs<Disk>,
        mount_point: &str,
        read_only: bool,
        serial: u32,
    ) -> Result<Self, String> {
        let api = api()?;
        security()?;
        let timeout = if read_only {
            READ_ONLY_TIMEOUT_MILLIS
        } else {
            METADATA_TIMEOUT_MILLIS
        };
        let mut params = VolumeParams {
            version: size_of::<VolumeParams>() as u16,
            sector_size: 4096,
            sectors_per_allocation_unit: 1,
            max_component_length: 255,
            volume_creation_time: 0,
            volume_serial_number: serial,
            transact_timeout: 0,
            irp_timeout: IRP_TIMEOUT_MILLIS,
            irp_capacity: 0,
            // Windows caches the data of files opened without write access
            // (`bypass_cache`), and keeps it after the last handle closed.
            // The information of an open file then never expires: WinFsp
            // updates it from every reply and matches aliases
            // case-insensitively, and nothing else changes the volume, so
            // only side effects on other objects (a parent's times, while the
            // parent is open) go unseen. Every handle's Cleanup is posted,
            // because that is where its core handle is given up.
            file_info_timeout: FILE_DATA_CACHED,
            flags: CASE_PRESERVED_NAMES
                | UNICODE_ON_DISK
                | REPARSE_POINTS
                | UM_FILE_CONTEXT_IS_USER_CONTEXT2
                | if read_only {
                    READ_ONLY_VOLUME
                } else {
                    POST_DISPOSITION_WHEN_NECESSARY_ONLY
                },
            prefix: [0; 192],
            file_system_name: [0; 16],
            // A listing lives as long as the attributes it carries, as on
            // Linux. A listing right after a change through the mount shows
            // it.
            // The security descriptor is constant and volume information is
            // free space.
            additional_flags: VOLUME_INFO_TIMEOUT_VALID
                | DIR_INFO_TIMEOUT_VALID
                | SECURITY_TIMEOUT_VALID,
            timeouts: [timeout, timeout, timeout, 0, 0],
            fsext_control_code: 0,
            reserved32: 0,
            reserved64: [0; 2],
        };
        for (slot, unit) in params
            .file_system_name
            .iter_mut()
            .zip("CarryFS".encode_utf16())
        {
            *slot = unit;
        }
        let carry = Box::into_raw(Box::new(carry));
        let mut fs: Fs = std::ptr::null_mut();
        // SAFETY: the parameters and the static interface outlive the call, and
        // `fs` receives the new file system.
        let status =
            unsafe { (api.create)(wide("WinFsp.Disk").as_ptr(), &params, &INTERFACE, &mut fs) };
        let mut mount = Self {
            fs,
            carry,
            mount_point: mount_point.to_string(),
            ticker: None,
        };
        if status != STATUS_SUCCESS {
            // SAFETY: nothing else took ownership of the boxed file system.
            drop(unsafe { Box::from_raw(carry) });
            return Err(format!(
                "FspFileSystemCreate failed: NTSTATUS {status:#010x}"
            ));
        }
        // SAFETY: `fs` is a live WinFsp file system; UserContext is ours to set
        // before dispatch starts.
        unsafe { (*fs).user_context = carry.cast() };
        // SAFETY: `fs` is live and the mount point is NUL-terminated.
        let status = unsafe { (api.set_mount_point)(fs, wide(mount_point).as_ptr()) };
        let status = if status == STATUS_SUCCESS {
            // SAFETY: `fs` is live; 0 selects one thread per processor.
            unsafe { (api.start_dispatcher)(fs, 0) }
        } else {
            status
        };
        if status != STATUS_SUCCESS {
            let _ = mount.stop();
            return Err(format!(
                "mounting {mount_point} failed: NTSTATUS {status:#010x}"
            ));
        }
        // The timer publishes on a writable mount, and on every mount tells
        // Windows which files to drop.
        let (stop, stopped) = mpsc::channel();
        let shared = Shared(carry, fs);
        let timer = std::thread::spawn(move || {
            let shared = shared;
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(TICK) {
                // SAFETY: the box and the WinFsp file system live until
                // `stop` has joined this thread.
                unsafe {
                    (*shared.0).tick(Instant::now(), PACING);
                    refresh(&*shared.0, shared.1);
                }
            }
        });
        mount.ticker = Some((stop, timer));
        Ok(mount)
    }

    /// The served file system, for answering device notifications.
    pub fn carry(&self) -> &CarryFs<Disk> {
        // SAFETY: the box is freed only by `stop`, which consumes the mount.
        unsafe { &*self.carry }
    }

    /// Stop dispatching, unmount, and return the file system for eject.
    pub fn stop(mut self) -> CarryFs<Disk> {
        // The timer first: it notifies through `fs`.
        if let Some((stop, timer)) = self.ticker.take() {
            drop(stop);
            let _ = timer.join();
        }
        // `api` succeeded before a Mount existed.
        if let Ok(api) = api() {
            // SAFETY: `fs` is live; stopping waits for in-flight callbacks, and
            // deleting also removes the mount point.
            unsafe {
                (api.stop_dispatcher)(self.fs);
                (api.delete)(self.fs);
            }
        }
        print_stats();
        // SAFETY: no callback can run after the file system is deleted, so the
        // box is uniquely owned again.
        *unsafe { Box::from_raw(self.carry.cast_mut()) }
    }
}

static INTERFACE: Interface = Interface {
    get_volume_info,
    set_volume_label,
    get_security_by_name,
    create,
    open,
    overwrite,
    cleanup,
    close,
    read,
    write,
    flush,
    get_file_info,
    set_basic_info,
    set_file_size,
    can_delete,
    rename,
    get_security,
    set_security: 0,
    read_directory,
    resolve_reparse_points,
    get_reparse_point,
    set_reparse_point,
    delete_reparse_point,
    unused: [0; 41],
};

/// # Safety
/// `fs` must be a file system created by `Mount::start`.
unsafe fn carry<'a>(fs: Fs) -> &'a CarryFs<Disk> {
    // SAFETY: the caller guarantees UserContext holds the boxed CarryFs,
    // which outlives dispatching.
    unsafe { &*(*fs).user_context.cast::<CarryFs<Disk>>() }
}

/// # Safety
/// `text` must be null or NUL-terminated.
unsafe fn wstr<'a>(text: Wstr) -> Option<&'a [u16]> {
    if text.is_null() {
        return None;
    }
    let mut length = 0;
    // SAFETY: the caller guarantees a terminator before the end of the allocation.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        Some(std::slice::from_raw_parts(text, length))
    }
}

/// Per-callback counts and time, kept when `CARRYFS_STATS` or `CARRYFS_TRACE`
/// is set and printed when the mount stops.
type Stats = Mutex<BTreeMap<&'static str, (u64, Duration)>>;

fn stats() -> Option<&'static Stats> {
    static STATS: OnceLock<Option<Stats>> = OnceLock::new();
    STATS
        .get_or_init(|| {
            (std::env::var_os("CARRYFS_STATS").is_some()
                || std::env::var_os("CARRYFS_TRACE").is_some())
            .then(|| Mutex::new(BTreeMap::new()))
        })
        .as_ref()
}

fn print_stats() {
    let Some(stats) = stats() else { return };
    let Ok(stats) = stats.lock() else { return };
    let total: Duration = stats.values().map(|(_, time)| *time).sum();
    eprintln!("carryfs: callbacks: {total:?} in total");
    for (name, (calls, time)) in stats.iter() {
        let each = time.as_micros() / u128::from((*calls).max(1));
        eprintln!("carryfs: callback {name}: {calls} calls, {time:?}, {each} us each");
    }
}

/// Contain panics: unwinding into WinFsp's dispatcher is undefined behavior.
/// With `CARRYFS_TRACE` set, every callback and its status is printed.
fn guard(name: &'static str, operation: impl FnOnce() -> Result<(), NtStatus>) -> NtStatus {
    static TRACE: OnceLock<bool> = OnceLock::new();
    let trace = *TRACE.get_or_init(|| std::env::var_os("CARRYFS_TRACE").is_some());
    let thread = std::thread::current().id();
    if trace {
        eprintln!("carryfs: {thread:?} {name} enter");
    }
    let start = Instant::now();
    let status = match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(status)) => status,
        Err(_) => {
            eprintln!("carryfs: {name} panicked");
            STATUS_UNEXPECTED_IO_ERROR
        }
    };
    if let Some(stats) = stats()
        && let Ok(mut stats) = stats.lock()
    {
        let entry = stats.entry(name).or_default();
        entry.0 += 1;
        entry.1 += start.elapsed();
    }
    if trace {
        eprintln!("carryfs: {thread:?} {name} {status:#010x}");
    }
    status
}

/// Report the stored spelling of an opened path. Windows uses it for
/// `FileNameInformation`, `GetFinalPathNameByHandle`, and change
/// notifications.
///
/// WinFsp requires a normalized name that differs from the requested path only
/// in case, so this is skipped when the lengths differ, as they do when NFC
/// composes a decomposed name. It is also skipped when WinFsp's buffer is too
/// small. Windows then reports the spelling the caller used.
///
/// # Safety
/// `info` must be the `FileInfo` of an `OPEN_FILE_INFO` from Create or Open.
unsafe fn set_normalized_name(info: *mut FileInfo, name: &[u16], requested: &[u16]) {
    if name.len() != requested.len() {
        return;
    }
    // SAFETY: FileInfo is the first member of OPEN_FILE_INFO, so they share an address.
    let open = unsafe { &mut *info.cast::<OpenFileInfo>() };
    let bytes = size_of_val(name);
    if open.normalized_name.is_null() || bytes > usize::from(open.normalized_name_size) {
        return;
    }
    // SAFETY: the buffer holds at least `normalized_name_size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), open.normalized_name, name.len()) };
    open.normalized_name_size = bytes as u16;
}

fn key(context: Context) -> u64 {
    context as u64
}

/// Keep the open being answered out of Windows' cache: every read and write
/// through it reaches the adapter when it is made. Opens with write access
/// ask for this, so a write is never held back in Windows' memory, and a
/// write through one drops the range it covers from what Windows cached.
///
/// # Safety
/// Must be called inside a Create or Open callback.
unsafe fn bypass_cache() -> Result<(), NtStatus> {
    let api = api().map_err(|_| STATUS_UNEXPECTED_IO_ERROR)?;
    // SAFETY: during a callback WinFsp's operation context holds the response
    // it will send, which for Create and Open ends in the `Opened` flags.
    unsafe {
        let response = (api.operation_context)()
            .cast::<u8>()
            .add(RESPONSE)
            .cast::<*mut u8>()
            .read();
        let flags = response.add(OPENED_FLAGS).cast::<u32>();
        flags.write_unaligned(flags.read_unaligned() | DISABLE_CACHE);
    }
    Ok(())
}

/// Tell WinFsp of the paths requests made stale in Windows
/// (`CarryFs::take_stale`). For a path it is told has changed, WinFsp writes
/// out and drops the pages Windows holds of it and forgets its cached
/// information, on a system thread. A filter of zero reports the change to
/// no program watching the directory.
///
/// # Safety
/// `fs` must be the live WinFsp file system that serves `carry`.
unsafe fn refresh(carry: &CarryFs<Disk>, fs: Fs) {
    let stale = carry.take_stale();
    if stale.is_empty() {
        return;
    }
    let Ok(api) = api() else { return };
    // One call carries many records, each a header and a name, padded to
    // eight bytes; Filter and Action stay zero.
    let mut records: Vec<u64> = Vec::new();
    let send = |records: &mut Vec<u64>| {
        // SAFETY: the caller guarantees `fs`; the buffer holds whole records.
        let status =
            unsafe { (api.notify)(fs, records.as_ptr().cast(), size_of_val(&records[..])) };
        if status != STATUS_SUCCESS {
            eprintln!("carryfs: telling Windows of changes failed: NTSTATUS {status:#010x}");
        }
        records.clear();
    };
    for path in stale {
        let size = NOTIFY_INFO + size_of_val(path.as_slice());
        let Ok(header) = u16::try_from(size) else {
            continue;
        };
        if size_of_val(&records[..]) + size > NOTIFY_BATCH {
            send(&mut records);
        }
        let at = records.len();
        records.resize(at + size.div_ceil(8), 0);
        records[at] = u64::from(header);
        // SAFETY: the record just added holds `size` bytes, the name after
        // the header.
        unsafe {
            std::ptr::copy_nonoverlapping(
                path.as_ptr().cast::<u8>(),
                records.as_mut_ptr().add(at).cast::<u8>().add(NOTIFY_INFO),
                size_of_val(path.as_slice()),
            );
        }
    }
    if !records.is_empty() {
        send(&mut records);
    }
}

/// Whether an open may write file data.
fn writes(access: u32) -> bool {
    access & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0
}

unsafe extern "system" fn get_volume_info(fs: Fs, info: *mut VolumeInfo) -> NtStatus {
    guard("get_volume_info", || {
        // SAFETY: WinFsp passes its live file system and a writable VolumeInfo.
        let (carry, info) = unsafe { (carry(fs), &mut *info) };
        let (total, free) = carry.volume_size()?;
        info.total_size = total;
        info.free_size = free;
        let label: Vec<u16> = "CarryFS".encode_utf16().collect();
        info.volume_label[..label.len()].copy_from_slice(&label);
        info.volume_label_length = (label.len() * 2) as u16;
        Ok(())
    })
}

unsafe extern "system" fn set_volume_label(_: Fs, _: Wstr, _: *mut VolumeInfo) -> NtStatus {
    STATUS_INVALID_DEVICE_REQUEST
}

unsafe extern "system" fn get_security_by_name(
    fs: Fs,
    name: Wstr,
    attributes: *mut u32,
    descriptor: PSECURITY_DESCRIPTOR,
    size: *mut usize,
) -> NtStatus {
    guard("get_security_by_name", || {
        // SAFETY: WinFsp passes a NUL-terminated path; the out pointers may be null.
        let found = match unsafe { carry(fs).attributes_by_name(wstr(name).unwrap_or_default()) } {
            Ok(found) => found,
            // A path through a symlink is missing here; WinFsp then resolves
            // it, taking the index of the link component in `attributes`. A
            // path whose last name alone is missing ran through directories
            // only, so there is no link to look for: every create asks.
            Err(status @ STATUS_OBJECT_PATH_NOT_FOUND) => {
                let api = api().map_err(|_| STATUS_UNEXPECTED_IO_ERROR)?;
                // SAFETY: `fs` and `name` come from WinFsp, and `attributes` is
                // null or writable.
                let found = unsafe {
                    (api.find_reparse_point)(
                        fs,
                        reparse_by_name,
                        std::ptr::null_mut(),
                        name,
                        attributes,
                    )
                };
                return Err(if found != 0 { STATUS_REPARSE } else { status });
            }
            Err(status) => return Err(status),
        };
        if !attributes.is_null() {
            // SAFETY: non-null out pointer from WinFsp.
            unsafe { *attributes = found };
        }
        // SAFETY: same descriptor contract as GetSecurity.
        unsafe { copy_security(descriptor, size) }
    })
}

/// # Safety
/// `size` must be null or point to the capacity of `descriptor`.
unsafe fn copy_security(
    descriptor: PSECURITY_DESCRIPTOR,
    size: *mut usize,
) -> Result<(), NtStatus> {
    if size.is_null() {
        return Ok(());
    }
    let bytes = security().map_err(|_| STATUS_UNEXPECTED_IO_ERROR)?;
    // SAFETY: `size` is non-null; `descriptor` holds that many writable bytes.
    unsafe {
        let capacity = *size;
        *size = bytes.len();
        if descriptor.is_null() || capacity < bytes.len() {
            return Err(STATUS_BUFFER_OVERFLOW);
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), descriptor.cast::<u8>(), bytes.len());
    }
    Ok(())
}

unsafe extern "system" fn create(
    fs: Fs,
    name: Wstr,
    options: u32,
    access: u32,
    _attributes: u32,
    _descriptor: PSECURITY_DESCRIPTOR,
    _allocation: u64,
    context: *mut Context,
    info: *mut FileInfo,
) -> NtStatus {
    guard("create", || {
        // SAFETY: WinFsp passes a NUL-terminated path, writable out pointers,
        // and an OPEN_FILE_INFO whose first member is the FileInfo.
        unsafe {
            let (key, file, normalized) = carry(fs).create(
                wstr(name).unwrap_or_default(),
                options & FILE_DIRECTORY_FILE != 0,
                options & FILE_WRITE_THROUGH != 0,
            )?;
            *context = key as Context;
            *info = file;
            set_normalized_name(info, &normalized, wstr(name).unwrap_or_default());
            if writes(access) {
                bypass_cache()?;
            }
        }
        Ok(())
    })
}

unsafe extern "system" fn open(
    fs: Fs,
    name: Wstr,
    options: u32,
    access: u32,
    context: *mut Context,
    info: *mut FileInfo,
) -> NtStatus {
    guard("open", || {
        // SAFETY: WinFsp passes a NUL-terminated path, writable out pointers,
        // and an OPEN_FILE_INFO whose first member is the FileInfo.
        unsafe {
            let (key, file, normalized) = carry(fs).open(
                wstr(name).unwrap_or_default(),
                writes(access),
                options & FILE_WRITE_THROUGH != 0,
            )?;
            *context = key as Context;
            *info = file;
            set_normalized_name(info, &normalized, wstr(name).unwrap_or_default());
            if writes(access) {
                bypass_cache()?;
            }
        }
        Ok(())
    })
}

unsafe extern "system" fn overwrite(
    fs: Fs,
    context: Context,
    _attributes: u32,
    _replace: u8,
    _allocation: u64,
    info: *mut FileInfo,
) -> NtStatus {
    // SAFETY: WinFsp passes a writable FileInfo.
    guard("overwrite", || unsafe {
        *info = carry(fs).overwrite(key(context))?;
        Ok(())
    })
}

unsafe extern "system" fn cleanup(fs: Fs, context: Context, name: Wstr, flags: u32) {
    guard("cleanup", || {
        // SAFETY: the name is null or NUL-terminated.
        unsafe { carry(fs).cleanup(key(context), wstr(name), flags) };
        Ok(())
    });
}

unsafe extern "system" fn close(fs: Fs, context: Context) {
    guard("close", || {
        // SAFETY: WinFsp passes its live file system.
        unsafe { carry(fs).close(key(context)) };
        Ok(())
    });
}

unsafe extern "system" fn read(
    fs: Fs,
    context: Context,
    buffer: *mut c_void,
    offset: u64,
    length: u32,
    transferred: *mut u32,
) -> NtStatus {
    guard("read", || {
        // SAFETY: WinFsp passes a buffer writable for `length` bytes.
        unsafe {
            let buffer = std::slice::from_raw_parts_mut(buffer.cast::<u8>(), length as usize);
            let read = carry(fs).read(key(context), offset, buffer)?;
            *transferred = read as u32;
        }
        Ok(())
    })
}

unsafe extern "system" fn write(
    fs: Fs,
    context: Context,
    buffer: *const c_void,
    offset: u64,
    length: u32,
    to_end: u8,
    constrained: u8,
    transferred: *mut u32,
    info: *mut FileInfo,
) -> NtStatus {
    guard("write", || {
        // SAFETY: WinFsp passes a buffer readable for `length` bytes and writable out pointers.
        unsafe {
            let data = std::slice::from_raw_parts(buffer.cast::<u8>(), length as usize);
            let (written, file) =
                carry(fs).write(key(context), offset, data, to_end != 0, constrained != 0)?;
            *transferred = written as u32;
            *info = file;
        }
        Ok(())
    })
}

unsafe extern "system" fn flush(fs: Fs, context: Context, info: *mut FileInfo) -> NtStatus {
    guard("flush", || {
        // A null context flushes the volume; FileInfo is then ignored.
        let context = (!context.is_null()).then(|| key(context));
        // SAFETY: WinFsp passes its live file system and a null or writable FileInfo.
        unsafe {
            let file = carry(fs).flush(context)?;
            if !info.is_null() {
                *info = file;
            }
        }
        Ok(())
    })
}

unsafe extern "system" fn get_file_info(fs: Fs, context: Context, info: *mut FileInfo) -> NtStatus {
    // SAFETY: WinFsp passes a writable FileInfo.
    guard("get_file_info", || unsafe {
        *info = carry(fs).file_info(key(context))?;
        Ok(())
    })
}

unsafe extern "system" fn set_basic_info(
    fs: Fs,
    context: Context,
    _attributes: u32,
    creation: u64,
    _access: u64,
    last_write: u64,
    _change: u64,
    info: *mut FileInfo,
) -> NtStatus {
    // SAFETY: WinFsp passes a writable FileInfo.
    guard("set_basic_info", || unsafe {
        *info = carry(fs).set_basic_info(key(context), creation, last_write)?;
        Ok(())
    })
}

unsafe extern "system" fn set_file_size(
    fs: Fs,
    context: Context,
    size: u64,
    allocation: u8,
    info: *mut FileInfo,
) -> NtStatus {
    // SAFETY: WinFsp passes a writable FileInfo.
    guard("set_file_size", || unsafe {
        *info = carry(fs).set_file_size(key(context), size, allocation != 0)?;
        Ok(())
    })
}

unsafe extern "system" fn can_delete(fs: Fs, context: Context, _name: Wstr) -> NtStatus {
    // SAFETY: WinFsp passes its live file system.
    guard("can_delete", || unsafe {
        carry(fs).can_delete(key(context))
    })
}

unsafe extern "system" fn rename(
    fs: Fs,
    _context: Context,
    name: Wstr,
    new_name: Wstr,
    replace: u8,
) -> NtStatus {
    // SAFETY: WinFsp passes NUL-terminated paths.
    guard("rename", || unsafe {
        carry(fs).rename(
            wstr(name).unwrap_or_default(),
            wstr(new_name).unwrap_or_default(),
            replace != 0,
        )
    })
}

unsafe extern "system" fn get_security(
    _fs: Fs,
    _context: Context,
    descriptor: PSECURITY_DESCRIPTOR,
    size: *mut usize,
) -> NtStatus {
    // SAFETY: WinFsp passes the descriptor capacity in `size`.
    guard("get_security", || unsafe {
        copy_security(descriptor, size)
    })
}

unsafe extern "system" fn read_directory(
    fs: Fs,
    context: Context,
    _pattern: Wstr,
    marker: Wstr,
    buffer: *mut c_void,
    length: u32,
    transferred: *mut u32,
) -> NtStatus {
    guard("read_directory", || {
        let api = api().map_err(|_| STATUS_UNEXPECTED_IO_ERROR)?;
        // SAFETY: WinFsp passes a null or NUL-terminated marker, a buffer
        // writable for `length` bytes, and `transferred` as its fill level.
        unsafe {
            let ended = carry(fs).read_directory(key(context), wstr(marker), |name, info| {
                let mut entry = DirInfo {
                    size: (104 + name.len() * 2) as u16,
                    file_info: *info,
                    next_offset: [0; 24],
                    name: [0; 256],
                };
                entry.name[..name.len()].copy_from_slice(name);
                (api.add_dir_info)(&entry, buffer, length, transferred) != 0
            })?;
            if ended {
                (api.add_dir_info)(std::ptr::null(), buffer, length, transferred);
            }
        }
        Ok(())
    })
}

/// # Safety
/// `size` must be null or valid, and `buffer` must be null or writable for
/// `*size` bytes.
unsafe fn copy_reparse(data: &[u8], buffer: *mut c_void, size: *mut usize) -> Result<(), NtStatus> {
    if buffer.is_null() || size.is_null() {
        return Ok(());
    }
    // SAFETY: `size` is non-null and holds the capacity of `buffer`.
    unsafe {
        if data.len() > *size {
            return Err(STATUS_BUFFER_TOO_SMALL);
        }
        *size = data.len();
        std::ptr::copy_nonoverlapping(data.as_ptr(), buffer.cast::<u8>(), data.len());
    }
    Ok(())
}

unsafe extern "system" fn reparse_by_name(
    fs: Fs,
    _: *mut c_void,
    name: Wstr,
    _directory: u8,
    buffer: *mut c_void,
    size: *mut usize,
) -> NtStatus {
    // SAFETY: WinFsp passes a NUL-terminated path and, with a buffer, its capacity.
    guard("reparse_by_name", || unsafe {
        let data = carry(fs).reparse_by_name(wstr(name).unwrap_or_default())?;
        copy_reparse(&data, buffer, size)
    })
}

unsafe extern "system" fn resolve_reparse_points(
    fs: Fs,
    name: Wstr,
    index: u32,
    last: u8,
    status: *mut c_void,
    buffer: *mut c_void,
    size: *mut usize,
) -> NtStatus {
    let Ok(api) = api() else {
        return STATUS_UNEXPECTED_IO_ERROR;
    };
    // SAFETY: every argument is WinFsp's own, passed through to its helper.
    unsafe {
        (api.resolve_reparse_points)(
            fs,
            reparse_by_name,
            std::ptr::null_mut(),
            name,
            index,
            last,
            status,
            buffer,
            size,
        )
    }
}

unsafe extern "system" fn get_reparse_point(
    fs: Fs,
    context: Context,
    _name: Wstr,
    buffer: *mut c_void,
    size: *mut usize,
) -> NtStatus {
    // SAFETY: WinFsp passes a writable buffer and its capacity.
    guard("get_reparse_point", || unsafe {
        let data = carry(fs).reparse(key(context))?;
        copy_reparse(&data, buffer, size)
    })
}

unsafe extern "system" fn set_reparse_point(
    fs: Fs,
    context: Context,
    name: Wstr,
    buffer: *mut c_void,
    size: usize,
) -> NtStatus {
    guard("set_reparse_point", || {
        if buffer.is_null() {
            return Err(crate::fs::STATUS_IO_REPARSE_DATA_INVALID);
        }
        let api = api().map_err(|_| STATUS_UNEXPECTED_IO_ERROR)?;
        // SAFETY: WinFsp passes a NUL-terminated path and `size` readable
        // bytes, and during this callback its operation context holds the
        // FSCTL request, which is larger than the field read here.
        unsafe {
            let request = *(api.operation_context)();
            let on_volume = request
                .add(TARGET_ON_FILE_SYSTEM)
                .cast::<u16>()
                .read_unaligned();
            let data = std::slice::from_raw_parts(buffer.cast::<u8>(), size);
            carry(fs).set_reparse(
                key(context),
                wstr(name).unwrap_or_default(),
                data,
                on_volume,
            )
        }
    })
}

/// A symlink cannot become a plain file again; remove it instead.
unsafe extern "system" fn delete_reparse_point(
    _: Fs,
    _: Context,
    _: Wstr,
    _: *mut c_void,
    _: usize,
) -> NtStatus {
    STATUS_INVALID_DEVICE_REQUEST
}
