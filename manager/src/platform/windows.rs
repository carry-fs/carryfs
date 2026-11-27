//! Windows backends for the manager's disk tasks.
//!
//! Disks, partitions, and volumes are read through IOCTLs that need no
//! administrator rights. Windows reports a CarryFS partition as RAW, so the
//! CarryFS service's control pipe says which partitions it mounted and where,
//! and an elevated manager reads the other partitions' roots. Reading a
//! partition, erasing, upgrading, and changing drive letters need an elevated
//! manager, which Unlock starts through UAC.

use super::{Device, DeviceAction, DeviceKind, Result, output};
use carryfs_core::{ImageDevice, MountMode, Volume, VolumeLabel};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::Duration,
};
use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::{
        CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_DevNode_Status,
        CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW,
        CM_Get_Device_Interface_PropertyW, CM_Get_Parent, CM_LOCATE_DEVNODE_NORMAL,
        CM_Locate_DevNodeW, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
        CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CM_Request_Device_EjectW,
        CR_SUCCESS, DN_REMOVABLE, HCMNOTIFICATION, PNP_VetoTypeUnknown,
    },
    Devices::Properties::{DEVPKEY_Device_InstanceId, DEVPROPTYPE},
    Foundation::{
        CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
    },
    Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
    Storage::FileSystem::{
        DeleteVolumeMountPointW, FILE_SHARE_READ, FILE_SHARE_WRITE, FindFirstVolumeW,
        FindNextVolumeW, FindVolumeClose, GetLogicalDrives, GetVolumeInformationW,
        GetVolumePathNamesForVolumeNameW, SetVolumeMountPointW,
    },
    System::{
        IO::DeviceIoControl,
        Ioctl::{
            DISK_GEOMETRY_EX, DRIVE_LAYOUT_INFORMATION_EX, FSCTL_ALLOW_EXTENDED_DASD_IO,
            FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, GUID_DEVINTERFACE_DISK,
            GUID_DEVINTERFACE_VOLUME, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO,
            IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
            PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT, PARTITION_STYLE_MBR,
            PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
            STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const ERROR_INVALID_PARAMETER: i32 = 87;
/// CTL_CODE(IOCTL_VOLUME_BASE, 0, METHOD_BUFFERED, FILE_ANY_ACCESS).
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
/// GPT partition types Windows keeps for itself: EFI system, Microsoft
/// reserved, and Windows recovery.
const SYSTEM_GPT_TYPES: [u128; 3] = [
    0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b,
    0xe3c9e316_0b5c_4db8_817d_f92df00215ae,
    0xde94bba4_06d1_4d40_a16a_bfd50179d6ac,
];
/// MBR types: EFI system, Windows recovery, and dynamic disk.
const SYSTEM_MBR_TYPES: [u8; 3] = [0xef, 0x27, 0x42];
const BASIC_DATA: &str = "{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}";
/// The service's control pipe (windows/src/control.rs).
const PIPE: &str = r"\\.\pipe\CarryFS";

/// `program` with no console window: CarryFS Manager is a windowed app.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

fn system32(name: &str) -> PathBuf {
    std::env::var_os("SystemRoot")
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
        .join("System32")
        .join(name)
}

/// Where the installers put CarryFS.
pub fn installed() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from)
        .join("CarryFS")
}

pub fn curl() -> Command {
    command(system32("curl.exe"))
}

/// Open `path` in File Explorer.
pub fn show(path: &Path) -> Result<()> {
    explorer().arg(path).spawn()?;
    Ok(())
}

/// Select `path` in a File Explorer window.
pub fn reveal(path: &Path) -> Result<()> {
    // Explorer reads `/select,"PATH"` itself; Windows paths hold no quotes.
    explorer()
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()?;
    Ok(())
}

fn explorer() -> Command {
    // Explorer exits with 1 even when it opens the window, so its status is not checked.
    Command::new(
        std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
            .join("explorer.exe"),
    )
}

/// Run a PowerShell script for the storage tasks Windows offers no simpler
/// API for. A failure carries the script's error message.
fn powershell(script: &str) -> Result<String> {
    let script = format!(
        "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'\n\
         try {{\n{script}\n}} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 1 }}"
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let stdout = output(
        command(system32(r"WindowsPowerShell\v1.0\powershell.exe"))
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
            .arg(base64(&utf16))
            .stdin(Stdio::null()),
    )?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            text.push(if i <= chunk.len() {
                ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char
            } else {
                '='
            });
        }
    }
    text
}

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

fn from_wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// The inverse of `GUID::from_u128`.
fn guid_value(id: &windows_sys::core::GUID) -> u128 {
    u128::from(id.data1) << 96
        | u128::from(id.data2) << 80
        | u128::from(id.data3) << 64
        | u128::from(u64::from_be_bytes(id.data4))
}

fn guid(id: &windows_sys::core::GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        id.data1,
        id.data2,
        id.data3,
        id.data4[0],
        id.data4[1],
        id.data4[2],
        id.data4[3],
        id.data4[4],
        id.data4[5],
        id.data4[6],
        id.data4[7]
    )
}

// ------------------------------------------------------------- elevation

/// Whether this process runs as administrator.
pub fn elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo handle needs no closing and the token is an out parameter.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0;
        // SAFETY: the buffer is a live TOKEN_ELEVATION of the size passed.
        let answered = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&raw mut elevation).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
        } != 0;
        // SAFETY: opened above and not used again.
        unsafe { CloseHandle(token) };
        answered && elevation.TokenIsElevated != 0
    })
}

/// Start this app as administrator through UAC, to select `select` once
/// it lists the disks. The caller exits once it starts.
pub fn relaunch_elevated(select: Option<&str>) -> Result<()> {
    let exe = wide(std::env::current_exe()?);
    let parameters = wide(
        select
            .filter(|id| super::valid_id(id))
            .map(|id| format!("--select {id}"))
            .unwrap_or_default(),
    );
    let verb = wide("runas");
    // SAFETY: every string is NUL-terminated and outlives the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as usize;
    if result <= 32 {
        // SAFETY: reads this thread's last error, which ShellExecuteW set.
        return Err(if unsafe { GetLastError() } == ERROR_CANCELLED {
            "Unlock cancelled. Operations on disks need administrator permission.".into()
        } else {
            format!("Could not unlock: {}", io::Error::last_os_error()).into()
        });
    }
    Ok(())
}

fn require_elevation(task: &str) -> Result<()> {
    if elevated() {
        Ok(())
    } else {
        Err(format!("Unlock to {task}.").into())
    }
}

// ------------------------------------------------------------- device I/O

/// Open `path` for IOCTLs that need no access rights.
fn open_query(path: impl AsRef<Path>) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(SHARE)
        .open(path)
}

/// One IOCTL; returns the bytes it wrote to `out`.
fn ioctl(file: &File, code: u32, input: &[u8], out: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0;
    // SAFETY: both buffers are live for the lengths passed, and the handle
    // is synchronous, so no OVERLAPPED.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

/// An IOCTL whose output can be any size: retry with larger buffers.
fn ioctl_growing(file: &File, code: u32, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut size = 4096;
    loop {
        let mut out = vec![0u8; size];
        match ioctl(file, code, input, &mut out) {
            Ok(n) => {
                out.truncate(n);
                return Ok(out);
            }
            Err(error)
                if size < 1 << 20
                    && matches!(error.raw_os_error(), Some(ERROR_INSUFFICIENT_BUFFER | 234)) =>
            {
                size *= 4;
            }
            Err(error) => return Err(error),
        }
    }
}

/// A plain-old-data `T` at `offset` of `bytes`.
fn read<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    let end = offset.checked_add(size_of::<T>())?;
    let slice = bytes.get(offset..end)?;
    // SAFETY: the slice holds size_of::<T>() bytes, and the IOCTL structures
    // read this way are plain old data, valid for any bytes.
    Some(unsafe { std::ptr::read_unaligned(slice.as_ptr().cast::<T>()) })
}

// ------------------------------------------------------------- inventory

#[derive(Clone, Debug, Default)]
struct Disk {
    number: u32,
    name: String,
    bytes: u64,
    serial: Option<String>,
    bus: u32,
    style: &'static str,
    partitions: Vec<Partition>,
}

#[derive(Clone, Debug, Default)]
struct Partition {
    number: u32,
    offset: u64,
    bytes: u64,
    /// The GPT partition GUID, or the MBR disk signature and offset.
    id: Option<String>,
    /// EFI system, Microsoft reserved, or recovery.
    system: bool,
}

#[derive(Clone, Debug, Default)]
struct VolumeRecord {
    /// `\\?\Volume{вЂ¦}\`
    name: String,
    disk: u32,
    offset: u64,
    filesystem: String,
    label: String,
    mounts: Vec<PathBuf>,
}

/// What the CarryFS service reports.
#[derive(Debug, Default)]
struct Mounts {
    mounted: HashMap<String, PathBuf>,
    mounting: HashSet<String>,
    /// The label each mounted volume reports, by partition.
    labels: HashMap<String, String>,
}

pub fn device_path(id: &str) -> PathBuf {
    match parse_id(id) {
        Some((disk, Some(partition))) => partition_path(disk, partition),
        Some((disk, None)) => PathBuf::from(format!(r"\\.\PhysicalDrive{disk}")),
        None => PathBuf::from(id),
    }
}

fn partition_path(disk: u32, partition: u32) -> PathBuf {
    PathBuf::from(format!(r"\\.\Harddisk{disk}Partition{partition}"))
}

/// `diskN` or `diskNsM`, the identifiers macOS uses, as disk and partition numbers.
fn parse_id(id: &str) -> Option<(u32, Option<u32>)> {
    if !super::valid_id(id) {
        return None;
    }
    let rest = id.strip_prefix("disk")?;
    match rest.split_once('s') {
        Some((disk, partition)) => Some((disk.parse().ok()?, Some(partition.parse().ok()?))),
        None => Some((rest.parse().ok()?, None)),
    }
}

/// Disks numbered 0 to 63, as the CarryFS service probes them.
pub fn discover() -> Result<Vec<Device>> {
    let disks: Vec<Disk> = (0..64).filter_map(disk).collect();
    Ok(devices(&disks, &volumes(), &service_mounts(), &probe))
}

/// Read `device`'s disk again; the caller verifies it is the same device.
pub fn details(device: &Device) -> Result<Device> {
    let (number, _) = parse_id(&device.id).ok_or("Invalid device identifier")?;
    let disks: Vec<Disk> = disk(number).into_iter().collect();
    // Read only the selected partition's roots.
    let probe_one = |path: &Path, bytes| {
        if path == device.path() {
            probe(path, bytes)
        } else {
            None
        }
    };
    devices(&disks, &volumes(), &service_mounts(), &probe_one)
        .into_iter()
        .find(|d| d.id == device.id)
        .ok_or_else(|| {
            "The device changed or was reconnected. Refresh devices and select it again.".into()
        })
}

fn disk(number: u32) -> Option<Disk> {
    let file = open_query(format!(r"\\.\PhysicalDrive{number}")).ok()?;
    // A reader with no card answers no geometry; it has nothing to show.
    let mut geometry = [0u8; size_of::<DISK_GEOMETRY_EX>()];
    ioctl(&file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut geometry).ok()?;
    let geometry: DISK_GEOMETRY_EX = read(&geometry, 0)?;
    let mut disk = Disk {
        number,
        bytes: u64::try_from(geometry.DiskSize).ok()?,
        style: "raw",
        ..Disk::default()
    };
    describe(&file, &mut disk);
    // A disk no one has initialized has no layout.
    if let Ok(layout) = ioctl_growing(&file, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[]) {
        partitions(&layout, &mut disk);
    }
    Some(disk)
}

/// The disk's bus, name, and serial number.
fn describe(file: &File, disk: &mut Disk) {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // SAFETY: STORAGE_PROPERTY_QUERY is plain old data; its bytes are its fields.
    let input = unsafe {
        std::slice::from_raw_parts(
            (&raw const query).cast::<u8>(),
            size_of::<STORAGE_PROPERTY_QUERY>(),
        )
    };
    let mut out = vec![0u8; 1024];
    let Ok(n) = ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, input, &mut out) else {
        return;
    };
    out.truncate(n);
    let Some(descriptor) = read::<STORAGE_DEVICE_DESCRIPTOR>(&out, 0) else {
        return;
    };
    let string = |offset: u32| -> Option<String> {
        let start = offset as usize;
        if start == 0 || start >= out.len() {
            return None;
        }
        let end = out[start..].iter().position(|&b| b == 0)? + start;
        Some(String::from_utf8_lossy(&out[start..end]).trim().to_owned()).filter(|s| !s.is_empty())
    };
    disk.bus = descriptor.BusType as u32;
    disk.name = [
        string(descriptor.VendorIdOffset),
        string(descriptor.ProductIdOffset),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    disk.serial = string(descriptor.SerialNumberOffset);
}

fn partitions(layout: &[u8], disk: &mut Disk) {
    // The header's fields, read one by one: a layout with no partitions is
    // shorter than the structure, which declares one entry.
    let (Some(style), Some(count)) = (read::<u32>(layout, 0), read::<u32>(layout, 4)) else {
        return;
    };
    // The MBR member's first field; only MBR entries use it.
    let signature = read::<u32>(
        layout,
        std::mem::offset_of!(DRIVE_LAYOUT_INFORMATION_EX, Anonymous),
    )
    .unwrap_or(0);
    disk.style = match style as i32 {
        PARTITION_STYLE_GPT => "gpt",
        PARTITION_STYLE_MBR => "mbr",
        _ => "raw",
    };
    let entries = std::mem::offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    for index in 0..count as usize {
        let offset = entries + index * size_of::<PARTITION_INFORMATION_EX>();
        let Some(entry) = read::<PARTITION_INFORMATION_EX>(layout, offset) else {
            break;
        };
        let (id, system) = match entry.PartitionStyle {
            PARTITION_STYLE_GPT => {
                // SAFETY: a GPT entry's union holds its GPT member.
                let gpt = unsafe { entry.Anonymous.Gpt };
                if guid_value(&gpt.PartitionType) == 0 {
                    continue;
                }
                (
                    Some(guid(&gpt.PartitionId)),
                    SYSTEM_GPT_TYPES.contains(&guid_value(&gpt.PartitionType)),
                )
            }
            PARTITION_STYLE_MBR => {
                // SAFETY: an MBR entry's union holds its MBR member.
                let kind = unsafe { entry.Anonymous.Mbr.PartitionType };
                // Unused slots and extended-partition containers.
                if matches!(kind, 0 | 0x05 | 0x0f | 0x85) {
                    continue;
                }
                (
                    Some(format!(
                        "mbr-{signature:08x}-{:x}",
                        entry.StartingOffset as u64
                    )),
                    SYSTEM_MBR_TYPES.contains(&kind),
                )
            }
            _ => (None, false),
        };
        if entry.PartitionNumber == 0 {
            continue;
        }
        disk.partitions.push(Partition {
            number: entry.PartitionNumber,
            offset: entry.StartingOffset as u64,
            bytes: entry.PartitionLength as u64,
            id,
            system,
        });
    }
}

/// Every volume Windows mounted a file system on, with where it lies.
fn volumes() -> Vec<VolumeRecord> {
    let mut name = [0u16; 64];
    // SAFETY: the buffer is live for the length passed.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut volumes = Vec::new();
    loop {
        if let Some(volume) = volume(&from_wide(&name)) {
            volumes.push(volume);
        }
        // SAFETY: the search handle is open and the buffer is live.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: opened above and not used again.
    unsafe { FindVolumeClose(find) };
    volumes
}

fn volume(name: &str) -> Option<VolumeRecord> {
    // The volume device is the name without its trailing backslash.
    let file = open_query(name.trim_end_matches('\\')).ok()?;
    // One extent: a basic volume on one partition.
    let mut extents = [0u8; 32];
    ioctl(
        &file,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        &mut extents,
    )
    .ok()?;
    if read::<u32>(&extents, 0)? != 1 {
        return None;
    }
    let disk = read::<u32>(&extents, 8)?;
    let offset = read::<i64>(&extents, 16)? as u64;
    let root = wide(name);
    let (label, filesystem) = information(name).unwrap_or_default();
    let mut paths = vec![0u16; 1024];
    let mut needed = 0;
    // SAFETY: the root is NUL-terminated and the buffer is live for its length.
    let listed = unsafe {
        GetVolumePathNamesForVolumeNameW(
            root.as_ptr(),
            paths.as_mut_ptr(),
            paths.len() as u32,
            &mut needed,
        )
    } != 0;
    Some(VolumeRecord {
        name: name.to_owned(),
        disk,
        offset,
        filesystem,
        label,
        mounts: if listed {
            paths
                .split(|&c| c == 0)
                .take_while(|path| !path.is_empty())
                .map(|path| PathBuf::from(String::from_utf16_lossy(path)))
                .collect()
        } else {
            Vec::new()
        },
    })
}

/// The label and file system name of the volume at `root`, a path ending in
/// a backslash.
fn information(root: &str) -> Option<(String, String)> {
    let root = wide(root);
    let (mut label, mut filesystem) = ([0u16; 261], [0u16; 261]);
    // SAFETY: the root is NUL-terminated and both buffers are live for the
    // lengths passed; the other outputs are optional.
    let known = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } != 0;
    known.then(|| (from_wide(&label), from_wide(&filesystem)))
}

/// The partition's CarryFS name, when its roots carry the CarryFS magic:
/// `Some(None)` for a volume too damaged to name. Only an administrator can
/// read a partition. Reading shares the partition, so the service's exclusive
/// open would fail if it started at the same moment; the service mounts on
/// arrival, before the manager lists the new disk.
fn probe(path: &Path, bytes: u64) -> Option<Option<String>> {
    if !elevated() {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .share_mode(SHARE)
        .open(path)
        .ok()?;
    let mut device = ImageDevice::from_device_file(file, bytes).ok()?;
    if !carryfs_core::has_root_magic(&mut device) {
        return None;
    }
    Some(
        Volume::open(device)
            .and_then(|volume| volume.info())
            .map(|info| info.name())
            .ok(),
    )
}

/// The manager's records for `disks`. `probe` names a CarryFS partition at a
/// path of a byte length, as [`probe`] does.
fn devices(
    disks: &[Disk],
    volumes: &[VolumeRecord],
    mounts: &Mounts,
    probe: &dyn Fn(&Path, u64) -> Option<Option<String>>,
) -> Vec<Device> {
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\";
    let system_disks: HashSet<u32> = volumes
        .iter()
        .filter(|v| v.mounts.iter().any(|m| m == Path::new(&system_drive)))
        .map(|v| v.disk)
        .collect();
    let mut devices = Vec::new();
    for disk in disks {
        let kind = match disk.bus {
            // iSCSI, virtual, file-backed virtual (VHD), and Storage Spaces.
            9 | 14 | 15 | 16 => DeviceKind::Virtual,
            0 => DeviceKind::Unknown,
            _ => DeviceKind::Physical,
        };
        // 1394, USB, SD, and MMC.
        let external = matches!(disk.bus, 4 | 7 | 12 | 13);
        let internal = !external || system_disks.contains(&disk.number);
        let whole_id = format!("disk{}", disk.number);
        devices.push(Device {
            id: whole_id.clone(),
            parent: None,
            name: if disk.name.is_empty() {
                whole_id.clone()
            } else {
                disk.name.clone()
            },
            bytes: disk.bytes,
            uuid: disk.serial.clone(),
            filesystem: disk.style.into(),
            mount: None,
            whole: true,
            internal,
            kind,
        });
        for partition in &disk.partitions {
            let id = format!("disk{}s{}", disk.number, partition.number);
            let path = partition_path(disk.number, partition.number);
            let key = path.display().to_string();
            let volume = volumes
                .iter()
                .find(|v| v.disk == disk.number && v.offset == partition.offset);
            let served = mounts.mounted.get(&key);
            let carryfs = served.is_some() || mounts.mounting.contains(&key);
            let windows_fs = volume.filter(|v| !v.filesystem.is_empty());
            let named = if carryfs || windows_fs.is_some() || partition.system {
                None
            } else {
                probe(&path, partition.bytes)
            };
            let filesystem = if carryfs || named.is_some() {
                "carryfs".to_owned()
            } else {
                windows_fs.map_or_else(|| "Unknown".into(), |v| v.filesystem.to_ascii_lowercase())
            };
            let name = named
                .flatten()
                .or_else(|| mounts.labels.get(&key).cloned())
                .or_else(|| windows_fs.map(|v| v.label.clone()))
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| id.clone());
            devices.push(Device {
                id,
                parent: Some(whole_id.clone()),
                name,
                bytes: partition.bytes,
                uuid: partition.id.clone(),
                filesystem,
                mount: served
                    .cloned()
                    .or_else(|| volume.and_then(|v| v.mounts.first().cloned())),
                whole: false,
                internal: internal || partition.system,
                kind,
            });
        }
    }
    devices
}

// ------------------------------------------------------------- service

/// Send one request to the CarryFS service and return the lines after `ok`.
fn service(request: &str) -> Result<Vec<String>> {
    let mut pipe = None;
    for _ in 0..40 {
        match OpenOptions::new().read(true).write(true).open(PIPE) {
            Ok(file) => {
                pipe = Some(file);
                break;
            }
            // Every instance is answering; the service makes another at once.
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(
                    "The CarryFS service isn't running, or it predates this manager. Start it in Services, or update CarryFS from Software."
                        .into(),
                );
            }
            Err(error) => return Err(format!("CarryFS service: {error}").into()),
        }
    }
    let mut pipe = pipe.ok_or("The CarryFS service is busy. Try again.")?;
    pipe.write_all(format!("{request}\n").as_bytes())?;
    let mut reply = String::new();
    pipe.read_to_string(&mut reply)?;
    parse_reply(&reply)
}

fn parse_reply(reply: &str) -> Result<Vec<String>> {
    let mut lines = reply.lines();
    match lines.next() {
        Some(first) if first == "ok" || first.starts_with("ok ") => Ok(first
            .strip_prefix("ok ")
            .into_iter()
            .chain(lines)
            .map(str::to_owned)
            .collect()),
        Some(first) if first.starts_with("error ") => {
            Err(first["error ".len()..].to_owned().into())
        }
        _ => Err("The CarryFS service sent no reply.".into()),
    }
}

fn parse_mounts(lines: &[String]) -> Mounts {
    let mut mounts = Mounts::default();
    for line in lines {
        match line.split('\t').collect::<Vec<_>>()[..] {
            ["mounted", volume, point] => {
                mounts
                    .mounted
                    .insert(volume.to_owned(), PathBuf::from(format!("{point}\\")));
            }
            ["mounting", volume] => {
                mounts.mounting.insert(volume.to_owned());
            }
            _ => {}
        }
    }
    mounts
}

/// The service's mounts; none while it isn't running, which is logged once.
fn service_mounts() -> Mounts {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    match service("list") {
        Ok(lines) => {
            let mut mounts = parse_mounts(&lines);
            mounts.labels = mounts
                .mounted
                .iter()
                .filter_map(|(volume, point)| {
                    let (label, _) = information(&point.to_string_lossy())?;
                    Some((volume.clone(), label))
                })
                .collect();
            mounts
        }
        Err(error) => {
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("CarryFS Manager: listing CarryFS mounts: {error}");
            }
            Mounts::default()
        }
    }
}

// ------------------------------------------------------------- tasks

/// The partition, read-only, held so that nothing else opens it, the
/// service's mount included, until the file closes.
pub fn device_file(device: &Device) -> Result<File> {
    require_elevation("read disks directly")?;
    OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(device.path())
        .map_err(|error| match error.raw_os_error() {
            Some(ERROR_SHARING_VIOLATION) => {
                "The volume is in use. Unmount it, or close programs using it.".into()
            }
            _ => format!("{}: {error}", device.path().display()).into(),
        })
}

pub fn act(fresh: &Device, action: DeviceAction) -> Result<String> {
    match action {
        DeviceAction::Eject => eject(fresh),
        DeviceAction::Mount if fresh.carryfs() => {
            let point = service(&format!("mount {}", fresh.path().display()))?;
            Ok(format!("Mounted at {}", point.join(" ")))
        }
        DeviceAction::Unmount if fresh.carryfs() => {
            service(&format!("unmount {}", fresh.path().display()))?;
            Ok("Unmounted".into())
        }
        DeviceAction::Mount => {
            require_elevation("mount volumes")?;
            let volume = volume_of(fresh)?;
            let letter = free_letter().ok_or("No drive letter is free.")?;
            let (point, name) = (wide(&letter), wide(&volume.name));
            // SAFETY: both strings are NUL-terminated.
            if unsafe { SetVolumeMountPointW(point.as_ptr(), name.as_ptr()) } == 0 {
                return Err(format!("Mount: {}", io::Error::last_os_error()).into());
            }
            Ok(format!("Mounted at {letter}"))
        }
        DeviceAction::Unmount => {
            require_elevation("unmount volumes")?;
            let volume = volume_of(fresh)?;
            // Dismounting needs the volume to itself, so busy volumes stay mounted.
            let file = lock(Path::new(volume.name.trim_end_matches('\\')))?;
            for mount in &volume.mounts {
                let point = wide(mount);
                // SAFETY: the mount point is NUL-terminated.
                if unsafe { DeleteVolumeMountPointW(point.as_ptr()) } == 0 {
                    return Err(format!(
                        "Unmount {}: {}",
                        mount.display(),
                        io::Error::last_os_error()
                    )
                    .into());
                }
            }
            drop(file);
            Ok("Unmounted".into())
        }
    }
}

/// The Windows volume on `device`'s partition.
fn volume_of(device: &Device) -> Result<VolumeRecord> {
    let (number, Some(partition)) = parse_id(&device.id).ok_or("Invalid device identifier")? else {
        return Err("Select a partition.".into());
    };
    let offset = disk(number)
        .and_then(|d| d.partitions.into_iter().find(|p| p.number == partition))
        .ok_or("The partition is no longer there. Refresh devices.")?
        .offset;
    volumes()
        .into_iter()
        .find(|v| v.disk == number && v.offset == offset && !v.filesystem.is_empty())
        .ok_or_else(|| "Windows recognizes no file system on this partition.".into())
}

/// The lowest free drive letter from D:, as `D:\`.
fn free_letter() -> Option<String> {
    // SAFETY: GetLogicalDrives takes no arguments.
    let used = unsafe { GetLogicalDrives() };
    (b'D'..=b'Z')
        .find(|letter| used & (1 << (letter - b'A')) == 0)
        .map(|letter| format!("{}:\\", letter as char))
}

/// Open `volume` for writing and lock and dismount it, so that nothing else
/// reads or writes it until the returned file closes.
fn lock(volume: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(SHARE)
        .open(volume)
        .map_err(|e| format!("{}: {e}", volume.display()))?;
    ioctl(&file, FSCTL_LOCK_VOLUME, &[], &mut []).map_err(|e| {
        format!("The volume is in use. Close programs using it and try again. ({e})")
    })?;
    ioctl(&file, FSCTL_DISMOUNT_VOLUME, &[], &mut [])
        .map_err(|e| format!("Dismount {}: {e}", volume.display()))?;
    Ok(file)
}

fn eject(fresh: &Device) -> Result<String> {
    let (number, _) = parse_id(&fresh.id).ok_or("Invalid device identifier")?;
    if fresh.kind == DeviceKind::Virtual {
        require_elevation("eject disk images")?;
        powershell(&format!(
            r"Dismount-DiskImage -DevicePath '\\.\PhysicalDrive{number}' | Out-Null"
        ))?;
        return Ok("Ejected".into());
    }
    let instance = instance_id(number)?;
    let instance = wide(&instance);
    let mut node = 0;
    // SAFETY: the instance ID is NUL-terminated and the node is an out parameter.
    if unsafe { CM_Locate_DevNodeW(&mut node, instance.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) }
        != CR_SUCCESS
    {
        return Err("The disk is no longer connected.".into());
    }
    // Eject the removable device the disk belongs to, such as its USB device.
    let mut target = node;
    loop {
        let (mut status, mut problem) = (0, 0);
        // SAFETY: both outputs are live.
        let known =
            unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, target, 0) } == CR_SUCCESS;
        if known && status & DN_REMOVABLE != 0 {
            break;
        }
        let mut parent = 0;
        // SAFETY: the output is live.
        if unsafe { CM_Get_Parent(&mut parent, target, 0) } != CR_SUCCESS {
            target = node;
            break;
        }
        target = parent;
    }
    let mut veto = PNP_VetoTypeUnknown;
    let mut name = [0u16; 260];
    // SAFETY: the veto outputs are live for the length passed.
    let result = unsafe {
        CM_Request_Device_EjectW(target, &mut veto, name.as_mut_ptr(), name.len() as u32, 0)
    };
    if result != CR_SUCCESS || veto != PNP_VetoTypeUnknown {
        let by = from_wide(&name);
        return Err(format!(
            "Windows didn't eject the disk{}. Close files on it; if CarryFS was saving changes, try again in a moment.",
            if by.is_empty() { String::new() } else { format!(": {by} is in use") }
        )
        .into());
    }
    Ok("Ejected".into())
}

/// The Plug and Play instance ID of disk `number`.
fn instance_id(number: u32) -> Result<String> {
    let mut length = 0;
    // SAFETY: the class GUID is a constant and the length is an out parameter.
    if unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut length,
            &GUID_DEVINTERFACE_DISK,
            std::ptr::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    } != CR_SUCCESS
    {
        return Err("Cannot list disks".into());
    }
    let mut list = vec![0u16; length as usize];
    // SAFETY: the buffer holds the length the size call returned.
    if unsafe {
        CM_Get_Device_Interface_ListW(
            &GUID_DEVINTERFACE_DISK,
            std::ptr::null(),
            list.as_mut_ptr(),
            length,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    } != CR_SUCCESS
    {
        return Err("Cannot list disks".into());
    }
    for interface in list.split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let path = String::from_utf16_lossy(interface);
        let Ok(file) = open_query(&path) else {
            continue;
        };
        let mut out = [0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
        if ioctl(&file, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out).is_err()
            || read::<STORAGE_DEVICE_NUMBER>(&out, 0).map(|n| n.DeviceNumber) != Some(number)
        {
            continue;
        }
        let interface = wide(&path);
        let mut kind: DEVPROPTYPE = 0;
        let mut value = vec![0u16; 512];
        let mut size = (value.len() * 2) as u32;
        // SAFETY: the interface is NUL-terminated and the buffer is live for
        // `size` bytes.
        if unsafe {
            CM_Get_Device_Interface_PropertyW(
                interface.as_ptr(),
                &DEVPKEY_Device_InstanceId,
                &mut kind,
                value.as_mut_ptr().cast(),
                &mut size,
                0,
            )
        } == CR_SUCCESS
        {
            return Ok(from_wide(&value));
        }
    }
    Err("The disk is no longer connected.".into())
}

/// Erase a partition, or a whole disk into one GPT partition, as CarryFS
/// named `label`, then ask the service to mount it.
pub fn erase(fresh: &Device, label: VolumeLabel) -> Result<String> {
    require_elevation("erase disks")?;
    let (number, partition) = parse_id(&fresh.id).ok_or("Invalid device identifier")?;
    let partition = match partition {
        Some(partition) => {
            // No drive letter, and hidden, so Windows offers no format prompt.
            // Only basic data partitions take these attributes.
            powershell(&format!(
                "$p = Get-Partition -DiskNumber {number} -PartitionNumber {partition}\n\
                 if ((Get-Disk -Number {number}).PartitionStyle -eq 'GPT' -and $p.GptType -eq '{BASIC_DATA}') {{\n\
                 $p | Set-Partition -NoDefaultDriveLetter $true\n\
                 $p | Set-Partition -IsHidden $true\n}}"
            ))?;
            partition
        }
        None => {
            // Clearing the disk deletes partitions the service holds open.
            for (volume, _) in service_mounts().mounted {
                if volume.starts_with(&format!(r"\\.\Harddisk{number}Partition")) {
                    service(&format!("unmount {volume}"))?;
                }
            }
            powershell(&format!(
                "$d = Get-Disk -Number {number}\n\
                 if ($d.PartitionStyle -ne 'RAW') {{ Clear-Disk -Number {number} -RemoveData -RemoveOEM -Confirm:$false }}\n\
                 Initialize-Disk -Number {number} -PartitionStyle GPT\n\
                 $p = New-Partition -DiskNumber {number} -UseMaximumSize -GptType '{BASIC_DATA}' -IsHidden\n\
                 $p | Set-Partition -NoDefaultDriveLetter $true\n\
                 $p.PartitionNumber"
            ))?
            .parse()
            .map_err(|_| "Windows didn't report the new partition.")?
        }
    };
    let path = partition_path(number, partition);
    format(&path, label)?;
    Ok(match service(&format!("mount {}", path.display())) {
        Ok(point) => format!(
            "Erased {} as CarryFS. Mounted at {}.",
            path.display(),
            point.join(" ")
        ),
        Err(error) => format!(
            "Erased {} as CarryFS. It isn't mounted: {error}",
            path.display()
        ),
    })
}

/// Format the partition at `path` as CarryFS, whatever it held.
fn format(path: &Path, label: VolumeLabel) -> Result<()> {
    let file = lock(path)?;
    // A handle opened while a file system was mounted keeps its I/O inside
    // that file system's volume, which can end before the partition does.
    match ioctl(&file, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut []) {
        Ok(_) => {}
        // RAW, which holds a volume no file system recognized, has no such
        // limit and refuses the request.
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER) => {}
        Err(error) => return Err(format!("{}: {error}", path.display()).into()),
    }
    let mut length = [0u8; 8];
    ioctl(&file, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut length)?;
    let bytes = u64::from_le_bytes(length);
    let mut device = ImageDevice::from_exclusive_device_file(file, bytes, MountMode::ReadWrite)?;
    carryfs_core::erase_reserved_bands(&mut device)?;
    // Closing the device unlocks the volume.
    Volume::format_with_label(device, label)?.eject()?;
    Ok(())
}

/// Let an unmounted CarryFS partition store permissions and the hidden flag,
/// with the installed adapter.
pub fn upgrade(fresh: &Device) -> Result<String> {
    require_elevation("upgrade disks")?;
    Ok(String::from_utf8_lossy(&output(
        command(installed().join("carryfs-windows.exe"))
            .arg("upgrade")
            .arg(fresh.path()),
    )?)
    .trim()
    .to_owned())
}

/// Install a CarryFS release with the published installer, which verifies
/// the package and asks for administrator permission through UAC.
pub fn install(version: &str) -> Result<String> {
    // The caller checked that `version` is X.Y.Z.
    powershell(&format!(
        "[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12\n\
         $installer = Invoke-RestMethod -Uri 'https://carryfs.com/install.ps1' -UseBasicParsing -TimeoutSec 300\n\
         & ([scriptblock]::Create($installer)) -Version '{version}' -Yes"
    ))
}

// ------------------------------------------------------------- watching

type Changed = Mutex<Box<dyn Fn() + Send>>;

/// Calls `changed` on a background thread whenever a disk or a partition's
/// volume arrives or leaves, or a drive letter appears or goes, as when the
/// service mounts a volume.
pub fn watch(changed: impl Fn() + Send + 'static) -> Result<()> {
    // Kept for the process lifetime: the notification may call it at any time.
    let changed: Box<dyn Fn() + Send> = Box::new(changed);
    let changed: &'static Changed = Box::leak(Box::new(Mutex::new(changed)));
    for class in [GUID_DEVINTERFACE_DISK, GUID_DEVINTERFACE_VOLUME] {
        // SAFETY: CM_NOTIFY_FILTER is plain old data; all-zero is valid.
        let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
        filter.cbSize = size_of::<CM_NOTIFY_FILTER>() as u32;
        filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
        filter.u.DeviceInterface.ClassGuid = class;
        let mut notification: HCMNOTIFICATION = std::ptr::null_mut();
        // SAFETY: the filter is initialized and the context lives for the process.
        let result = unsafe {
            CM_Register_Notification(
                &filter,
                std::ptr::from_ref(changed).cast(),
                Some(on_disk),
                &mut notification,
            )
        };
        if result != CR_SUCCESS {
            return Err(format!("disk notifications unavailable (CONFIGRET {result})").into());
        }
    }
    std::thread::spawn(move || {
        // SAFETY: GetLogicalDrives takes no arguments.
        let mut letters = unsafe { GetLogicalDrives() };
        loop {
            std::thread::sleep(Duration::from_secs(1));
            // SAFETY: as above.
            let now = unsafe { GetLogicalDrives() };
            if now != letters {
                letters = now;
                notify(changed);
            }
        }
    });
    Ok(())
}

fn notify(changed: &Changed) {
    match changed.lock() {
        Ok(changed) => changed(),
        Err(_) => eprintln!("CarryFS Manager: disk watching stopped; use Refresh"),
    }
}

unsafe extern "system" fn on_disk(
    _: HCMNOTIFICATION,
    context: *const std::ffi::c_void,
    action: CM_NOTIFY_ACTION,
    _: *const CM_NOTIFY_EVENT_DATA,
    _: u32,
) -> u32 {
    if matches!(
        action,
        CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL | CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
    ) {
        // SAFETY: `watch` registered a context that lives for the process.
        notify(unsafe { &*context.cast::<Changed>() });
    }
    NO_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_disk() -> Disk {
        Disk {
            number: 3,
            name: "OCZ VERTEX4".into(),
            bytes: 128 << 30,
            serial: Some("SER1".into()),
            bus: 7,
            style: "gpt",
            partitions: vec![
                Partition {
                    number: 1,
                    offset: 1 << 20,
                    bytes: 16 << 20,
                    id: Some("msr".into()),
                    system: true,
                },
                Partition {
                    number: 2,
                    offset: 17 << 20,
                    bytes: 8 << 30,
                    id: Some("part-2".into()),
                    system: false,
                },
                Partition {
                    number: 3,
                    offset: 9 << 30,
                    bytes: 8 << 30,
                    id: Some("part-3".into()),
                    system: false,
                },
                Partition {
                    number: 4,
                    offset: 17 << 30,
                    bytes: 8 << 30,
                    id: Some("part-4".into()),
                    system: false,
                },
            ],
        }
    }

    #[test]
    fn partitions_show_windows_file_systems_service_mounts_and_probed_volumes() {
        let volumes = [VolumeRecord {
            name: r"\\?\Volume{1}\".into(),
            disk: 3,
            offset: 9 << 30,
            filesystem: "exFAT".into(),
            label: "Photos".into(),
            mounts: vec![PathBuf::from(r"F:\")],
        }];
        let mut mounts = parse_mounts(&[r"mounted	\\.\Harddisk3Partition2	E:".to_owned()]);
        mounts
            .labels
            .insert(r"\\.\Harddisk3Partition2".into(), "Travel".into());
        let probed = |path: &Path, _| {
            (path == Path::new(r"\\.\Harddisk3Partition4")).then(|| Some("Backup".to_owned()))
        };
        let devices = devices(&[usb_disk()], &volumes, &mounts, &probed);
        assert_eq!(devices.len(), 5);
        let disk = &devices[0];
        assert!(disk.whole && !disk.internal && disk.kind == DeviceKind::Physical);
        assert_eq!(disk.uuid.as_deref(), Some("SER1"));
        // The reserved partition stays hidden and can't be erased.
        assert!(devices[1].protected() && !devices[1].visible_by_default());
        let served = &devices[2];
        assert!(served.carryfs());
        assert_eq!(served.name, "Travel");
        assert_eq!(served.mount.as_deref(), Some(Path::new(r"E:\")));
        assert_eq!(served.path(), Path::new(r"\\.\Harddisk3Partition2"));
        let exfat = &devices[3];
        assert_eq!(
            (exfat.format_name(), exfat.name.as_str()),
            ("exFAT", "Photos")
        );
        assert_eq!(exfat.mount.as_deref(), Some(Path::new(r"F:\")));
        assert!(exfat.external_partition());
        let probed = &devices[4];
        assert!(probed.carryfs() && probed.mount.is_none());
        assert_eq!(probed.name, "Backup");
    }

    #[test]
    fn internal_virtual_and_system_disks_are_protected() {
        let mut sata = usb_disk();
        sata.bus = 11;
        let mut vhd = usb_disk();
        vhd.number = 4;
        vhd.bus = 15;
        let mut system = usb_disk();
        system.number = 5;
        let volumes = [VolumeRecord {
            disk: 5,
            mounts: vec![PathBuf::from(
                std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\",
            )],
            filesystem: "NTFS".into(),
            ..VolumeRecord::default()
        }];
        let devices = devices(
            &[sata, vhd, system],
            &volumes,
            &Mounts::default(),
            &|_, _| None,
        );
        let disk = |id: &str| devices.iter().find(|d| d.id == id).unwrap();
        assert!(disk("disk3").internal && !disk("disk3").erasable());
        assert_eq!(disk("disk4").kind, DeviceKind::Virtual);
        assert!(!disk("disk4").erasable() && !disk("disk4s2").erasable());
        assert!(disk("disk5").protected() && disk("disk5s2").protected());
    }

    #[test]
    fn identifiers_map_to_windows_device_paths() {
        assert_eq!(parse_id("disk3s12"), Some((3, Some(12))));
        assert_eq!(parse_id("disk3"), Some((3, None)));
        assert_eq!(parse_id("disk3;x"), None);
        assert_eq!(device_path("disk3"), Path::new(r"\\.\PhysicalDrive3"));
        assert_eq!(
            device_path("disk3s1"),
            Path::new(r"\\.\Harddisk3Partition1")
        );
    }

    #[test]
    fn service_replies_carry_results_or_errors() {
        assert_eq!(parse_reply("ok E:\n").unwrap(), ["E:"]);
        assert!(parse_reply("ok\n").unwrap().is_empty());
        assert_eq!(
            parse_reply("error disk is busy\n").unwrap_err().to_string(),
            "disk is busy"
        );
        assert!(parse_reply("").is_err());
        let mounts = parse_mounts(
            &parse_reply("ok\nmounted\t\\\\.\\Harddisk1Partition2\tE:\nmounting\t\\\\.\\Harddisk2Partition1\n")
                .unwrap(),
        );
        assert_eq!(
            mounts.mounted.get(r"\\.\Harddisk1Partition2"),
            Some(&PathBuf::from(r"E:\"))
        );
        assert!(mounts.mounting.contains(r"\\.\Harddisk2Partition1"));
    }

    #[test]
    fn base64_matches_powershell_encoded_commands() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    #[ignore = "Reads this PC's disk inventory and, as administrator, the roots of partitions with no Windows file system; never writes"]
    fn discovered_devices_can_be_reidentified() {
        let devices = discover().unwrap();
        for device in &devices {
            let fresh = details(device).unwrap_or_else(|e| panic!("{}: {e}", device.id));
            device.verify(&fresh).unwrap();
            println!(
                "{} {:?} {} {} {:?} internal={} {:?}",
                device.id,
                device.name,
                device.format_name(),
                device.bytes,
                device.mount,
                device.internal,
                device.kind
            );
        }
        assert!(!devices.is_empty());
    }

    /// The disk-image disk `CARRYFS_TEST_DISK` names, refusing any other kind.
    fn test_disk() -> u32 {
        let number = std::env::var("CARRYFS_TEST_DISK")
            .expect("Set CARRYFS_TEST_DISK to the number of a disposable VHD")
            .parse()
            .unwrap();
        let disk = disk(number).expect("No such disk");
        assert_eq!(
            disk.bus, 15,
            "Disk {number} is not a VHD; refusing to erase it"
        );
        number
    }

    fn find(id: &str) -> Device {
        discover()
            .unwrap()
            .into_iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("{id} is missing"))
    }

    #[test]
    #[ignore = "Erases the VHD CARRYFS_TEST_DISK names; needs administrator rights and the CarryFS service with its control pipe"]
    fn a_vhd_erases_mounts_reads_and_ejects() {
        let number = test_disk();
        let label = |name: &str| VolumeLabel::parse(name).unwrap();
        // A volume Windows formatted: its drive letter goes and comes back.
        let created = powershell(&format!(
            "Initialize-Disk -Number {number} -PartitionStyle GPT\n\
             $p = New-Partition -DiskNumber {number} -UseMaximumSize -AssignDriveLetter\n\
             $p | Format-Volume -FileSystem NTFS -NewFileSystemLabel Before -Confirm:$false | Out-Null\n\
             $p.PartitionNumber"
        ))
        .unwrap();
        let id = format!("disk{number}s{created}");
        let ntfs = find(&id);
        assert_eq!((ntfs.format_name(), ntfs.name.as_str()), ("NTFS", "Before"));
        // A disk image is virtual, so only the mount is checked here.
        assert!(ntfs.mount.is_some());
        act(&ntfs, DeviceAction::Unmount).unwrap();
        assert!(find(&id).mount.is_none());
        println!("{}", act(&find(&id), DeviceAction::Mount).unwrap());
        assert!(find(&id).mount.is_some());
        act(&find(&id), DeviceAction::Unmount).unwrap();

        // Erasing it dismounts NTFS; the service mounts the new volume.
        println!("{}", erase(&find(&id), label("Whole")).unwrap());
        let partition = find(&id);
        assert!(partition.carryfs());
        let point = partition
            .mount
            .clone()
            .expect("The service didn't mount it");
        std::fs::write(point.join("hello.txt"), b"hello from Windows").unwrap();
        assert!(
            act(&partition, DeviceAction::Unmount)
                .unwrap()
                .contains("Unmounted")
        );
        let unmounted = find(&partition.id);
        assert!(unmounted.carryfs() && unmounted.mount.is_none());
        assert_eq!(unmounted.name, "Whole");
        partition.verify(&unmounted).unwrap();

        // Read it as First Aid does: in this process, then through the CLI.
        let browser = crate::model::Browser::open_device(unmounted.clone()).unwrap();
        assert!(
            browser
                .entries
                .iter()
                .any(|e| e.item.name.stored() == b"hello.txt")
        );
        let control = std::sync::Arc::new(crate::operation::Control::default());
        let report = crate::operation::inspect(browser.clone(), true, control.clone()).unwrap();
        assert!(report.contains("0 unreadable ranges"), "{report}");
        let plan = crate::operation::fsck(browser.source(), crate::operation::Fsck::Plan, control)
            .unwrap();
        assert!(plan.contains("clean: true"), "{plan}");

        // The open partition is held: the service can't mount it meanwhile.
        let held = device_file(&unmounted).unwrap();
        assert!(device_file(&unmounted).is_err());
        drop(held);
        let mounted = act(&unmounted, DeviceAction::Mount).unwrap();
        println!("{mounted}");
        let remounted = find(&unmounted.id);
        let point = remounted.mount.clone().expect("Mount reported no point");
        assert_eq!(
            std::fs::read(point.join("hello.txt")).unwrap(),
            b"hello from Windows"
        );
        act(&remounted, DeviceAction::Unmount).unwrap();

        // The whole disk becomes one hidden GPT partition, which the service mounts.
        println!(
            "{}",
            erase(&find(&format!("disk{number}")), label("Again")).unwrap()
        );
        let again = discover()
            .unwrap()
            .into_iter()
            .find(|d| d.parent.as_deref() == Some(&format!("disk{number}")) && d.carryfs())
            .expect("No CarryFS partition after erasing the disk");
        let point = again.mount.clone().expect("The service didn't mount it");
        assert!(!point.join("hello.txt").exists());

        act(&find(&format!("disk{number}")), DeviceAction::Eject).unwrap();
        assert!(disk(number).is_none(), "The VHD is still attached");
    }
}
