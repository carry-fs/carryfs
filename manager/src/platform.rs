use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Physical,
    Virtual,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub bytes: u64,
    pub uuid: Option<String>,
    pub filesystem: String,
    pub mount: Option<PathBuf>,
    pub whole: bool,
    pub internal: bool,
    pub kind: DeviceKind,
}
impl Device {
    pub fn carryfs(&self) -> bool {
        self.filesystem.eq_ignore_ascii_case("carryfs")
    }
    pub fn external_partition(&self) -> bool {
        !self.protected()
            && self.kind == DeviceKind::Physical
            && !self.whole
            && self.uuid.is_some()
            && !self.filesystem.to_ascii_lowercase().contains("apfs")
    }
    /// An external physical disk, or a partition on one.
    pub fn erasable(&self) -> bool {
        self.external_partition()
            || (self.whole && !self.protected() && self.kind == DeviceKind::Physical)
    }
    /// Physical disks, internal ones included, and their user partitions;
    /// containers, system volumes, and disk images stay hidden.
    pub fn visible_by_default(&self) -> bool {
        if self.carryfs() {
            return true;
        }
        if self.kind == DeviceKind::Virtual {
            return false;
        }
        if self.whole {
            return self.parent.is_none();
        }
        if self.protected() {
            return false;
        }
        let fs = self.filesystem.to_ascii_lowercase();
        !fs.contains("efi") && !fs.contains("apple_apfs") && !fs.contains("partition_scheme")
    }
    pub fn format_name(&self) -> &str {
        match self.filesystem.to_ascii_lowercase().as_str() {
            "carryfs" => "CarryFS",
            "apfs" => "APFS",
            "exfat" => "exFAT",
            "hfs" | "hfs+" => "HFS+",
            "msdos" => "FAT",
            _ => "Unknown format",
        }
    }
    pub fn protected(&self) -> bool {
        self.internal
            || self.mount.as_ref().is_some_and(|p| {
                p == Path::new("/") || p.starts_with("/System") || p.starts_with("/private/var")
            })
    }
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/dev/{}", self.id))
    }
    pub fn verify(&self, fresh: &Self) -> Result<()> {
        if self.id != fresh.id
            || self
                .uuid
                .as_ref()
                .is_some_and(|uuid| fresh.uuid.as_ref() != Some(uuid))
            || self.bytes != fresh.bytes
            || self.whole != fresh.whole
        {
            return Err(
                "The device changed or was reconnected. Refresh devices and select it again."
                    .into(),
            );
        }
        Ok(())
    }
}

pub fn output(command: &mut Command) -> Result<Vec<u8>> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "{}: {}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .trim_matches(':')
        .trim()
        .to_owned()
        .into());
    }
    Ok(output.stdout)
}

#[cfg(target_os = "macos")]
fn diskutil(args: &[&str]) -> Result<Value> {
    let xml = output(Command::new("/usr/sbin/diskutil").args(args))?;
    let mut child = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("Cannot send disk information to plutil")?
        .write_all(&xml)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}
fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
pub fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("disk") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .split('s')
            .all(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
}
fn parse_device(v: &Value, parent: Option<String>, whole: bool) -> Result<Device> {
    let id = string(v, "DeviceIdentifier").ok_or("Disk information is missing an identifier")?;
    if !valid_id(&id) {
        return Err("Invalid device identifier in disk information".into());
    }
    Ok(Device {
        name: string(v, "VolumeName")
            .or_else(|| string(v, "MediaName"))
            .unwrap_or_else(|| id.clone()),
        id,
        parent,
        // `Size` is the media size in both `list` and `info`; `info`'s
        // `TotalSize` is the mounted filesystem's, which can be smaller.
        bytes: v
            .get("Size")
            .or_else(|| v.get("TotalSize"))
            .and_then(Value::as_u64)
            .ok_or("Missing device size")?,
        uuid: string(v, "DiskUUID").or_else(|| string(v, "VolumeUUID")),
        filesystem: string(v, "FilesystemType")
            .or_else(|| string(v, "FilesystemName"))
            .or_else(|| string(v, "Content"))
            .unwrap_or_else(|| "Unknown".into()),
        mount: string(v, "MountPoint").map(PathBuf::from),
        whole,
        internal: v
            .get("Internal")
            .or_else(|| v.get("OSInternal"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        kind: match v.get("VirtualOrPhysical").and_then(Value::as_str) {
            Some("Physical") => DeviceKind::Physical,
            Some("Virtual") => DeviceKind::Virtual,
            _ => DeviceKind::Unknown,
        },
    })
}
fn parse_inventory(root: &Value) -> Result<Vec<Device>> {
    let disks = root
        .get("AllDisksAndPartitions")
        .and_then(Value::as_array)
        .ok_or("Disk inventory has no disk list")?;
    let mut devices = Vec::new();
    for disk in disks {
        let mut parent = parse_device(disk, None, true)?;
        if let Some(stores) = disk.get("APFSPhysicalStores").and_then(Value::as_array) {
            parent.kind = DeviceKind::Virtual;
            parent.parent = stores
                .first()
                .and_then(|store| string(store, "DeviceIdentifier"));
        }
        let parent_id = parent.id.clone();
        devices.push(parent);
        for key in ["Partitions", "APFSVolumes"] {
            if let Some(parts) = disk.get(key).and_then(Value::as_array) {
                for part in parts {
                    let mut volume = parse_device(part, Some(parent_id.clone()), false)?;
                    volume.filesystem = if key == "APFSVolumes" {
                        "apfs".into()
                    } else {
                        volume.filesystem
                    };
                    devices.push(volume);
                }
            }
        }
    }
    Ok(devices)
}

pub fn discover() -> Result<Vec<Device>> {
    #[cfg(target_os = "macos")]
    {
        let devices = parse_inventory(&diskutil(&["list", "-plist"])?)?;
        devices.iter().map(details).collect()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("System-device discovery is currently available on macOS. Image files work on this platform.".into())
    }
}

/// Calls `changed` on a background thread whenever a disk appears, disappears,
/// mounts, or unmounts. An orphaned `diskutil activity` exits on its next write.
#[cfg(target_os = "macos")]
pub fn watch(changed: impl Fn() + Send + 'static) -> Result<()> {
    let mut child = Command::new("/usr/sbin/diskutil")
        .arg("activity")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("Cannot read disk activity")?;
    std::thread::spawn(move || {
        // The disks already present are listed first, up to the first idle mark.
        let mut idle = false;
        for line in BufReader::new(stdout)
            .lines()
            .map_while(std::result::Result::ok)
        {
            idle |= line.starts_with("***DAIdle");
            if idle && disk_event(&line) {
                changed();
            }
        }
        let status = child.wait();
        eprintln!("CarryFS Manager: disk watching stopped ({status:?}); use Refresh");
    });
    Ok(())
}
fn disk_event(line: &str) -> bool {
    [
        "***DiskAppeared ('",
        "***DiskDisappeared ('",
        "***DiskDescriptionChanged ('",
    ]
    .iter()
    .any(|event| line.starts_with(event))
}

pub fn details(device: &Device) -> Result<Device> {
    #[cfg(target_os = "macos")]
    {
        let v = diskutil(&["info", "-plist", &device.id])?;
        let mut fresh = parse_device(
            &v,
            device.parent.clone(),
            v.get("WholeDisk").and_then(Value::as_bool).unwrap_or(true),
        )?;
        device.verify(&fresh)?;
        if fresh.kind == DeviceKind::Unknown
            && let Some(parent) = &fresh.parent
            && parent != &fresh.id
        {
            let info = diskutil(&["info", "-plist", parent])?;
            fresh.kind = match info.get("VirtualOrPhysical").and_then(Value::as_str) {
                Some("Physical") => DeviceKind::Physical,
                Some("Virtual") => DeviceKind::Virtual,
                _ => DeviceKind::Unknown,
            };
        }
        Ok(fresh)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = device;
        Err("Device operations are currently available on macOS.".into())
    }
}

/// The raw node of an unmounted device, read-only, opened by the
/// administrator helper; it asks for approval once per session.
pub fn device_file(device: &Device) -> Result<fs::File> {
    #[cfg(target_os = "macos")]
    {
        crate::privileged::open_device(&device.id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = device;
        Err("Device reads are currently available on macOS.".into())
    }
}

pub fn open_device(device: &Device) -> Result<carryfs_core::ImageDevice> {
    let fresh = details(device)?;
    if fresh.mount.is_some() {
        return Err("Unmount the volume before inspecting or recovering it.".into());
    }
    #[cfg(target_os = "macos")]
    {
        let file = device_file(&fresh)?;
        file.try_lock()
            .map_err(|e| format!("The device is in use: {e}"))?;
        // Recheck after authorization: the user may have mounted or reattached it meanwhile.
        let after = details(&fresh)?;
        if after.mount.is_some() {
            return Err("The volume was mounted while opening it. Unmount and try again.".into());
        }
        Ok(carryfs_core::ImageDevice::from_device_file(
            file,
            fresh.bytes,
        )?)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("Device reads are currently available on macOS.".into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceAction {
    Mount,
    Unmount,
    Eject,
}
pub fn act(device: &Device, action: DeviceAction) -> Result<String> {
    let fresh = details(device)?;
    if fresh.protected() || device.uuid.is_none() {
        return Err("This source is protected or has no stable reported identity.".into());
    }
    let verb = match action {
        DeviceAction::Mount if !fresh.whole && fresh.mount.is_none() => "mount",
        DeviceAction::Unmount if !fresh.whole && fresh.mount.is_some() => "unmount",
        DeviceAction::Eject if fresh.kind == DeviceKind::Physical || fresh.parent.is_some() => {
            "eject"
        }
        _ => {
            return Err(
                "This operation is not available for the selected device state. Refresh devices."
                    .into(),
            );
        }
    };
    let target = &fresh.id;
    Ok(String::from_utf8_lossy(&output(
        Command::new("/usr/sbin/diskutil").args([verb, target]),
    )?)
    .trim()
    .to_owned())
}

#[derive(Clone, Debug, Default)]
pub struct Versions {
    pub installed: Option<String>,
    pub latest: Option<String>,
}
pub fn versions() -> Versions {
    let installed = match fs::read_to_string("/Library/Filesystems/carryfs.fs/Contents/VERSION") {
        Ok(value) => Some(value.trim().to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!("Unable to read installed version: {error}")),
    };
    Versions {
        installed,
        latest: None,
    }
}
impl Versions {
    pub fn update_available(&self) -> bool {
        let Some(latest) = self.latest.as_deref().and_then(version) else {
            return false;
        };
        match self.installed.as_deref() {
            None => true,
            Some(installed) => version(installed).is_some_and(|installed| latest > installed),
        }
    }
}
fn version(s: &str) -> Option<[u64; 3]> {
    let parts: Vec<_> = s
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    parts.try_into().ok()
}
pub fn check_updates() -> Result<Versions> {
    let mut v = versions();
    let index = output(Command::new("/usr/bin/curl").args([
        "--fail",
        "--silent",
        "--show-error",
        "--max-time",
        "20",
        "https://carryfs.com/releases.txt",
    ]))?;
    v.latest = String::from_utf8(index)?
        .lines()
        .filter(|s| version(s).is_some())
        .max_by_key(|s| version(s))
        .map(str::to_owned);
    if v.latest.is_none() {
        return Err("The release index contains no supported stable version.".into());
    }
    Ok(v)
}

/// Open `path` in Finder.
pub fn show(path: &Path) -> Result<()> {
    output(Command::new("/usr/bin/open").arg(path))?;
    Ok(())
}

/// Select `path` in a Finder window.
pub fn reveal(path: &Path) -> Result<()> {
    output(Command::new("/usr/bin/open").arg("-R").arg(path))?;
    Ok(())
}

pub fn install(version_name: &str) -> Result<String> {
    if version(version_name).is_none() {
        return Err("Invalid release version".into());
    }
    #[cfg(target_os = "macos")]
    {
        crate::privileged::install(version_name)?;
        Ok(format!(
            "Installed CarryFS {version_name}. Volumes were not changed."
        ))
    }
    #[cfg(not(target_os = "macos"))]
    Err("Installing from the manager is currently available on macOS.".into())
}

/// Erase an unmounted external partition, or a whole external disk into one
/// GPT partition, as CarryFS named `name`. Disk Arbitration runs the installed
/// formatter; no administrator approval needed.
pub fn erase(device: &Device, name: &str) -> Result<String> {
    let fresh = details(device)?;
    if !fresh.erasable() || fresh.mount.is_some() {
        return Err(
            "Erasing requires an external physical disk or an unmounted partition on one.".into(),
        );
    }
    carryfs_core::VolumeLabel::parse(name).map_err(|e| format!("Invalid name: {e}"))?;
    if !Path::new("/Library/Filesystems/carryfs.fs/Contents/Resources/newfs_carryfs").is_file() {
        return Err("Install CarryFS from Software before erasing a partition.".into());
    }
    let args: &[&str] = if fresh.whole {
        // diskutil unmounts the disk's volumes itself.
        &["eraseDisk", "CarryFS", name, "GPT", &fresh.id]
    } else {
        &["eraseVolume", "CarryFS", name, &fresh.id]
    };
    Ok(
        String::from_utf8_lossy(&output(Command::new("/usr/sbin/diskutil").args(args))?)
            .trim()
            .to_owned(),
    )
}

/// Let an unmounted CarryFS partition store permissions and the hidden flag.
pub fn upgrade(device: &Device) -> Result<String> {
    let fresh = details(device)?;
    if fresh.protected() || fresh.whole || !fresh.carryfs() || fresh.mount.is_some() {
        return Err("Upgrading requires an unmounted external CarryFS partition.".into());
    }
    #[cfg(target_os = "macos")]
    {
        crate::privileged::upgrade(&fresh.id)
    }
    #[cfg(not(target_os = "macos"))]
    Err("Upgrading a device is currently available on macOS.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inventory_preserves_disks_partitions_and_mounts_and_rejects_bad_identifiers() {
        let v = serde_json::json!({"AllDisksAndPartitions":[{"DeviceIdentifier":"disk8","Size":536870912,"Partitions":[{"DeviceIdentifier":"disk8s1","Size":268435456,"DiskUUID":"ABC-123","VolumeName":"Backup","Content":"CarryFS","MountPoint":"/Volumes/Backup"}]}]});
        let devices = parse_inventory(&v).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[1].parent.as_deref(), Some("disk8"));
        assert_eq!(
            devices[1].mount.as_deref(),
            Some(Path::new("/Volumes/Backup"))
        );
        assert!(devices[1].carryfs());
        assert!(valid_id("disk8s1"));
        assert!(!valid_id("disk8;erase"));
        assert!(!valid_id("disk"));
        let mut changed = devices[1].clone();
        changed.uuid = Some("DIFFERENT".into());
        assert!(devices[1].verify(&changed).is_err());
        let info = serde_json::json!({"DeviceIdentifier":"disk8s1","Size":268435456,"TotalSize":268000000,"DiskUUID":"ABC-123"});
        let fresh = parse_device(&info, Some("disk8".into()), false).unwrap();
        assert!(devices[1].verify(&fresh).is_ok());
    }
    #[test]
    fn apfs_containers_belong_to_their_physical_store() {
        let inventory = serde_json::json!({"AllDisksAndPartitions": [
            {"DeviceIdentifier":"disk0", "Size":1000, "Partitions":[{"DeviceIdentifier":"disk0s2", "Size":900}]},
            {"DeviceIdentifier":"disk3", "Size":900, "APFSPhysicalStores":[{"DeviceIdentifier":"disk0s2"}],
             "APFSVolumes":[{"DeviceIdentifier":"disk3s1", "Size":800, "VolumeName":"Macintosh HD"}]}
        ]});
        let devices = parse_inventory(&inventory).unwrap();
        assert_eq!(devices[2].parent.as_deref(), Some("disk0s2"));
        assert_eq!(devices[3].parent.as_deref(), Some("disk3"));
    }
    #[test]
    fn disk_events_ignore_approvals_and_diskless_volumes() {
        assert!(disk_event(
            "***DiskAppeared ('disk5', DAVolumePath = '<null>') Time=1"
        ));
        assert!(disk_event(
            "***DiskDisappeared ('disk5s1', DAVolumeKind = 'exfat') Time=1"
        ));
        assert!(disk_event(
            "***DiskDescriptionChanged ('disk5s1', DAVolumePath = '<null>') Time=1"
        ));
        assert!(!disk_event(
            "***DiskMountApproval ('disk5s1', DAVolumeKind = 'exfat') Time=1"
        ));
        assert!(!disk_event(
            "***DiskAppeared ((no BSD name), DAVolumeKind = 'autofs') Time=1"
        ));
        assert!(!disk_event("***DAIdle (no DADiskRef) Time=1"));
    }
    #[test]
    fn versions_sort_numerically_and_do_not_accept_shell_input() {
        assert!(version("0.10.0") > version("0.9.9"));
        assert_eq!(version("1.2.3;whoami"), None);
        assert_eq!(version("1.2"), None);
        assert!(
            !Versions {
                installed: Some("1.0.0".into()),
                latest: Some("0.9.0".into())
            }
            .update_available()
        );
        assert!(
            Versions {
                installed: Some("0.9.0".into()),
                latest: Some("1.0.0".into())
            }
            .update_available()
        );
        assert!(
            !Versions {
                installed: Some("Unknown".into()),
                latest: Some("1.0.0".into())
            }
            .update_available()
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod live_tests {
    use super::*;
    #[test]
    #[ignore = "Reads this Mac's live disk inventory; never opens or changes a device"]
    fn discovered_devices_can_be_reidentified_without_opening_them() {
        let devices = discover().unwrap();
        for device in &devices {
            let fresh = details(device).unwrap_or_else(|e| panic!("{}: {e}", device.id));
            assert_eq!(device.id, fresh.id);
        }
        println!(
            "Reidentified {} disks and volumes through diskutil metadata only",
            devices.len()
        );
    }
    #[test]
    #[ignore = "Attaches a throwaway disk image to this Mac"]
    fn attaching_a_disk_is_noticed() {
        let (tx, rx) = std::sync::mpsc::channel();
        watch(move || tx.send(()).unwrap_or_default()).unwrap();
        let image = std::env::temp_dir().join(format!("carryfs-watch-{}.img", std::process::id()));
        fs::File::create(&image).unwrap().set_len(8 << 20).unwrap();
        // Give the watcher time to list the present disks before attaching.
        std::thread::sleep(std::time::Duration::from_secs(3));
        let attached = output(
            Command::new("/usr/bin/hdiutil")
                .args(["attach", "-nomount"])
                .arg(&image),
        )
        .unwrap();
        let disk = String::from_utf8_lossy(&attached)
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        let noticed = rx.recv_timeout(std::time::Duration::from_secs(10));
        output(Command::new("/usr/bin/hdiutil").args(["detach", &disk])).unwrap();
        fs::remove_file(&image).unwrap();
        noticed.expect("No disk event after attaching an image");
    }
}

#[cfg(test)]
mod classification_tests {
    use super::*;
    #[test]
    fn omitted_kind_is_unknown_and_a_partition_flag_does_not_override_virtual() {
        let mut info = serde_json::json!({"DeviceIdentifier":"disk8s1","TotalSize":268435456,"PartitionMapPartition":true});
        assert_eq!(
            parse_device(&info, Some("disk8".into()), false)
                .unwrap()
                .kind,
            DeviceKind::Unknown
        );
        info["VirtualOrPhysical"] = "Virtual".into();
        assert_eq!(
            parse_device(&info, Some("disk8".into()), false)
                .unwrap()
                .kind,
            DeviceKind::Virtual
        );
    }
}
