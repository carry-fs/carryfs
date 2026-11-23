"""FreeBSD/NetBSD lifecycle for full.Sample's shared workload and result schema.

Physical disks only, mounted as root. CarryFS runs through `carryfs-fuse serve`
(fusefs on FreeBSD, PUFFS on NetBSD). The native rows are UFS2 or FFSv2 and
exFAT through the FUSE exfat port.
"""

import fcntl
import os
from pathlib import Path
import re
import select
import stat
import struct
import subprocess
import time

from benchmarks import full

HOST = full.HOST
# DIOCGMEDIASIZE is _IOR('d', 129, off_t) on FreeBSD and _IOR('d', 132, off_t) on NetBSD.
MEDIASIZE = {"freebsd": 0x40086481, "netbsd": 0x40086484}[HOST]
# CarryFS keeps its roots in a 2 MiB band at each end of the device (FORMAT.md).
BAND = 2 << 20
CACHE = ("sync, then a fresh mount before each first pass (unmount releases the filesystem's cached data); "
         "CarryFS reads the raw disk; immediate and delayed repeats; drive cache retained")
FLUSH = "fsync(file) and fsync(directory)"
MOUNT_LINE = re.compile(r"^(\S+) on (.+?) (?:type (\S+) )?\((.*)\)$")


def media_size(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        return struct.unpack("q", fcntl.ioctl(fd, MEDIASIZE, bytes(8)))[0]
    finally:
        os.close(fd)


def block_device(path):
    """NetBSD mounts block nodes and does raw I/O on /dev/r*; FreeBSD disks have one node."""
    return re.sub(r"^/dev/r", "/dev/", path) if HOST == "netbsd" else path


def disk_name(path):
    match = re.match(r"[a-z]+[0-9]+", Path(path).name)
    return match[0] if match else Path(path).name


def mounts():
    """mount(8) lines: FreeBSD puts the type first in the parentheses, NetBSD after `type`."""
    result = []
    for line in full.capture(["mount"]).splitlines():
        match = MOUNT_LINE.match(line)
        if match:
            options = [option.strip() for option in match[4].split(",")]
            fstype = match[3] or options.pop(0)
            result.append(dict(source=match[1], target=match[2], fstype=fstype, options=",".join(options)))
    return result


def validate_device(path, minimum):
    real = os.path.realpath(path)
    if not stat.S_ISCHR(os.stat(real).st_mode):
        raise ValueError(f"{path}: select a raw disk device")
    disk = disk_name(block_device(real))
    if any(disk_name(entry["source"]) == disk for entry in mounts()):
        raise ValueError(f"{path}: the disk holds a mounted volume")
    size = media_size(real)
    if size < minimum:
        raise ValueError(f"{path}: disk is too small for the workload")
    return dict(path=real, size=size, rdev=os.stat(real).st_rdev)


def preflight(config, args):
    if args.images:
        raise ValueError("BSD runs use physical disks; image mode runs on Linux, macOS and Windows")
    if config["mount_mode"] != "root":
        raise ValueError("BSD runs mount as root; set mount_mode to root")
    if args.command == "run" and os.geteuid() != 0:
        raise ValueError("root mount mode needs root")
    if args.command == "run" and not args.format_devices:
        raise ValueError("physical runs format partitions; pass --format-devices to explicitly select that operation")
    systems = {target["fs"] for disk in config["disks"] for target in disk["filesystems"]}
    required = [(("mount", "umount"), "base system mount tools"), (("git",), "source revision metadata; install Git")]
    for fs, commands, help_ in (
            ("ufs", ("newfs", "fsck_ufs"), "UFS formatter/checker; part of the base system"),
            ("ffs", ("newfs", "fsck_ffs"), "FFS formatter/checker; part of the base system"),
            ("exfat", ("mkexfatfs", "exfatfsck", "mount.exfat"),
             "exFAT tools; install exfat-utils and fusefs-exfat (FreeBSD) or fuse-exfat (NetBSD)")):
        if fs in systems:
            required.append((commands, help_))
    for disk in config["disks"]:
        for target in disk["filesystems"]:
            required.append((tuple(Path(target["bin_dir"]) / name for name in ("carryfs", "carryfs-fuse")),
                             f"{disk['name']}/{target['name']} formatter and checker; run build first and pass --bin-dir"))
    full.require_commands(required)
    if "carryfs" in systems:
        if HOST == "freebsd" and subprocess.run(["kldstat", "-q", "-m", "fusefs"]).returncode:
            raise ValueError("fusefs is not loaded; run kldload fusefs")
        if HOST == "netbsd" and not Path("/dev/puffs").exists():
            raise ValueError("/dev/puffs is unavailable; CarryFS mounts need PUFFS")
    full.preflight_output(args, True)
    devices = {}
    for disk in config["disks"]:
        for target in disk["filesystems"]:
            devices[f"{disk['name']}/{target['name']}"] = validate_device(target["device"], full.minimum_bytes(config["workload"]))
            mount = Path(target["mount"])
            if mount.is_symlink() or os.path.ismount(mount) or (mount.exists() and (not mount.is_dir() or any(mount.iterdir()))):
                raise ValueError(f"{mount}: mount directory must be empty and unmounted")
            for protected_path in (full.REPO, Path(target["bin_dir"]), args.out or full.REPO / "build"):
                if protected_path.resolve().is_relative_to(mount.resolve()):
                    raise ValueError(f"{mount}: would hide source, binaries or result files")
    return devices


class Sample(full.Sample):
    @property
    def stat_inode_ids(self):
        # FUSE exFAT on NetBSD reports st_ino 0 from stat; readdir's numbers are distinct.
        return not (HOST == "netbsd" and self.target["fs"] == "exfat")

    def prepare(self):
        key = f"{self.disk}/{self.target['name']}"
        current = validate_device(self.target["device"], full.minimum_bytes(self.runner.workload))
        if current != self.runner.devices[key]:
            raise ValueError("device identity changed after preflight")
        self.device = current["path"]
        if os.path.ismount(self.point):
            raise ValueError(f"{self.point}: already mounted")
        fs = self.target["fs"]
        # Native formatters leave an earlier CarryFS root in the end band, and
        # the CarryFS formatter reads an exFAT boot sector's 0x55AA as a
        # partition table, so every sample starts from zeroed bands.
        fd = os.open(self.device, os.O_WRONLY)
        try:
            for offset in (0, current["size"] - BAND):
                if os.pwrite(fd, bytes(BAND), offset) != BAND:
                    raise OSError("short write while clearing the reserved bands")
            os.fsync(fd)
        finally:
            os.close(fd)
        if fs == "carryfs":
            self.command(Path(self.target["bin_dir"]) / "carryfs-fuse", "mkfs", self.device)
        elif fs == "exfat":
            # exfat-utils writes partial sectors, which NetBSD raw disks refuse.
            self.command("mkexfatfs", "-n", "CFSBENCH", block_device(self.device))
        elif fs == "ufs":
            self.command("newfs", "-U", "-L", "CFSBENCH", self.device)
        else:
            # -F: NetBSD's newfs refuses the whole-disk partition d, whose label type is unused.
            self.command("newfs", "-O", "2", "-F", "-s", current["size"] // 512, self.device)

    def mount(self, mode, phase):
        if self.mounted or os.path.ismount(self.point):
            raise ValueError(f"{self.point}: already mounted")
        os.sync()
        fs = self.target["fs"]
        native_mode = mode if self.runner.config["native_ro"] else "rw"
        started = time.perf_counter()
        if fs == "carryfs":
            self.daemon_log = (self.folder / "daemon.log").open("a")
            env = dict(os.environ)
            env.pop("CARRYFS_RW_CACHE", None)
            env.pop("CARRYFS_TRACE", None)
            self.daemon = subprocess.Popen(
                [str(Path(self.target["bin_dir"]) / "carryfs-fuse"), "serve", self.device, str(self.point),
                 f"{mode},uid={os.getuid()},gid={os.getgid()}"],
                stdout=subprocess.PIPE, stderr=self.daemon_log, text=True, env=env)
            # serve prints ready once mounted; a stat of the mount point during
            # the mount can hold the vnode the FUSE handshake needs.
            if not select.select([self.daemon.stdout], [], [], 60)[0] or self.daemon.stdout.readline().strip() != "ready":
                raise RuntimeError(f"daemon mount failed or timed out; see {self.folder / 'daemon.log'}")
        elif fs == "exfat":
            self.command("mount.exfat", "-o", f"{native_mode},noatime,uid={os.getuid()},gid={os.getgid()}",
                         block_device(self.device), self.point, timeout=60)
        else:
            # log: NetBSD's WAPBL journal, the installer default.
            options = native_mode + ",noatime" + (",log" if fs == "ffs" else "")
            self.command("mount", "-t", fs, "-o", options, block_device(self.device), self.point, timeout=60)
        self.mounted = True
        elapsed = time.perf_counter() - started
        mounted = next((entry for entry in mounts() if entry["target"] == str(self.point)), None)
        if mounted is None:
            raise RuntimeError(f"{self.point}: not in the mount table after mounting")
        if fs in ("ufs", "ffs") and (os.path.realpath(mounted["source"]) != block_device(self.device) or mounted["fstype"] != fs):
            raise ValueError("mounted source/filesystem differs from the selected target")
        key = f"{self.disk}/{self.target['name']}/{mode}"
        setting = {field: mounted[field] for field in ("fstype", "options")}
        if self.runner.mount_settings.setdefault(key, setting) != setting:
            raise ValueError("mount options changed within the run")
        self.record("mount", requested_mode=mode, phase=phase, actual=mounted)
        self.emit(None, "mount", "mount_" + phase, elapsed, "s")
        return self.point / "bench"

    def check(self):
        fs = self.target["fs"]
        if fs == "carryfs":
            size = self.runner.devices[f"{self.disk}/{self.target['name']}"]["size"]
            for action in ("check", "scrub"):
                self.command(Path(self.target["bin_dir"]) / "carryfs", action, "--bytes", size, self.device)
        elif fs == "exfat":
            self.command("exfatfsck", "-n", block_device(self.device))
        else:
            # -f: a cleanly unmounted volume is otherwise skipped.
            self.command("fsck_" + fs, "-f", "-n", self.device)
        self.record("check", result="passed")

    def stats(self):
        # No per-disk counters are read on BSD; the rows are marked unavailable.
        return None
