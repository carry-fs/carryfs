#!/usr/bin/env python3
"""End-to-end check of the CarryFS kernel driver, judged by the userspace
core: every phase ends with `carryfs check`, and data phases with `scrub`
and a byte comparison after remount.

Needs root, the module loaded (`make build && insmod carryfs.ko`), and the
`carryfs` CLI (`cargo build --release -p carryfs-core`). Images live in a
temporary directory on loop devices.
"""

import argparse
import errno
import hashlib
import itertools
import os
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


class Volume:
    """A fresh image, mounted through the kernel driver while in use."""

    # Volumes still mounted, which a failed check leaves for `main`.
    mounted = set()

    def __init__(self, cli, work, name, mib):
        self.cli, self.image, self.point = cli, work / f"{name}.img", work / f"mnt-{name}"
        self.point.mkdir(exist_ok=True)
        run(cli, "mkfs", self.image, mib << 20, stdout=subprocess.DEVNULL)
        self.loop = None

    def mount(self, mode="rw"):
        self.loop = subprocess.check_output(["losetup", "-f", "--show", self.image], text=True).strip()
        # -i: an installed mount.carryfs helper would mount through FUSE.
        run("mount", "-i", "-t", "carryfs", "-o", mode, self.loop, self.point)
        Volume.mounted.add(self)
        return self.point

    def unmount(self):
        run("umount", self.point)
        Volume.mounted.discard(self)
        run("losetup", "-d", self.loop)
        backing = Path("/sys/block") / Path(self.loop).name / "loop/backing_file"
        while backing.exists():
            time.sleep(0.02)
        self.loop = None

    def check(self, scrub=False):
        for action in ("check", "scrub") if scrub else ("check",):
            result = subprocess.run([self.cli, action, self.image], capture_output=True, text=True)
            if result.returncode:
                sys.exit(f"{self.image.name}: carryfs {action} failed:\n{result.stdout}{result.stderr}")
        return result.stdout.splitlines()[0]


def namespace(volume, count):
    """Creates, renames across directories, and removals that split,
    merge, and collapse catalog pages."""
    random.seed(7)
    expected = set()
    for phase in range(4):
        root = volume.mount()
        for directory in ("a", "b"):
            listed = sorted(os.listdir(root / directory)) if (root / directory).exists() else []
            assert listed == sorted(n.split("/")[1] for n in expected if n.startswith(directory + "/")), phase
        (root / "a").mkdir(exist_ok=True)
        (root / "b").mkdir(exist_ok=True)
        if phase == 0:
            for i in range(count):
                name = f"a/entry-{i:06d}-{'x' * (i % 40)}"
                (root / name).touch()
                expected.add(name)
            os.symlink("entry-000000-", root / "a/link")
            expected.add("a/link")
        elif phase == 1:
            names = sorted(n for n in expected if n.startswith("a/"))
            random.shuffle(names)
            for name in names[: len(names) // 2]:
                moved = "b/" + name[2:].upper()
                os.rename(root / name, root / moved)
                expected.discard(name)
                expected.add(moved)
        elif phase == 2:
            names = sorted(expected)
            random.shuffle(names)
            for name in names[: len(names) * 9 // 10]:
                os.unlink(root / name)
                expected.discard(name)
        else:
            for name in sorted(expected):
                os.unlink(root / name)
            expected.clear()
            os.rmdir(root / "a")
            os.rmdir(root / "b")
        volume.unmount()
        print(f"  namespace phase {phase}: {volume.check()}")


def listings(volume):
    """Listings kept on directory inodes follow every change while mounted,
    including a change between the calls of one listing."""
    root = volume.mount()
    for name in ("a", "b"):
        (root / name).mkdir()
    expected = {"a": set(), "b": set()}

    def same():
        for name, entries in expected.items():
            listed = sorted(os.listdir(root / name))
            assert listed == sorted(entries), (name, len(listed), len(entries))

    for i in range(300):
        (root / "a" / f"f{i}").touch()
        expected["a"].add(f"f{i}")
    same()
    same()
    (root / "a" / "new").touch()
    expected["a"].add("new")
    same()
    os.unlink(root / "a" / "f7")
    expected["a"].discard("f7")
    same()
    os.rename(root / "a" / "f8", root / "b" / "moved")
    expected["a"].discard("f8")
    expected["b"].add("moved")
    same()
    # Change the directory part-way through one listing.
    with os.scandir(root / "a") as entries:
        next(entries)
        (root / "a" / "during").touch()
        expected["a"].add("during")
        list(entries)
    same()
    same()
    # A directory spread over committed and draft pages, listed through
    # small getdents buffers: a pass may reach its end mid-batch.
    names = [f"{a}{b}{c}{d}{e}{f}" for a in "abcd" for b in "abcd" for c in "abcd"
             for d in "abcd" for e in "abcd" for f in "abcd"]
    (root / "c").mkdir()
    for index, name in enumerate(names):
        (root / "c" / name).touch()
        if index == 500:
            os.sync()
    for _ in range(3):
        assert sorted(os.listdir(root / "c")) == names
    volume.unmount()
    print(f"  listings: {volume.check()}")


def data(volume, work):
    """Writes, overwrites, appends, sparse growth, and truncation, compared
    with the same changes made to a local copy."""
    source = work / "source"
    shutil.rmtree(source, ignore_errors=True)
    source.mkdir()
    for size in (0, 1, 4095, 4096, 65535, 65536, 65537, 131072, 1048699, 3_000_000):
        (source / f"size-{size}").write_bytes(os.urandom(size))
    with open(source / "sparse", "wb") as sparse:
        sparse.truncate(40 << 20)
        sparse.seek(30_000_000)
        sparse.write(b"middle")
    root = volume.mount()
    run("cp", "-a", f"{source}/.", root)

    def both(name, change):
        for base in (root, source):
            change(base / name)

    def overwrite(path):
        with open(path, "r+b") as f:
            f.seek(70_000)
            f.write(b"Z" * 5000)

    def far(path):
        with open(path, "r+b") as f:
            f.seek(5 * 65536 + 17)
            f.write(b"far")

    both("size-3000000", overwrite)
    both("size-4095", lambda p: p.open("ab").write(b"A" * 70_000))
    both("tail", lambda p: p.write_bytes(b"t" * 100))
    both("tail", far)
    both("size-1048699", lambda p: os.truncate(p, 200_000))
    both("size-131072", lambda p: os.truncate(p, 70_001))
    both("size-65537", lambda p: os.truncate(p, 300_000))
    # A writeback batch of 64 units that ends right before the stored
    # partial tail unit, which the rewrite grows. This used to hang
    # writeback (fstests generic/551).
    unit = 65536
    first, second = os.urandom(64 * unit + unit // 2), os.urandom(66 * unit)

    def batch_tail(path):
        path.write_bytes(first)
        os.sync()
        with open(path, "r+b") as f:
            f.write(second)
            os.fsync(f.fileno())

    both("batch-tail", batch_tail)
    volume.unmount()
    print(f"  data: {volume.check(scrub=True)}")
    root = volume.mount("ro")
    run("diff", "-r", "--no-dereference", source, root)
    volume.unmount()
    print("  data: content matches after remount")


def reclaim(volume):
    """Rewrites retire their old blocks; later commits reclaim them."""
    root = volume.mount()
    before = os.statvfs(root).f_bfree
    for _ in range(12):
        (root / "rewrite").write_bytes(os.urandom(8_000_000))
        os.sync()
    time.sleep(6)
    used = before - os.statvfs(root).f_bfree
    assert used < 2 * 8_000_000 // 4096, f"{used} blocks still used"
    # A deleted file retires one range larger than a reclaim batch (65,536
    # blocks); it must drain too, not stall all reclaim behind it.
    with open(root / "big", "wb") as f:
        for _ in range(300):
            f.write(os.urandom(1 << 20))
    os.sync()
    os.unlink(root / "big")
    os.sync()
    deadline = time.monotonic() + 60
    while before - os.statvfs(root).f_bfree > 2 * 8_000_000 // 4096:
        assert time.monotonic() < deadline, f"{before - os.statvfs(root).f_bfree} blocks still used"
        time.sleep(1)
    volume.unmount()
    print(f"  reclaim: {used} blocks in use after 12 rewrites of 1954, 300 MiB delete drained; {volume.check()}")


def fill(path, chunk):
    """Append synced chunks to `path` until one fails with ENOSPC.
    Unbuffered, so a failed chunk is not retried at close."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    try:
        while True:
            try:
                if os.write(fd, os.urandom(chunk)) < chunk:
                    return
                os.fsync(fd)
            except OSError as error:
                assert error.errno == errno.ENOSPC, error
                return
    finally:
        os.close(fd)


def full_write(volume):
    """Writes to a full volume fail with ENOSPC instead of being accepted
    and lost at writeback; dirtied units' space reservations all return."""
    root = volume.mount()
    (root / "mapped").write_bytes(os.urandom(65536))
    os.sync()
    start = os.statvfs(root).f_bavail
    digest, written = hashlib.sha256(), 0
    fd = os.open(root / "fill", os.O_WRONLY | os.O_CREAT, 0o644)
    try:
        while True:
            chunk = os.urandom(1 << 20)
            count = os.write(fd, chunk)
            digest.update(chunk[:count])
            written += count
            assert written <= start * 4096, "writes accepted past the free space"
    except OSError as error:
        assert error.errno == errno.ENOSPC, error
    # Every accepted byte had space reserved, so writeback finds it.
    os.fsync(fd)
    os.close(fd)
    assert written > start * 4096 // 2, f"only {written} bytes fit"
    # A shared mapping dirtying a clean unit needs space too: SIGBUS.
    mapped = subprocess.run([sys.executable, "-c", (
        "import mmap, sys\n"
        "with open(sys.argv[1], 'r+b') as f:\n"
        "    m = mmap.mmap(f.fileno(), 0)\n"
        "    m[0] = 1\n"
    ), root / "mapped"])
    assert mapped.returncode == -signal.SIGBUS, mapped.returncode
    volume.unmount()
    root = volume.mount("ro")
    data = (root / "fill").read_bytes()
    assert len(data) == written and hashlib.sha256(data).digest() == digest.digest()
    volume.unmount()
    root = volume.mount()
    os.unlink(root / "fill")
    os.sync()
    deadline = time.monotonic() + 60
    while os.statvfs(root).f_bavail < start - 64:
        assert time.monotonic() < deadline, f"{start - os.statvfs(root).f_bavail} blocks not returned"
        os.sync()
        time.sleep(1)
    volume.unmount()
    print(f"  full write: {written >> 20} MiB accepted and kept; {volume.check()}")


def full_truncate(volume):
    """A truncate whose tail rewrite fails for space leaves the stored size
    past i_size; a later writeback of the file must not store i_size over
    the full tail unit (fstests generic/476)."""
    root = volume.mount()
    path = root / "file"
    path.write_bytes(os.urandom(2 * 65536))
    os.sync()
    # Fill with data, then with empty files, which may use the 8,192 blocks
    # of headroom data writes leave (write.rs HEADROOM), until the 9 blocks
    # of the tail unit's rewrite no longer fit.
    fill(root / "filler", 1 << 20)
    for i in itertools.count():
        if os.statvfs(root).f_bavail < 8192 + 9:
            break
        (root / f"empty{i}").touch()
    try:
        os.truncate(path, 100_000)
        raise AssertionError("truncate had space for its tail rewrite")
    except OSError as error:
        assert error.errno == errno.ENOSPC, error
    for name in os.listdir(root):
        if name != "file":
            os.unlink(root / name)
    os.sync()
    with open(path, "r+b") as f:
        f.write(b"x")
        os.fsync(f.fileno())
    volume.unmount()
    print(f"  full truncate: {volume.check()}")


def orphan(volume, work):
    """An unlinked file still open is durable as an orphan; mounting a copy
    of that state removes it."""
    root = volume.mount()
    (root / "keep").write_bytes(b"k")
    (root / "orphan").write_bytes(os.urandom(3_000_000))
    os.sync()
    held = open(root / "orphan", "rb")
    os.unlink(root / "orphan")
    os.sync()
    crashed = work / "crashed.img"
    run("cp", "--sparse=always", volume.image, crashed)
    held.close()
    volume.unmount()
    copy = Volume.__new__(Volume)
    copy.cli, copy.image, copy.point, copy.loop = volume.cli, crashed, volume.point, None
    assert os.listdir(copy.mount()) == ["keep"]
    copy.unmount()
    print(f"  orphan: removed at mount; {copy.check()}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[2] / "target/release/carryfs")
    parser.add_argument("--entries", type=int, default=3000)
    args = parser.parse_args()
    if not module_loaded():
        sys.exit("load the module first: make build && insmod carryfs.ko")
    with tempfile.TemporaryDirectory(prefix="carryfs-kernel-") as directory:
        try:
            checks(args, Path(directory))
        finally:
            # A failed check leaves its volume mounted.
            for volume in list(Volume.mounted):
                volume.unmount()
    print("all checks passed")


def checks(args, work):
    print("namespace")
    namespace(Volume(args.cli, work, "namespace", 1024), args.entries)
    print("listings")
    listings(Volume(args.cli, work, "listings", 512))
    print("data")
    data(Volume(args.cli, work, "data", 512), work)
    print("reclaim")
    reclaim(Volume(args.cli, work, "reclaim", 512))
    print("full write")
    full_write(Volume(args.cli, work, "full-write", 512))
    print("full truncate")
    full_truncate(Volume(args.cli, work, "full-truncate", 512))
    print("orphan")
    orphan(Volume(args.cli, work, "orphan", 512), work)


def module_loaded():
    return any(line.split()[-1] == "carryfs" for line in Path("/proc/filesystems").read_text().splitlines())


if __name__ == "__main__":
    main()
