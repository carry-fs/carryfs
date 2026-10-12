"""One full workload and result contract, with explicit host/disk configuration."""

import csv
import errno
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import select
import shlex
import shutil
import stat
import statistics
import subprocess
import sys
import time
import tomllib
from types import SimpleNamespace

if os.name == "posix":
    import fcntl

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[1]
from benchmarks.workloads import bench
from benchmarks.workloads import listing

SYSTEMS = ("carryfs", "ext4", "exfat", "btrfs", "apfs", "ntfs")
SUPPORTED = {"linux": ("carryfs", "ext4", "exfat", "btrfs"),
             "darwin": ("carryfs", "exfat", "apfs"),
             "win32": ("carryfs", "exfat", "ntfs")}
DEFAULTS = dict(seq_mib=512, files=1000, dirs=20, per_dir=50,
                many_files=10000, batch=128, stale_seconds=1.5)
CACHE = "sync + partition fadvise(DONTNEED) before each fresh mount; immediate and delayed repeats; drive cache retained"
EFI_TYPES = {"c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "0xef", "ef",
             "21686148-6449-6e6f-744e-656564454649"}


def wait_exit(process, timeout):
    """Return `process`'s exit status, or raise `TimeoutExpired`.

    `Popen.wait(timeout)` and `subprocess.run(timeout)` poll with sleeps that
    double from 0.5 ms, so a short mount, unmount or daemon exit is reported
    at the next of 3.5, 7.5, 15.5, 31.5 or 63.5 ms: a 7 ms unmount and a 15 ms
    one both read as 16. Linux reports the exit through a pidfd instead;
    hosts without pidfds keep the polling wait.
    """
    if sys.platform != "linux" or not hasattr(os, "pidfd_open"):
        return process.wait(timeout=timeout)
    if process.poll() is not None:
        return process.returncode
    try:
        pidfd = os.pidfd_open(process.pid)
    except OSError as error:
        if error.errno not in (errno.ENOSYS, errno.EPERM, errno.ESRCH):
            raise
        return process.wait(timeout=timeout)
    try:
        if not select.select([pidfd], [], [], timeout)[0]:
            raise subprocess.TimeoutExpired(process.args, timeout)
    finally:
        os.close(pidfd)
    return process.wait()


def capture(args):
    result = subprocess.run([str(arg) for arg in args], text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"{shlex.join([str(arg) for arg in args])}: {result.stderr.strip() or result.stdout.strip()}")
    return result.stdout


def digest(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def source_digest(path):
    return hashlib.sha256(Path(path).read_bytes().replace(b"\r\n", b"\n")).hexdigest()


def validate_listing(names, attributes, count, inode_ids=True):
    if sorted(names) != [f"f{index:05d}.dat" for index in range(count)]:
        raise ValueError("listing names or counts differ")
    if attributes and (len(attributes) != count or any(a.st_size != 4096 for a in attributes)
                       or inode_ids and len({a.st_ino for a in attributes}) != count):
        raise ValueError("listing attributes differ")


def name(value):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", value):
        raise ValueError(f"invalid name: {value!r}")
    return value


def configuration(args):
    if args.images:
        config = dict(mount_mode="user" if sys.platform == "darwin" else "root", disks=[dict(name="images", filesystems=[
            dict(fs=fs, name=fs) for fs in args.systems])])
    else:
        config = json.loads(args.config.read_text())
    if not isinstance(config, dict):
        raise ValueError("configuration must be a JSON object")
    unknown = set(config) - {"rounds", "mount_mode", "native_ro", "workload", "latency", "disks"}
    if unknown:
        raise ValueError(f"unknown configuration fields: {sorted(unknown)}")
    config.setdefault("rounds", 3)
    if args.rounds is not None:
        config["rounds"] = args.rounds
    if type(config["rounds"]) is not int or config["rounds"] <= 0:
        raise ValueError("rounds must be a positive integer")
    config.setdefault("mount_mode", "root" if sys.platform == "win32" else "user")
    if config["mount_mode"] not in ("user", "root"):
        raise ValueError("mount_mode must be user or root")
    config.setdefault("native_ro", False)
    if getattr(args, "native_ro", False):
        config["native_ro"] = True
    config.setdefault("latency", False)
    if args.latency:
        config["latency"] = True
    if type(config["native_ro"]) is not bool or type(config["latency"]) is not bool:
        raise ValueError("native_ro and latency must be booleans")
    if sys.platform == "linux" and config["mount_mode"] == "user" and config["native_ro"]:
        raise ValueError("native_ro requires root mounts; fstab user mounts keep their configured mode")
    override = config.get("workload", {})
    if not isinstance(override, dict) or set(override) - set(DEFAULTS):
        raise ValueError("unknown workload field")
    config["workload"] = dict(DEFAULTS, **override)
    for key, value in config["workload"].items():
        if key == "stale_seconds":
            if isinstance(value, bool) or not isinstance(value, (float, int)) or not math.isfinite(value) or value < 1.1:
                raise ValueError("stale_seconds must be finite and at least 1.1")
        elif type(value) is not int or value <= 0:
            raise ValueError(f"{key} must be a positive integer")
    if config["workload"]["files"] == config["workload"]["many_files"]:
        raise ValueError("files and many_files must differ so listing cohorts remain distinct")
    from benchmarks import focused
    selection = focused.select(config["workload"], getattr(args, "metric", None), getattr(args, "phase", None))
    if selection is not None:
        config["selection"] = selection
        if selection["metric"].startswith("batch_fsync_"):
            config["latency"] = True
        if config["latency"] and focused.operation(config["workload"], selection) not in ("batch-create", "batch-remove"):
            raise ValueError("--latency applies to batch creation/deletion metrics")
    if not isinstance(config.get("disks"), list) or not config["disks"]:
        raise ValueError("disks must be a nonempty list")
    names = set()
    for disk in config["disks"]:
        if not isinstance(disk, dict) or "name" not in disk:
            raise ValueError("each disk must be an object with a name")
        name(disk["name"])
        if disk["name"] in names or set(disk) - {"name", "filesystems"}:
            raise ValueError("disk names must be unique; disks accept name and filesystems")
        names.add(disk["name"])
        systems = disk.get("filesystems")
        if not isinstance(systems, list) or not systems:
            raise ValueError("filesystems must be a nonempty list")
        labels = set()
        for target in systems:
            if not isinstance(target, dict) or set(target) - {"fs", "name", "device", "mount", "bin_dir", "partition_uuid"} or target.get("fs") not in SUPPORTED.get(sys.platform, ()):
                raise ValueError(f"unsupported filesystem target: {target}")
            target.setdefault("name", target["fs"])
            name(target["name"])
            if target["name"] in labels:
                raise ValueError("filesystem target names must be unique within a disk")
            labels.add(target["name"])
            if not args.images:
                if sys.platform != "linux" and not isinstance(target.get("partition_uuid"), str):
                    raise ValueError("macOS/Windows physical targets require partition_uuid")
                for field in ("device", "mount"):
                    if not isinstance(target.get(field), str) or not target[field] or (sys.platform != "win32" and not Path(target[field]).is_absolute()):
                        raise ValueError(f"{disk['name']}/{target['name']}: {field} must be an absolute path")
            if "bin_dir" in target and (not isinstance(target["bin_dir"], str) or not target["bin_dir"]):
                raise ValueError("bin_dir must be a nonempty path string")
            target["bin_dir"] = str(Path(target.get("bin_dir", args.bin_dir)).resolve())
    return config


def block_inventory():
    inventory = json.loads(capture(["lsblk", "--json", "--bytes", "--paths", "--output",
                                    "NAME,TYPE,PKNAME,SIZE,MOUNTPOINTS,PARTTYPE,MODEL,SERIAL,RO"]))
    flat = {}
    def add(items):
        for item in items:
            key = os.path.realpath(item["name"])
            parents = set(flat.get(key, {}).get("parents", []))
            if item.get("pkname"):
                parents.add(os.path.realpath(item["pkname"]))
            # Shared RAID/LVM devices appear under more than one parent.
            flat[key] = dict(item, parents=sorted(parents))
            add(item.get("children", []))
    add(inventory["blockdevices"])
    return flat


def validate_device(path, inventory, minimum):
    real = os.path.realpath(path)
    info = inventory.get(real)
    if info is None or info["type"] != "part" or not stat.S_ISBLK(os.stat(real).st_mode):
        raise ValueError(f"{path}: select a partition, not a whole disk or a regular file")
    if info.get("ro") or any(info.get("mountpoints") or []) or (info.get("parttype") or "").lower() in EFI_TYPES:
        raise ValueError(f"{path}: read-only, mounted, swap, EFI or boot partitions are refused")
    parent = os.path.realpath(info["pkname"])
    protected = {"/", "/boot", "/boot/efi", "/home", "[SWAP]"}
    for other in inventory.values():
        if not protected.intersection(other.get("mountpoints") or []):
            continue
        pending = [os.path.realpath(other["name"])]
        visited = set()
        while pending:
            ancestor = pending.pop()
            if ancestor == parent:
                raise ValueError(f"{path}: disk also contains a system or swap volume")
            if ancestor in visited:
                continue
            visited.add(ancestor)
            current = inventory.get(ancestor, {})
            pending.extend(current.get("parents", [os.path.realpath(current["pkname"])] if current.get("pkname") else []))
    if info["size"] < minimum:
        raise ValueError(f"{path}: partition is too small for the workload")
    return dict(path=real, size=info["size"], parent=parent,
                model=inventory.get(parent, {}).get("model"),
                serial=inventory.get(parent, {}).get("serial"), rdev=os.stat(real).st_rdev)


def minimum_bytes(workload):
    return (workload["seq_mib"] << 20) + 4096 * (workload["files"] + workload["many_files"] +
                                              workload["dirs"] * workload["per_dir"]) + (256 << 20)


def require_commands(groups):
    missing = []
    for commands, help_ in groups:
        absent = [str(command) for command in commands if shutil.which(str(command)) is None]
        if absent:
            missing.append(f"  - {', '.join(absent)}: {help_}")
    if missing:
        raise ValueError("missing benchmark dependencies:\n" + "\n".join(missing) +
                         "\nFix these dependencies and rerun plan. Host commands must be on PATH.")


def writable_directory(path):
    existing = path
    while not existing.exists() and not existing.is_symlink():
        existing = existing.parent
    if not existing.is_dir() or not os.access(existing, os.W_OK | os.X_OK):
        raise ValueError(f"{path}: needs a writable directory or parent; check access to {existing}")


def preflight_output(args, check_access):
    out = args.out or REPO / "build"
    if args.out and (out.exists() or out.is_symlink()):
        raise ValueError(f"{out}: output path already exists; choose a new --out path")
    if check_access:
        writable_directory(out.parent if args.out else out)


def preflight(config, args):
    from benchmarks import native
    if sys.platform != "linux":
        return native.preflight(config, args)
    if args.command == "run" and config["mount_mode"] == "root" and os.geteuid() != 0:
        raise ValueError("root mount mode needs root; user mode uses fusermount3 and fstab user entries")
    required = [(("lsblk", "findmnt", "mount", "umount", "wipefs"), "Linux disk/mount tools; install util-linux"),
                (("git",), "source revision metadata; install Git")]
    if args.images:
        required.extend([(("losetup",), "disposable loop images; install util-linux"),
                         (("udevadm",), "loop-device cleanup; install your host's udev package")])
        if args.size_mib * (1 << 20) < minimum_bytes(config["workload"]):
            raise ValueError("image capacity is too small for the selected workload")
    elif args.command == "run" and not args.format_devices:
        raise ValueError("physical runs format partitions; pass --format-devices to explicitly select that operation")
    systems = {target["fs"] for disk in config["disks"] for target in disk["filesystems"]}
    for fs, commands, help_ in (
            ("ext4", ("mkfs.ext4", "e2fsck"), "ext4 formatter/checker; install e2fsprogs"),
            ("exfat", ("mkfs.exfat", "fsck.exfat"), "exFAT formatter/checker; install exfatprogs"),
            ("btrfs", ("mkfs.btrfs", "btrfs"), "Btrfs formatter/checker; install btrfs-progs")):
        if fs in systems:
            required.append((commands, help_))
    if "carryfs" in systems and config["mount_mode"] == "user":
        required.append((("fusermount3",), "CarryFS user mounts; install your host's FUSE 3 tools"))
    for disk in config["disks"]:
        for target in disk["filesystems"]:
            if target["fs"] == "carryfs":
                required.append((tuple(Path(target["bin_dir"]) / executable for executable in native.binaries(config["mount_mode"])),
                                 f"{disk['name']}/{target['name']} binaries; run build first and pass its release directory as --bin-dir"))
    require_commands(required)
    check_access = native.access_checks(config)
    if check_access and config["mount_mode"] == "root":
        capabilities = re.search(r"^CapEff:\s*([0-9a-fA-F]+)$", Path("/proc/self/status").read_text(), re.MULTILINE)
        if capabilities is None or not int(capabilities[1], 16) & (1 << 21):
            raise ValueError("root mounts need CAP_SYS_ADMIN; use a host with mount privileges or user mount mode")
    preflight_output(args, check_access)
    if "carryfs" in systems:
        if not Path("/dev/fuse").exists():
            raise ValueError("/dev/fuse is unavailable; enable FUSE before benchmarking CarryFS")
        if check_access and not os.access("/dev/fuse", os.R_OK | os.W_OK):
            raise ValueError("/dev/fuse needs read/write access for CarryFS mounts")
    inventory = block_inventory() if not args.images else {}
    fstab = None
    if config["mount_mode"] == "user":
        fstab = json.loads(capture(["findmnt", "--fstab", "--evaluate", "--json", "--output",
                                  "SOURCE,TARGET,FSTYPE,OPTIONS"]))["filesystems"]
    devices = {}
    for disk in config["disks"]:
        for target in disk["filesystems"]:
            fs = target["fs"]
            if not args.images:
                key = f"{disk['name']}/{target['name']}"
                devices[key] = validate_device(target["device"], inventory, minimum_bytes(config["workload"]))
                if check_access and not os.access(devices[key]["path"], os.R_OK | os.W_OK):
                    raise ValueError(f"{target['device']}: partition needs read/write access before formatting")
                mount = Path(target["mount"])
                if mount.is_symlink() or os.path.ismount(mount) or (mount.exists() and (not mount.is_dir() or any(mount.iterdir()))):
                    raise ValueError(f"{mount}: mount directory must be empty and unmounted")
                for protected_path in (REPO, Path(target["bin_dir"]), args.out or REPO / "build"):
                    if protected_path.resolve().is_relative_to(mount.resolve()):
                        raise ValueError(f"{mount}: would hide source, binaries or result files")
                if check_access and (fs == "carryfs" or not mount.exists()):
                    writable_directory(mount)
                if fs != "carryfs" and fstab is not None:
                    entries = [entry for entry in fstab if entry["target"] == str(mount)]
                    if len(entries) != 1 or os.path.realpath(entries[0]["source"]) != devices[key]["path"] or entries[0]["fstype"] != fs:
                        raise ValueError(f"{mount}: no matching fstab entry for {fs} on {target['device']}")
                    if "user" not in entries[0]["options"].split(",") and "users" not in entries[0]["options"].split(","):
                        raise ValueError(f"{mount}: fstab needs user or users")
    return devices


def plan(args):
    from benchmarks import native
    config = configuration(args)
    devices = preflight(config, args)
    print(json.dumps(dict(preflight="passed", access_checks="checked for current user" if native.access_checks(config) else
                          "deferred; rerun plan with the root/administrator identity used for run",
                          configuration=config, devices=devices, images=args.images,
                          image_mib=args.size_mib if args.images else None,
                          cache=native.cache_policy(), operation="format each target before every sample"), indent=2))


def build_mount(out, source):
    crate = out / "mount-harness"
    (crate / "src").mkdir(parents=True)
    # Generate the manifest in build/, so the selected source tree stays untouched.
    root = json.dumps(str(source), ensure_ascii=False)
    patch = tomllib.loads((source / "Cargo.toml").read_text())["patch"]["crates-io"]["fuser"]
    manifest = ('[package]\nname="carryfs-bench-mount"\nversion="0.1.0"\nedition="2024"\n'
                '[workspace]\n[features]\nfault-injection=[]\n[dependencies]\ncarryfs-core={path=' + root + '}\nlibc="0.2"\n'
                'fuser={version="0.18",default-features=false}\n[patch.crates-io]\nfuser={' +
                ",".join(f"{key}={json.dumps(value)}" for key, value in patch.items()) + '}\n')
    (crate / "Cargo.toml").write_text(manifest)
    shutil.copyfile(source / "Cargo.lock", crate / "Cargo.lock")
    header = "".join(f"#[allow(dead_code)]\n#[path={json.dumps(str(source / 'fuse/src' / (module + '.rs')), ensure_ascii=False)}]\nmod {module};\n"
                     for module in ("fs", "device"))
    (crate / "src/main.rs").write_text(header + (REPO / "benchmarks/mount.rs").read_text())
    with (out / "build-mount.log").open("w") as log:
        subprocess.run(["cargo", "build", "--offline", "--release", "--manifest-path", str(crate / "Cargo.toml"),
                        "--target-dir", str(out)], check=True, stdout=log, stderr=subprocess.STDOUT)


def device_stats(device):
    dev = Path(os.path.realpath(device)).name
    values = [int(value) for value in (Path("/sys/class/block") / dev / "stat").read_text().split()]
    return values[2] * 512, values[6] * 512, values[9]


def daemon_memory(pid):
    """Linux daemon RSS and its lifetime high-water mark, in KiB."""
    fields = {line.split()[0]: int(line.split()[1])
              for line in Path(f"/proc/{pid}/status").read_text().splitlines()
              if line.startswith(("VmRSS:", "VmHWM:"))}
    return dict(rss_kib=fields["VmRSS:"], peak_rss_kib=fields["VmHWM:"])


def copy_sparse(source, destination):
    """Copy generated format extents in bounded reads, completing short writes."""
    size = os.fstat(source).st_size
    offset = 0
    while offset < size:
        try:
            start = os.lseek(source, offset, os.SEEK_DATA)
        except OSError as error:
            if error.errno == errno.ENXIO:
                break
            raise
        end = os.lseek(source, start, os.SEEK_HOLE)
        if not offset <= start < end <= size:
            raise ValueError("invalid sparse format extent")
        while start < end:
            block = os.pread(source, min(1 << 20, end - start), start)
            if not block:
                raise ValueError("format image was truncated")
            remaining = memoryview(block)
            while remaining:
                written = os.pwrite(destination, remaining, start)
                if written <= 0:
                    raise OSError("format copy made no progress")
                start += written
                remaining = remaining[written:]
        offset = end


class Sample:
    """One formatted filesystem, owning only mounts and loops it starts."""

    def __init__(self, runner, disk, target, round_):
        self.runner, self.disk, self.target, self.round = runner, disk, target, round_
        self.folder = runner.out / disk / f"{target['name']}-r{round_}"
        self.folder.mkdir(parents=True)
        self.log = self.folder / "commands.log"
        self.daemon = self.daemon_log = self.loop = None
        self.mounted = False
        self.device = target.get("device")
        self.point = Path(target["mount"]) if not runner.args.images else self.folder / "mnt"
        if sys.platform != "win32":
            self.point.mkdir(parents=True, exist_ok=True)
        self.phase = None
        self.writer = (self.folder / "results.jsonl").open("w")

    def command(self, *args, timeout=300):
        with self.log.open("a") as log:
            log.write("$ " + shlex.join([str(arg) for arg in args]) + "\n")
            log.flush()
            process = subprocess.Popen([str(arg) for arg in args], stdout=log, stderr=subprocess.STDOUT)
            try:
                status = wait_exit(process, timeout)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise
        if status:
            raise RuntimeError(f"command exited {status}; see {self.log}: {shlex.join([str(arg) for arg in args])}")

    def record(self, kind, **row):
        row.update(kind=kind, disk=self.disk, label=self.target["name"], fs=self.target["fs"], round=self.round)
        self.writer.write(json.dumps(row) + "\n")
        self.writer.flush()

    def emit(self, label, phase, metric, value, unit):
        phase = self.phase or phase
        selection = self.runner.config.get("selection")
        if selection is None or (phase, metric) == (selection["phase"], selection["metric"]):
            self.record("metric", phase=phase, metric=metric, value=value, unit=unit)

    def prepare(self):
        fs = self.target["fs"]
        if self.runner.args.images:
            image = self.folder / "disk.img"
            with image.open("xb") as sink:
                sink.truncate(self.runner.args.size_mib << 20)
            self.loop = capture(["losetup", "--find", "--show", image]).strip()
            self.device = self.loop
        else:
            expected = self.runner.devices[f"{self.disk}/{self.target['name']}"]
            current = validate_device(self.target["device"], block_inventory(), minimum_bytes(self.runner.workload))
            if current != expected:
                raise ValueError("device identity changed after preflight")
            self.device = current["path"]
        if os.path.ismount(self.point):
            raise ValueError(f"{self.point}: already mounted")
        if fs == "carryfs":
            image = self.folder / "format.img"
            destination = os.open(self.device, os.O_WRONLY | os.O_EXCL)
            try:
                size = os.lseek(destination, 0, os.SEEK_END)
                self.command(Path(self.target["bin_dir"]) / "carryfs", "mkfs", image, size)
                self.command("wipefs", "--all", "--force", self.device)
                source = os.open(image, os.O_RDONLY)
                try:
                    copy_sparse(source, destination)
                    os.fsync(destination)
                finally:
                    os.close(source)
            finally:
                os.close(destination)
        else:
            # Refuse an existing kernel claim before native signature removal.
            claim = os.open(self.device, os.O_RDONLY | os.O_EXCL)
            os.close(claim)
            self.command("wipefs", "--all", "--force", self.device)
        if fs == "ext4":
            self.command("mkfs.ext4", "-F", "-q", "-E",
                         f"lazy_itable_init=0,lazy_journal_init=0,nodiscard,root_owner={os.getuid()}:{os.getgid()}",
                         "-L", "CFSBENCH", self.device)
        elif fs == "exfat":
            self.command("mkfs.exfat", "-L", "CFSBENCH", self.device)
        elif fs == "btrfs":
            empty = self.folder / "empty"
            empty.mkdir()
            self.command("mkfs.btrfs", "-f", "-q", "-K", "-L", "CFSBENCH", "--rootdir", empty, self.device)

    def mount(self, mode, phase):
        if self.mounted or os.path.ismount(self.point):
            raise ValueError(f"{self.point}: already mounted")
        os.sync()
        fd = os.open(self.device, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)
        started = time.perf_counter()
        if self.target["fs"] == "carryfs":
            binaries = Path(self.target["bin_dir"])
            self.daemon_log = (self.folder / "daemon.log").open("a")
            if self.runner.config["mount_mode"] == "user":
                args = [binaries / "carryfs-bench-mount", mode, self.device, self.point]
            else:
                args = [binaries / "carryfs-fuse", "serve", self.device, self.point,
                        f"{mode},uid={os.getuid()},gid={os.getgid()}"]
            env = dict(os.environ)
            env.pop("CARRYFS_RW_CACHE", None)
            env.pop("CARRYFS_TRACE", None)
            self.daemon = subprocess.Popen([str(arg) for arg in args], stdin=subprocess.PIPE,
                                           stdout=subprocess.PIPE, stderr=self.daemon_log, text=True, env=env)
            if self.runner.config["mount_mode"] == "user":
                if not select.select([self.daemon.stdout], [], [], 60)[0] or self.daemon.stdout.readline().strip() != "ready":
                    raise RuntimeError(f"daemon failed to become ready; see {self.folder / 'daemon.log'}")
            else:
                while not os.path.ismount(self.point):
                    if self.daemon.poll() is not None or time.perf_counter() - started > 60:
                        raise RuntimeError(f"daemon mount failed or timed out; see {self.folder / 'daemon.log'}")
                    time.sleep(.01)
        elif self.runner.config["mount_mode"] == "user":
            self.command("mount", self.point, timeout=60)
        else:
            native_mode = mode if self.runner.config["native_ro"] else "rw"
            options = native_mode + ",noatime"
            if self.target["fs"] == "exfat":
                options += f",uid={os.getuid()},gid={os.getgid()}"
            self.command("mount", "-t", self.target["fs"], "-o", options, self.device, self.point, timeout=60)
        self.mounted = True
        elapsed = time.perf_counter() - started
        actual = capture(["findmnt", "--json", "-M", self.point, "-o", "SOURCE,FSTYPE,OPTIONS"])
        mounted = json.loads(actual)["filesystems"][0]
        if self.target["fs"] != "carryfs" and (os.path.realpath(mounted["source"]) != os.path.realpath(self.device)
                                                 or mounted["fstype"] != self.target["fs"]):
            raise ValueError("mounted source/filesystem differs from the selected target")
        key = f"{self.disk}/{self.target['name']}/{mode}"
        setting = {field: mounted[field] for field in ("fstype", "options")}
        previous = self.runner.mount_settings.setdefault(key, setting)
        if previous != setting:
            raise ValueError("mount options changed within the run")
        self.record("mount", requested_mode=mode, phase=phase, actual=json.loads(actual))
        self.emit(None, "mount", "mount_" + phase, elapsed, "s")
        return self.point / "bench"

    def unmount(self, phase):
        started = time.perf_counter()
        if sys.platform == "linux" and self.daemon is not None and self.daemon.poll() is None:
            self.record("memory", phase=phase, **daemon_memory(self.daemon.pid))
        if self.daemon is not None and self.runner.config["mount_mode"] == "user":
            if self.daemon.poll() is None:
                self.daemon.stdin.write("quit\n")
                self.daemon.stdin.flush()
        elif self.mounted:
            self.command("umount", self.point, timeout=60)
        if self.daemon is not None:
            status = wait_exit(self.daemon, 60)
            self.daemon_log.close()
            self.daemon_log = None
            self.daemon = None
            if status:
                raise RuntimeError(f"daemon exited {status}; see {self.folder / 'daemon.log'}")
        if self.mounted and os.path.ismount(self.point):
            raise RuntimeError(f"{self.point}: still mounted after daemon exit")
        self.mounted = False
        self.emit(None, "mount", "unmount_" + phase, time.perf_counter() - started, "s")

    def verify(self, root, remaining=None):
        expected = json.loads((root / "manifest.json").read_text())
        if remaining is not None:
            expected = {key: expected[key] for key in remaining}
        if bench.manifest(str(root)) != expected:
            raise ValueError(f"{self.folder}: namespace or SHA-256 manifest differs after remount")
        self.record("verification", entries=len(expected), result="passed")

    def check(self):
        fs = self.target["fs"]
        if fs == "carryfs":
            fd = os.open(self.device, os.O_RDONLY)
            try:
                size = os.lseek(fd, 0, os.SEEK_END)
            finally:
                os.close(fd)
            for action in ("check", "scrub"):
                self.command(Path(self.target["bin_dir"]) / "carryfs", action, "--bytes", size, self.device)
        elif fs == "ext4":
            self.command("e2fsck", "-fn", self.device)
        elif fs == "exfat":
            self.command("fsck.exfat", "-n", self.device)
        else:
            self.command("btrfs", "check", "--readonly", self.device)
        self.record("check", result="passed")

    def latency_sync(self, path):
        start = time.perf_counter_ns()
        bench.sync_path(path)
        self.latencies.append((time.perf_counter_ns() - start) / 1e6)

    def latency_report(self, phase):
        if not self.runner.config["latency"]:
            return
        values = sorted(self.latencies)
        self.record("latency", phase=phase, operation="batch_directory_fsync", samples_ms=self.latencies)
        for label, percentile in (("p50", .5), ("p95", .95), ("p99", .99), ("max", 1)):
            self.emit(None, phase, "batch_fsync_" + label, values[math.ceil(len(values) * percentile) - 1], "ms")

    def workload(self):
        if self.runner.config.get("selection") is not None:
            from benchmarks import focused
            focused.run(self)
            return
        w = self.runner.workload
        before = self.stats()
        root = self.mount("rw", "empty")
        root.mkdir()
        bench.sequential_write(str(root), "", w["seq_mib"])
        bench.small_files(str(root), "", w["files"])
        bench.tree(str(root), "", w["dirs"], w["per_dir"])
        bench.listing(str(root), "", "write")
        self.batch_files(root)
        many = root / "many"
        with (root / "manifest.json").open("w") as sink:
            json.dump(bench.manifest(str(root)), sink)
            sink.flush()
            bench.os.fsync(sink.fileno())
        bench.sync_path(root)
        self.unmount("write")
        for mode in ("rw", "ro"):
            self.mount(mode, mode + "-stream")
            for cache in ("cold", "warm"):
                self.phase = mode + "-" + cache
                bench.sequential_read(str(root), "", w["seq_mib"])
            self.phase = None
            self.verify(root)
            self.unmount(mode + "-stream")
            for count, directory in ((w["files"], root / "files"), (w["many_files"], many)):
                for pattern in listing.PATTERNS:
                    phase = f"{mode}-n{count}-{pattern}"
                    self.mount(mode, phase)
                    for cache in ("cold", "warm", "stale"):
                        if cache == "stale":
                            time.sleep(w["stale_seconds"])
                        result, names, attributes = listing.measure(directory, pattern)
                        entry_ids_missing = sys.platform == "win32" and pattern == "interleaved"
                        validate_listing(names, attributes, count, inode_ids=not entry_ids_missing)
                        self.record("listing", mode=mode, cache=cache, **result)
                    if entry_ids_missing:
                        # Windows DirEntry.stat has zero IDs; check after all timed passes.
                        validate_listing(names, [os.stat(directory / name) for name in names], count)
                    self.unmount(phase)
        self.mount("rw", "delete")
        self.verify(root)
        bench.remove(str(root), "")
        elapsed, _ = bench.timed(lambda: bench.sync_path(root))
        self.emit(None, "delete", "fsync_after_remove", elapsed * 1000, "ms")
        self.batch_files(root, remove=True)
        self.unmount("delete")
        self.mount("ro", "after-delete")
        self.verify(root, remaining=("seq.bin",))
        self.unmount("after-delete")
        self.check()
        after = self.stats()
        for metric, value, unit in zip(("sample_read", "sample_write", "sample_busy"),
                                       (tuple(a - b for a, b in zip(after, before)) if before is not None else (None, None, None)),
                                       ("bytes", "bytes", "ms")):
            if value is None:
                self.record("metric", phase="device", metric=metric, value=None, unit=unit,
                            status="unavailable", reason="no equivalent per-partition counters implemented on this platform")
            else:
                self.emit(None, "device", metric, value, unit)

    def batch_files(self, root, remove=False):
        w = self.runner.workload
        many = root / "many"
        if not remove:
            many.mkdir()
        self.latencies = []
        sync = self.latency_sync if self.runner.config["latency"] else bench.sync_path
        start = time.perf_counter()
        for index in range(w["many_files"]):
            path = many / f"f{index:05d}.dat"
            if remove:
                path.unlink()
            else:
                path.write_bytes(b"x" * 4096)
            if (index + 1) % w["batch"] == 0:
                sync(many)
        sync(many)
        if remove:
            many.rmdir()
            bench.sync_path(root)
        phase, verb = ("delete", "remove") if remove else ("write", "create")
        self.emit(None, phase, f"{verb}_{w['many_files']}_batch{w['batch']}_durable", (time.perf_counter() - start) * 1000, "ms")
        self.latency_report(phase)

    def stats(self):
        return device_stats(self.device)

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
                    if os.path.ismount(self.point):
                        tool = ["fusermount3", "-u"] if self.runner.config["mount_mode"] == "user" else ["umount"]
                        self.command(*tool, self.point, timeout=30)
                    self.daemon_log.close()
                if self.loop is not None:
                    self.command("losetup", "--detach", self.loop)
                    backing = Path("/sys/class/block") / Path(self.loop).name / "loop/backing_file"
                    deadline = time.monotonic() + 30
                    while backing.exists():
                        if time.monotonic() > deadline:
                            raise TimeoutError("loop detach did not finish")
                        time.sleep(.02)
                    self.command("udevadm", "settle")
                    self.loop = None
            finally:
                self.writer.close()


def sample_rows(path, workload, selection=None):
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    if sum(row["kind"] == "verification" and row.get("result") == "passed" for row in rows) != (2 if selection else 4):
        raise ValueError(f"{path}: missing manifest verifications")
    if not any(row["kind"] == "check" and row.get("result") == "passed" for row in rows):
        raise ValueError(f"{path}: missing offline check")
    listings = [(row["mode"], row["entries"], row["pattern"], row["cache"]) for row in rows if row["kind"] == "listing"]
    expected = {(mode, count, pattern, cache) for mode in ("rw", "ro")
                for count in (workload["files"], workload["many_files"])
                for pattern in listing.PATTERNS for cache in ("cold", "warm", "stale")}
    if selection:
        from benchmarks import focused
        kind, parameter, phases, unit = focused.catalog(workload)[selection["metric"]]
        if kind == "listing-cell":
            mode, cache = selection["phase"].split("-")
            expected = {(mode, parameter[1], parameter[2], cache)}
        else:
            expected = set()
    if len(listings) != len(expected) or set(listings) != expected:
        raise ValueError(f"{path}: missing or duplicate listing cells")
    result = []
    for row in rows:
        if row["kind"] == "metric":
            value = row["value"]
        elif row["kind"] == "listing":
            row = dict(row, phase=f"{row['mode']}-{row['cache']}",
                       metric=f"list_{row['entries']}_{row['pattern']}", unit="ms", value=row["seconds"] * 1000)
            value = row["value"]
        else:
            continue
        optional = row.get("phase") == "device" and row.get("metric") in ("sample_read", "sample_write", "sample_busy")
        if value is None and optional and row.get("status") == "unavailable" and row.get("reason"):
            result.append(row)
            continue
        if type(value) not in (int, float) or not math.isfinite(value) or value < 0:
            raise ValueError(f"{path}: invalid metric value")
        result.append(row)
    keys = [(row["phase"], row["metric"], row["unit"]) for row in result]
    if len(keys) != len(set(keys)):
        raise ValueError(f"{path}: duplicate metrics")
    if selection:
        if keys != [(selection["phase"], selection["metric"], unit)]:
            raise ValueError(f"{path}: missing or unexpected focused metric")
        return result
    required = {("write", "sequential_write_durable"), ("rw-cold", "sequential_read"),
                ("rw-warm", "sequential_read"), ("ro-cold", "sequential_read"),
                ("ro-warm", "sequential_read"),
                ("delete", f"remove_{workload['many_files']}_batch{workload['batch']}_durable")}
    if not required <= {(row["phase"], row["metric"]) for row in result}:
        raise ValueError(f"{path}: missing full-suite metrics")
    return result


def load(path):
    metadata = json.loads((path / "metadata.json").read_text())
    if metadata.get("version") != 2 or metadata.get("complete") is not True:
        raise ValueError(f"{path}: incomplete or unsupported full run")
    rows = []
    for disk in metadata["configuration"]["disks"]:
        for target in disk["filesystems"]:
            for round_ in range(1, metadata["configuration"]["rounds"] + 1):
                folder = path / name(disk["name"]) / f"{name(target['name'])}-r{round_}"
                if not (folder / "complete").exists():
                    raise ValueError(f"{folder}: incomplete sample")
                rows.extend(sample_rows(folder / "results.jsonl", metadata["configuration"]["workload"], metadata["configuration"].get("selection")))
    return metadata, rows


def table(rows, disk):
    values = {}
    labels = []
    for row in rows:
        if row["disk"] != disk:
            continue
        if row["label"] not in labels:
            labels.append(row["label"])
        key = (row["phase"], row["metric"], row["unit"])
        values.setdefault(key, {}).setdefault(row["label"], []).append(row["value"])
    lines = [f"## {disk}", "", "| Phase | Metric | Unit | " + " | ".join(labels) + " |",
             "|---|---|---|" + "---:|" * len(labels)]
    for key, groups in values.items():
        cells = []
        for label in labels:
            values_ = [value for value in groups.get(label, []) if value is not None]
            cells.append(f"{statistics.median(values_):.3f} ({min(values_):.3f}-{max(values_):.3f})" if values_ else "n/a")
        lines.append(f"| {' | '.join(key)} | {' | '.join(cells)} |")
    return "\n".join(lines)


def report_heading(config):
    selection = config.get("selection")
    if selection:
        return f"# Focused filesystem benchmark\n\nMetric: {selection['metric']}; phase: {selection['phase']}.\n\n"
    return "# Full filesystem benchmark\n\n"


def report(path):
    metadata, rows = load(path)
    text = report_heading(metadata["configuration"]) + "\n\n".join(table(rows, disk["name"]) for disk in metadata["configuration"]["disks"])
    return text, rows


def host_lock():
    path = "/tmp/carryfs-full-benchmark.lock"
    flags = os.O_RDONLY | os.O_NOFOLLOW
    try:
        return os.open(path, flags)
    except FileNotFoundError:
        try:
            return os.open(path, flags | os.O_CREAT | os.O_EXCL, 0o644)
        except FileExistsError:
            return os.open(path, flags)


def run(args):
    from benchmarks import native
    config = configuration(args)
    devices = preflight(config, args)
    print("Preflight passed for all selected targets.", flush=True)
    lock = native.windows_lock() if sys.platform == "win32" else host_lock()
    try:
        if sys.platform != "win32":
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError("another full benchmark is running on this host") from None
        if args.out:
            out = args.out.resolve()
            out.mkdir(parents=True, exist_ok=False)
        else:
            import tempfile
            (REPO / "build").mkdir(exist_ok=True)
            out = Path(tempfile.mkdtemp(prefix="benchmarks-full-", dir=REPO / "build"))
        print(f"Results: {out}", flush=True)
        sources = {path.relative_to(REPO).as_posix(): source_digest(path) for path in
                   (REPO / "benchmarks/workloads/bench.py", REPO / "benchmarks/workloads/listing.py",
                    REPO / "benchmarks/full.py", REPO / "benchmarks/run.py",
                    REPO / "benchmarks/mount.rs", REPO / "benchmarks/native.py", REPO / "benchmarks/native_io.py", REPO / "benchmarks/focused.py")}
        binaries = {}
        for disk in config["disks"]:
            for target in disk["filesystems"]:
                if target["fs"] == "carryfs":
                    directory = Path(target["bin_dir"])
                    binaries[f"{disk['name']}/{target['name']}"] = {name_: digest(directory / name_)
                                                                  for name_ in native.binaries(config["mount_mode"])}
        governors = {path.parent.parent.name: path.read_text().strip()
                     for path in Path("/sys/devices/system/cpu").glob("cpu[0-9]*/cpufreq/scaling_governor")}
        metadata = dict(version=2, suite="fs-focused-v1" if config.get("selection") else "fs-full-v1", complete=False, configuration=config,
                        devices=devices, media="images" if args.images else "physical partitions",
                        size_mib=args.size_mib if args.images else None, cache=native.cache_policy(),
                        flush=native.flush_policy(), platform=sys.platform,
                        backing_mount=native.backing_mount(out) if args.images else None,
                        host=platform.node(), kernel=platform.release(), platform_version=platform.version(), governors=governors,
                        load_average=os.getloadavg() if hasattr(os, "getloadavg") else None, sources=sources, binaries=binaries,
                        raw_sources={name_: digest(REPO / name_) for name_ in sources},
                        commit=capture(["git", "-c", f"safe.directory={REPO}", "-C", REPO, "rev-parse", "HEAD"]).strip())
        metadata_path = out / "metadata.json"
        metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")
        runner = SimpleNamespace(out=out, config=config, devices=devices, args=args, workload=config["workload"], mount_settings={})
        rows = []
        original_emit = bench.emit
        original_os, original_sync = bench.os, bench.sync_path
        try:
            if sys.platform != "linux":
                from benchmarks import native_io
                bench.os = SimpleNamespace(**dict(vars(os), open=native_io.open_fd, fsync=native_io.flush_fd))
                bench.sync_path = native_io.sync_path
            sample_type = Sample if sys.platform == "linux" else native.Sample
            for disk in config["disks"]:
                for round_ in range(1, config["rounds"] + 1):
                    targets = disk["filesystems"]
                    shift = (round_ - 1) % len(targets)
                    for target in targets[shift:] + targets[:shift]:
                        print(f"{disk['name']}, round {round_}/{config['rounds']}: {target['name']}", flush=True)
                        sample = sample_type(runner, disk["name"], target, round_)
                        bench.emit = sample.emit
                        try:
                            sample.prepare()
                            sample.workload()
                        finally:
                            sample.close()
                        rows.extend(sample_rows(sample.folder / "results.jsonl", config["workload"], config.get("selection")))
                        (sample.folder / "complete").write_text("namespace, SHA-256 and offline checks passed\n")
        finally:
            bench.emit = original_emit
            bench.os, bench.sync_path = original_os, original_sync
        text = report_heading(config) + "\n\n".join(table(rows, disk["name"]) for disk in config["disks"])
        (out / "summary.md").write_text(text + "\n")
        with (out / "results.tsv").open("w", newline="") as sink:
            writer = csv.DictWriter(sink, fieldnames=("disk", "label", "round", "phase", "metric", "value", "unit", "status", "reason"),
                                    delimiter="\t", extrasaction="ignore")
            writer.writeheader()
            writer.writerows(rows)
        metadata["complete"] = True
        metadata["mount_settings"] = runner.mount_settings
        metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")
        print(f"Benchmark passed. Summary: {out / 'summary.md'}", flush=True)
    finally:
        if sys.platform == "win32":
            native.windows_unlock(lock)
        else:
            os.close(lock)
