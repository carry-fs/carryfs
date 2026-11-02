#!/usr/bin/env python3
"""End-to-end check of the CarryFS kernel driver, judged by the userspace
core: every phase ends with `carryfs check`, and data phases with `scrub`
and a byte comparison after remount.

Needs root, the module loaded (`make build && insmod carryfs.ko`), and the
`carryfs` CLI (`cargo build --release -p carryfs-core`). Images live in a
temporary directory on loop devices.
"""

import argparse
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


class Volume:
    """A fresh image, mounted through the kernel driver while in use."""

    def __init__(self, cli, work, name, mib):
        self.cli, self.image, self.point = cli, work / f"{name}.img", work / f"mnt-{name}"
        self.point.mkdir(exist_ok=True)
        run(cli, "mkfs", self.image, mib << 20, stdout=subprocess.DEVNULL)
        self.loop = None

    def mount(self, mode="rw"):
        self.loop = subprocess.check_output(["losetup", "-f", "--show", self.image], text=True).strip()
        # -i: an installed mount.carryfs helper would mount through FUSE.
        run("mount", "-i", "-t", "carryfs", "-o", mode, self.loop, self.point)
        return self.point

    def unmount(self):
        run("umount", self.point)
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
    volume.unmount()
    assert used < 2 * 8_000_000 // 4096, f"{used} blocks still used"
    print(f"  reclaim: {used} blocks in use after 12 rewrites of 1954; {volume.check()}")


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
        work = Path(directory)
        print("namespace")
        namespace(Volume(args.cli, work, "namespace", 1024), args.entries)
        print("listings")
        listings(Volume(args.cli, work, "listings", 512))
        print("data")
        data(Volume(args.cli, work, "data", 512), work)
        print("reclaim")
        reclaim(Volume(args.cli, work, "reclaim", 512))
        print("orphan")
        orphan(Volume(args.cli, work, "orphan", 512), work)
    print("all checks passed")


def module_loaded():
    return any(line.split()[-1] == "carryfs" for line in Path("/proc/filesystems").read_text().splitlines())


if __name__ == "__main__":
    main()
