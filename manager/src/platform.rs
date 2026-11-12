use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(target_os = "macos")]
#[path = "../../src/cli/authopen.rs"]
mod authopen;

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
    pub fn visible_by_default(&self, all: &[Device]) -> bool {
        if self.carryfs() {
            return true;
        }
        if self.protected() || self.kind == DeviceKind::Virtual {
            return false;
        }
        if self.whole {
            return self.kind != DeviceKind::Virtual
                && !all.iter().any(|d| d.parent.as_deref() == Some(&self.id));
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
fn valid_id(id: &str) -> bool {
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
        bytes: v
            .get("TotalSize")
            .or_else(|| v.get("Size"))
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

pub fn open_device(device: &Device) -> Result<carryfs_core::ImageDevice> {
    let fresh = details(device)?;
    if fresh.mount.is_some() {
        return Err("Unmount the volume before inspecting or recovering it.".into());
    }
    #[cfg(target_os = "macos")]
    {
        let path = PathBuf::from(format!("/dev/r{}", fresh.id));
        let file = match fs::File::open(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                authopen::open_read_only(&path)?
            }
            result => result?,
        };
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
        DeviceAction::Mount if !fresh.whole && fresh.carryfs() && fresh.mount.is_none() => "mount",
        DeviceAction::Unmount if !fresh.whole && fresh.carryfs() && fresh.mount.is_some() => {
            "unmount"
        }
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

fn terminal(script: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("carryfs-manager-{}-{stamp}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("operation.command");
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&path)?;
        file.write_all(script.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
        output(
            Command::new("/usr/bin/open")
                .args(["-a", "Terminal"])
                .arg(path),
        )?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = script;
        Err("The interactive installer is currently available through the manager on macOS.".into())
    }
}

pub fn install(version_name: &str) -> Result<String> {
    if version(version_name).is_none() {
        return Err("Invalid release version".into());
    }
    terminal(&format!(
        "#!/bin/bash\nset -eu\nprintf '%s\\n' 'CarryFS software update to {version_name}. This does not migrate disk formats.'\ninstaller=$(mktemp -t carryfs-install)\ntrap 'rm -f \"$installer\"' EXIT\n/usr/bin/curl --fail --show-error --location https://carryfs.com/install.sh -o \"$installer\"\n/bin/bash \"$installer\" --version {version_name}\nprintf '%s\\n' 'Return to CarryFS Manager and refresh Versions.'\n"
    ))?;
    Ok("Installer opened in Terminal. It verifies the release package, requires unmounted volumes, and asks for installation and administrator approval. Refresh versions when it finishes.".into())
}

pub fn format_in_terminal(device: &Device, confirmation: &str) -> Result<String> {
    let fresh = details(device)?;
    if !fresh.external_partition() || fresh.mount.is_some() || confirmation != fresh.id {
        return Err("Formatting requires an unmounted external physical partition and its exact device identifier.".into());
    }
    if !Path::new("/Library/Filesystems/carryfs.fs/Contents/Resources/newfs_carryfs").is_file() {
        return Err("Install the CarryFS system tools before formatting.".into());
    }
    // The Terminal flow re-identifies the partition immediately before eraseVolume.
    let uuid = fresh.uuid.as_deref().ok_or("Missing partition UUID")?;
    if !uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err("Invalid partition UUID".into());
    }
    terminal(&format!(
        "#!/bin/bash\nset -eu\nprintf '%s\\n' 'ERASE {id}: {size} bytes. All files on this partition will be lost.'\n/usr/sbin/diskutil info {id}\nprintf '%s' 'Type {id} again to format: '\nread -r target\n[ \"$target\" = '{id}' ] || exit 1\nactual=$(/usr/sbin/diskutil info -plist {id} | /usr/bin/plutil -extract DiskUUID raw -o - -)\n[ \"$actual\" = '{uuid}' ] || {{ printf '%s\\n' 'Device identity changed; refusing format.'; exit 1; }}\n/usr/sbin/diskutil eraseVolume CarryFS CarryFS {id}\n",
        id = fresh.id,
        size = fresh.bytes
    ))?;
    Ok("Formatting confirmation opened in Terminal. No format has been reported as complete. Refresh devices after the system tool finishes.".into())
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
