"""macOS/Windows lifecycle for full.Sample's shared workload and result schema."""

import base64
import ctypes
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import sys
import tempfile
import time
import uuid

from benchmarks import full
from benchmarks import native_io


def binaries(mode):
    if sys.platform == "win32":
        return ("carryfs.exe", "carryfs-windows.exe")
    result = ("carryfs", "carryfs-fuse")
    return result + ("carryfs-bench-mount",) if sys.platform == "linux" else result


def access_checks(config):
    if config["mount_mode"] == "user":
        return True
    return bool(ctypes.windll.shell32.IsUserAnAdmin()) if sys.platform == "win32" else os.geteuid() == 0


def cache_policy():
    if sys.platform == "linux":
        return full.CACHE
    if sys.platform == "win32":
        return ("fresh CarryFS session; native partition volumes are locked and dismounted, image volumes detach/reattach VHD; "
                "backing-file and drive caches retained; immediate and delayed repeats")
    return "fresh mount before each first pass; host/backing-file and drive caches retained; immediate and delayed repeats"


def flush_policy():
    return {"linux": "fsync(file) and fsync(directory)",
            "darwin": "F_FULLFSYNC(file) and fsync(directory)",
            "win32": "FlushFileBuffers(file); directory barriers flush the entire volume"}[sys.platform]


def ps_quote(value):
    return "'" + str(value).replace("'", "''") + "'"


def powershell(script):
    encoded = base64.b64encode(("$ErrorActionPreference='Stop'; " + script).encode("utf-16-le")).decode()
    return full.capture(["powershell", "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded]).strip()


def plist(args):
    return plistlib.loads(subprocess.check_output([str(arg) for arg in args]))


def mac_info(device):
    return plist(["diskutil", "info", "-plist", device])


def mac_device(target, minimum):
    device = target["device"]
    if not re.fullmatch(r"/dev/(?:r)?disk[0-9]+s[0-9]+", device):
        raise ValueError("macOS device must be an explicit partition, such as /dev/disk7s2")
    expected = str(uuid.UUID(target.get("partition_uuid", ""))).lower()
    part = mac_info(device.replace("/dev/rdisk", "/dev/disk"))
    parent = mac_info(part["ParentWholeDisk"])
    content = str(part.get("Content", "")).lower()
    if (part.get("Internal", True) or not part.get("WritableMedia") or part.get("Mounted")
            or part.get("MountPoint") or "efi" in content
            or parent.get("VirtualOrPhysical") != "Physical"
            or str(part.get("DiskUUID", "")).lower() != expected):
        raise ValueError("macOS target must match the UUID of an unmounted, writable external physical partition; EFI is refused")
    containers = plist(["diskutil", "apfs", "list", "-plist"]).get("Containers", []) if "apfs" in content else []
    volumes = []
    for container in containers:
        stores = container.get("PhysicalStores", [])
        if any(store.get("DeviceIdentifier") == part["DeviceIdentifier"] for store in stores):
            if len(stores) != 1 or any(volume.get("MountPoint") for volume in container.get("Volumes", [])):
                raise ValueError("APFS physical store must have one backing partition and no mounted volumes")
            volumes.extend(container.get("Volumes", []))
    if target.get("fs") == "apfs" and "apfs" in content and len(volumes) != 1:
        raise ValueError("APFS benchmark partition must contain exactly one volume")
    root = mac_info("/")
    roots = {root.get("ParentWholeDisk")}
    stores = root.get("APFSPhysicalStores", [])
    if root.get("APFSContainerReference"):
        containers = plist(["diskutil", "apfs", "list", "-plist"]).get("Containers", [])
        for container in containers:
            if container.get("ContainerReference") == root["APFSContainerReference"]:
                stores += container.get("PhysicalStores", [])
    for store in stores:
        identifier = store.get("DeviceIdentifier", "")
        roots.add(re.sub(r"s[0-9]+$", "", identifier))
    if parent["DeviceIdentifier"] in roots:
        raise ValueError("macOS target disk backs the system volume")
    if part["TotalSize"] < minimum:
        raise ValueError("partition is too small for the workload")
    return dict(path="/dev/" + part["DeviceIdentifier"], size=part["TotalSize"],
                parent=parent["DeviceIdentifier"], uuid=expected,
                model=parent.get("MediaName"))


def windows_numbers(device):
    match = re.fullmatch(r"\\\\\.\\Harddisk([0-9]+)Partition([1-9][0-9]*)", device, re.IGNORECASE)
    if match is None:
        raise ValueError(r"Windows device must name a partition: \\.\HarddiskNPartitionP")
    return tuple(map(int, match.groups()))


def windows_device(target, minimum):
    number, partition = windows_numbers(target["device"])
    expected = str(uuid.UUID(target.get("partition_uuid", ""))).lower()
    info = json.loads(powershell(
        f"$d=Get-Disk -Number {number}; $p=Get-Partition -DiskNumber {number} -PartitionNumber {partition}; "
        "[ordered]@{guid=[string]$p.Guid; size=$p.Size; serial=[string]$d.SerialNumber;"
        " boot=[bool]($d.IsBoot -or $d.IsSystem -or $p.IsBoot -or $p.IsSystem);"
        " readonly=[bool]($d.IsReadOnly -or $p.IsReadOnly); letter=[string]$p.DriveLetter;"
        " paths=@($p.AccessPaths); type=[string]$p.GptType} | ConvertTo-Json -Compress"))
    protected = {"c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "e3c9e316-0b5c-4db8-817d-f92df00215ae",
                 "de94bba4-06d1-4d40-a16a-bfd50179d6ac"}
    mounted_paths = any(path and not re.fullmatch(r"\\\\\?\\Volume\{[^}]+\}\\", path, re.IGNORECASE)
                        for path in info.get("paths", []))
    if (info["boot"] or info["readonly"] or info["letter"].strip("\x00 ") or mounted_paths or info["type"].strip("{}").lower() in protected
            or info["guid"].strip("{}").lower() != expected):
        raise ValueError("Windows target must match an unmounted writable GPT partition UUID; system/EFI/recovery/reserved targets are refused")
    if info["size"] < minimum:
        raise ValueError("partition is too small for the workload")
    return dict(path=target["device"], size=info["size"], parent=number, uuid=expected, serial=info["serial"])


def preflight(config, args):
    if sys.platform not in ("darwin", "win32"):
        raise ValueError("supported hosts are Linux, macOS and Windows")
    if not args.images and args.command == "run" and not args.format_devices:
        raise ValueError("physical runs format partitions; pass --format-devices")
    minimum = full.minimum_bytes(config["workload"])
    if args.images and args.size_mib << 20 < minimum:
        raise ValueError("image capacity is too small for the selected workload")
    if sys.platform == "win32":
        if config["mount_mode"] != "root":
            raise ValueError("Windows mounts require mount_mode root and an elevated terminal")
        if args.command == "run" and not ctypes.windll.shell32.IsUserAnAdmin():
            raise ValueError("Windows image/device runs need an elevated terminal")
        if config["native_ro"] and not args.images:
            raise ValueError("Windows native_ro currently requires disposable images; physical disks are not made read-only")
        required = [(("powershell", "diskpart", "chkdsk"), "Windows disk/mount/check tools; enable the built-in tools")]
    else:
        if args.command == "run" and config["mount_mode"] == "root" and os.geteuid() != 0:
            raise ValueError("root mount mode needs root")
        required = [(("diskutil", "hdiutil", "umount"), "macOS disk/mount/check tools; use the built-in system tools")]
    required.append((("git",), "source revision metadata; install Git"))
    for disk in config["disks"]:
        for target in disk["filesystems"]:
            if target["fs"] == "carryfs":
                required.append((tuple(Path(target["bin_dir"]) / executable for executable in binaries(config["mount_mode"])),
                                 f"{disk['name']}/{target['name']} binaries; run build first and pass its release directory as --bin-dir"))
    full.require_commands(required)
    check_access = access_checks(config)
    full.preflight_output(args, check_access)
    devices = {}
    systems_by_device = {}
    carryfs = any(target["fs"] == "carryfs" for disk in config["disks"] for target in disk["filesystems"])
    if carryfs and sys.platform == "darwin" and not Path("/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse").exists():
        raise ValueError("macFUSE must already be installed and enabled")
    if carryfs and sys.platform == "win32":
        installed = powershell("(Get-ItemProperty 'HKLM:\\SOFTWARE\\WOW6432Node\\WinFsp' -ErrorAction Stop).InstallDir")
        if not installed:
            raise ValueError("WinFsp must already be installed")
        service = powershell("$s=Get-Service -Name CarryFS -ErrorAction SilentlyContinue; if($s){[string]$s.Status}")
        if service == "Running":
            raise ValueError("the CarryFS automount service must be stopped before a controlled benchmark")
    for disk in config["disks"]:
        for target in disk["filesystems"]:
            if args.images:
                continue
            key = f"{disk['name']}/{target['name']}"
            devices[key] = (mac_device if sys.platform == "darwin" else windows_device)(target, minimum)
            if sys.platform == "darwin":
                systems = systems_by_device.setdefault(devices[key]["path"], set())
                systems.add(target["fs"])
                if "apfs" in systems and len(systems) > 1:
                    raise ValueError("macOS APFS benchmark partitions cannot be shared with other filesystems")
            point = Path(target["mount"])
            if sys.platform == "win32":
                if not re.fullmatch(r"[D-Zd-z]:\\", target["mount"]) or point.exists():
                    raise ValueError("Windows mount must be an unused drive root, such as R:\\")
            else:
                if point.is_symlink() or os.path.ismount(point) or (point.exists() and (not point.is_dir() or any(point.iterdir()))):
                    raise ValueError("macOS mount directory must be empty and unmounted")
                for protected_path in (full.REPO, Path(target["bin_dir"]), args.out or full.REPO / "build"):
                    if protected_path.resolve().is_relative_to(point.resolve()):
                        raise ValueError("mount directory would hide source, binaries or results")
                if check_access and (target["fs"] == "carryfs" or not point.exists()):
                    full.writable_directory(point)
                raw = devices[key]["path"].replace("/dev/disk", "/dev/rdisk")
                if check_access and not os.access(raw, os.R_OK | os.W_OK):
                    raise ValueError("raw partition needs read/write access before formatting")
    return devices


def backing_mount(out):
    if sys.platform == "linux":
        return full.capture(["findmnt", "-T", out, "-n", "-o", "SOURCE,FSTYPE,OPTIONS"]).strip()
    if sys.platform == "darwin":
        point = Path(out).resolve()
        while not os.path.ismount(point) and point != point.parent:
            point = point.parent
        info = mac_info(point)
        return {key: info.get(key) for key in ("DeviceIdentifier", "FilesystemType", "MountPoint")}
    return powershell(f"Get-Volume -DriveLetter {ps_quote(out.drive[0])} | Select-Object FileSystem,UniqueId,AllocationUnitSize | ConvertTo-Json -Compress")


def windows_lock():
    api = native_io.windows_api()
    handle = api.CreateMutexW(None, False, "Global\\CarryFSFullBenchmark")
    error = ctypes.get_last_error()
    if not handle:
        raise ctypes.WinError(error)
    if error == 183:
        api.CloseHandle(handle)
        raise ValueError("another full benchmark is running on this host")
    return handle


def windows_unlock(handle):
    if not native_io.windows_api().CloseHandle(handle):
        raise ctypes.WinError(ctypes.get_last_error())


class Sample(full.Sample):
    """Only lifecycle differs; full.Sample owns every timed workload."""

    def __init__(self, *args):
        super().__init__(*args)
        self.temporary_point = None
        if sys.platform == "darwin" and self.runner.args.images:
            # exFAT remounts fail when the mount point is on its external backing volume.
            self.temporary_point = Path(tempfile.mkdtemp(prefix="carryfs-bench-mount-")).resolve()
            self.point = self.temporary_point
        self.image = self.attached = self.volume = self.size = None
        self.number = self.partition = None

    def stats(self):
        return None

    def ps(self, script):
        with self.log.open("a") as log:
            log.write("$ powershell " + script + "\n")
        return powershell(script)

    def diskpart(self, script):
        if '"' in str(self.folder) or "\n" in str(self.folder) or "\r" in str(self.folder):
            raise ValueError("output path cannot contain quotes or newlines")
        path = self.folder / ("diskpart-create.txt" if script.startswith("create vdisk") else "diskpart-format.txt")
        # diskpart /s rejects a UTF-16 script on Windows 10 22H2; it reads the ANSI code page.
        with path.open("x", encoding="mbcs") as sink:
            sink.write(script)
        self.command("diskpart", "/s", path)

    def prepare(self):
        if self.runner.args.images:
            self.size = self.runner.args.size_mib << 20
        else:
            expected = self.runner.devices[f"{self.disk}/{self.target['name']}"]
            current = (mac_device if sys.platform == "darwin" else windows_device)(
                self.target, full.minimum_bytes(self.runner.workload))
            if current != expected:
                raise ValueError("device identity changed after preflight")
            self.device, self.size = current["path"], current["size"]
        if sys.platform == "darwin":
            self.prepare_macos()
        else:
            self.prepare_windows()

    def prepare_macos(self):
        fs = self.target["fs"]
        cli = Path(self.target["bin_dir"]) / "carryfs"
        if self.runner.args.images:
            self.image = self.folder / "disk.img"
            if fs == "carryfs":
                self.command(cli, "mkfs", self.image, self.size)
            else:
                with self.image.open("xb") as sink:
                    sink.truncate(self.size)
            attached = plist(["hdiutil", "attach", "-plist", "-imagekey", "diskimage-class=CRawDiskImage", "-nomount", self.image])
            whole = [entry["dev-entry"] for entry in attached["system-entities"]
                     if re.fullmatch(r"/dev/disk[0-9]+", entry.get("dev-entry", ""))]
            if len(whole) != 1:
                raise ValueError("image attach did not return exactly one whole virtual disk")
            self.attached = self.device = whole[0]
        elif fs == "carryfs":
            image = self.folder / "format.img"
            self.command(cli, "mkfs", image, self.size)
            import fcntl
            with image.open("rb") as source, open(self.device.replace("/dev/disk", "/dev/rdisk"), "r+b", buffering=0) as sink:
                fcntl.flock(sink.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                full.copy_sparse(source.fileno(), sink.fileno())
                # Same raw-device cache flush as macos/src/raw.rs.
                fcntl.ioctl(sink.fileno(), 0x20006416)
        if fs == "carryfs":
            self.volume = self.device
        else:
            label = "CFS" + uuid.uuid4().hex[:8].upper()
            kind = {"exfat": "ExFAT", "apfs": "APFS"}[fs]
            if self.image:
                self.command("diskutil", "eraseDisk", kind, label, "GPT", self.device)
            elif fs == "apfs" and mac_info(self.device).get("Content") == "Apple_APFS":
                containers = plist(["diskutil", "apfs", "list", "-plist"])["Containers"]
                volumes = [volume for container in containers
                           if any(store["DeviceIdentifier"] == Path(self.device).name
                                  for store in container["PhysicalStores"])
                           for volume in container["Volumes"]]
                if len(volumes) != 1:
                    raise ValueError("APFS benchmark partition must contain exactly one volume")
                self.command("diskutil", "eraseVolume", kind, label, "/dev/" + volumes[0]["DeviceIdentifier"])
            else:
                self.command("diskutil", "eraseVolume", kind, label, self.device)
            matches = []
            def visit(item):
                if item.get("VolumeName") == label:
                    matches.append("/dev/" + item["DeviceIdentifier"])
                for key in ("Partitions", "APFSVolumes"):
                    for child in item.get(key, []):
                        visit(child)
            for item in plist(["diskutil", "list", "-plist"])["AllDisksAndPartitions"]:
                visit(item)
            if len(set(matches)) != 1:
                raise ValueError("formatted native volume was not uniquely identified")
            self.volume = matches[0]
            point = mac_info(self.volume).get("MountPoint")
            if point:
                if not self.image and os.geteuid() == 0:
                    self.command("mdutil", "-i", "off", point)
                elif not self.image:
                    # mdutil needs root. The marker keeps Spotlight off later mounts.
                    (Path(point) / ".metadata_never_index").touch()
                # Without root, the index Spotlight opened at format time refuses
                # the unmount for a few seconds.
                for attempt in range(30):
                    try:
                        self.command("diskutil", "unmount", self.volume)
                        break
                    except RuntimeError:
                        if attempt == 29 or self.image or os.geteuid() == 0:
                            raise
                        time.sleep(1)

    def prepare_windows(self):
        fs = self.target["fs"]
        if self.runner.args.images:
            self.image = self.folder / "disk.vhd"
            attrs = "0xC000000000000000" if fs == "carryfs" else "0x8000000000000000"
            self.attached = self.image
            self.diskpart(f'create vdisk file="{self.image}" maximum={self.runner.args.size_mib + 128} type=fixed\n'
                          f'select vdisk file="{self.image}"\nattach vdisk\nconvert gpt\n'
                          f'create partition primary size={self.runner.args.size_mib}\ngpt attributes={attrs}\n')
            self.locate_windows_image()
        else:
            self.number, self.partition = windows_numbers(self.device)
        if fs == "carryfs":
            self.command(Path(self.target["bin_dir"]) / "carryfs-windows.exe", "mkfs", self.device, "force")
        else:
            if not self.runner.args.images:
                self.ps(f"Set-Partition -DiskNumber {self.number} -PartitionNumber {self.partition} -NoDefaultDriveLetter $true")
            self.diskpart(f"select disk {self.number}\nselect partition {self.partition}\nformat fs={fs} quick label=CFSBENCH\n")
        if self.runner.args.images:
            used = self.ps("(Get-PSDrive -PSProvider FileSystem).Name").split()
            letter = next((letter for letter in "RSTUVWXYQPONML" if letter not in used), None)
            if letter is None:
                raise ValueError("no unused drive letter")
            self.point = Path(letter + ":\\")

    def locate_windows_image(self):
        info = json.loads(self.ps(
            f"$d=Get-DiskImage -ImagePath {ps_quote(self.image)} | Get-Disk; "
            "$p=@($d | Get-Partition | Where-Object Type -eq 'Basic'); "
            "if($p.Count -ne 1){throw 'expected one basic image partition'}; "
            "[ordered]@{number=$d.Number; partition=$p[0].PartitionNumber; size=$p[0].Size;"
            " boot=[bool]($d.IsBoot -or $d.IsSystem)} | ConvertTo-Json -Compress"))
        if info["boot"] or info["size"] != self.size:
            raise ValueError("attached image identity or capacity differs")
        self.number, self.partition = info["number"], info["partition"]
        self.device = f"\\\\.\\Harddisk{self.number}Partition{self.partition}"

    def windows_native_mount(self, mode):
        if self.image and self.attached is None:
            access = "ReadOnly" if mode == "ro" and self.runner.config["native_ro"] else "ReadWrite"
            self.ps(f"Mount-DiskImage -ImagePath {ps_quote(self.image)} -NoDriveLetter -Access {access} | Out-Null")
            self.attached = self.image
            self.locate_windows_image()
        self.ps(f"Add-PartitionAccessPath -DiskNumber {self.number} -PartitionNumber {self.partition} -AccessPath {ps_quote(self.point)}")

    def mount(self, mode, phase):
        if self.mounted or self.point.exists() and sys.platform == "win32":
            raise ValueError("mount point is already in use")
        started = time.perf_counter()
        fs = self.target["fs"]
        if fs == "carryfs":
            self.daemon_log = (self.folder / "daemon.log").open("a")
            if sys.platform == "darwin":
                args = [Path(self.target["bin_dir"]) / "carryfs-fuse", "serve", self.device, self.point,
                        f"{mode},uid={os.getuid()},gid={os.getgid()}"]
            else:
                args = [Path(self.target["bin_dir"]) / "carryfs-windows.exe", "serve", self.device, self.point.drive]
                if mode == "ro":
                    args.append("ro")
            env = dict(os.environ)
            for key in ("CARRYFS_TRACE", "CARRYFS_RW_CACHE", "CARRYFS_STATS"):
                env.pop(key, None)
            self.daemon = subprocess.Popen([str(arg) for arg in args], stdin=subprocess.PIPE,
                                           stdout=self.daemon_log, stderr=self.daemon_log, env=env)
            while not (self.point.exists() if sys.platform == "win32" else os.path.ismount(self.point)):
                if self.daemon.poll() is not None or time.perf_counter() - started > 60:
                    raise RuntimeError(f"mount failed or timed out; see {self.folder / 'daemon.log'}")
                time.sleep(.01)
        elif sys.platform == "darwin":
            args = ["diskutil", "mount", "-mountPoint", self.point]
            if mode == "ro" and self.runner.config["native_ro"]:
                args.append("readOnly")
            self.command(*args, self.volume, timeout=60)
        else:
            self.windows_native_mount(mode)
        self.mounted = True
        elapsed = time.perf_counter() - started
        if sys.platform == "darwin":
            readonly = bool(os.statvfs(self.point).f_flag & os.ST_RDONLY)
            if fs != "carryfs":
                info = mac_info(self.volume)
                if info.get("MountPoint") != str(self.point) or info.get("FilesystemType", "").lower() != fs:
                    raise ValueError("native mounted source or filesystem differs")
        else:
            info = native_io.volume_info(self.point)
            if info["fstype"] != fs:
                raise ValueError("native mounted filesystem differs")
            readonly = info["readonly"]
        if fs == "carryfs" or self.runner.config["native_ro"]:
            if readonly != (mode == "ro"):
                raise ValueError("actual mount mode differs")
        setting = dict(fstype=fs, options="ro" if readonly else "rw")
        key = f"{self.disk}/{self.target['name']}/{mode}"
        if self.runner.mount_settings.setdefault(key, setting) != setting:
            raise ValueError("mount settings changed within the run")
        self.record("mount", requested_mode=mode, phase=phase, actual=setting)
        self.emit(None, "mount", "mount_" + phase, elapsed, "s")
        return self.point / "bench"

    def unmount(self, phase):
        started = time.perf_counter()
        if self.daemon is not None:
            if self.daemon.poll() is None:
                if sys.platform == "darwin":
                    self.command("umount", self.point, timeout=60)
                else:
                    self.daemon.stdin.close()
            status = full.wait_exit(self.daemon, 60)
            self.daemon_log.close()
            self.daemon = self.daemon_log = None
            if status:
                raise RuntimeError(f"daemon exited {status}; see {self.folder / 'daemon.log'}")
        elif self.mounted:
            if sys.platform == "darwin":
                self.command("diskutil", "unmount", self.volume, timeout=60)
            else:
                volume = self.ps(f"Remove-PartitionAccessPath -DiskNumber {self.number} -PartitionNumber {self.partition} -AccessPath {ps_quote(self.point)}; "
                                 f"(Get-Partition -DiskNumber {self.number} -PartitionNumber {self.partition} | Get-Volume).Path")
                if self.image and self.attached is not None:
                    self.ps(f"Dismount-DiskImage -ImagePath {ps_quote(self.image)} | Out-Null")
                    self.attached = None
                elif not self.image:
                    # Without its access path a partition's volume stays mounted and cached.
                    native_io.dismount_volume(volume)
        if self.mounted and (self.point.exists() if sys.platform == "win32" else os.path.ismount(self.point)):
            raise RuntimeError("mount remains after unmount")
        self.mounted = False
        self.emit(None, "mount", "unmount_" + phase, time.perf_counter() - started, "s")

    def check(self):
        if self.target["fs"] == "carryfs":
            cli = Path(self.target["bin_dir"]) / ("carryfs.exe" if sys.platform == "win32" else "carryfs")
            device = self.device.replace("/dev/disk", "/dev/rdisk") if sys.platform == "darwin" else self.device
            for action in ("check", "scrub"):
                self.command(cli, action, "--bytes", self.size, device)
            method = "offline CarryFS check and scrub"
        elif sys.platform == "darwin":
            self.command("diskutil", "verifyVolume", self.volume)
            method = "diskutil verifyVolume; no repair"
        else:
            self.windows_native_mount("ro")
            self.mounted = True
            try:
                self.command("chkdsk", self.point.drive)
            finally:
                self.ps(f"Remove-PartitionAccessPath -DiskNumber {self.number} -PartitionNumber {self.partition} -AccessPath {ps_quote(self.point)}")
                self.mounted = False
            method = "read-only chkdsk on an otherwise idle mounted volume; no repair"
        self.record("check", result="passed", method=method)

    def close(self):
        try:
            if self.daemon is not None or self.mounted:
                self.unmount("cleanup")
        finally:
            try:
                if self.daemon is not None:
                    if self.daemon.poll() is None:
                        self.daemon.kill()
                    self.daemon.wait(timeout=10)
                    if sys.platform == "darwin" and os.path.ismount(self.point):
                        self.command("umount", self.point, timeout=30)
                    self.daemon_log.close()
                if sys.platform == "darwin" and self.volume and self.target["fs"] != "carryfs" and mac_info(self.volume).get("MountPoint"):
                    self.command("diskutil", "unmount", self.volume)
                if self.attached is not None:
                    if sys.platform == "darwin":
                        self.command("hdiutil", "detach", self.attached)
                    elif self.image.exists():
                        self.ps(f"$i=Get-DiskImage -ImagePath {ps_quote(self.image)}; if($i.Attached){{$i | Dismount-DiskImage | Out-Null}}")
            finally:
                self.writer.close()
                if self.temporary_point is not None and not os.path.ismount(self.temporary_point):
                    self.temporary_point.rmdir()
