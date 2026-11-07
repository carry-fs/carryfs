#!/usr/bin/env python3
"""End-to-end check of the CarryFS kernel driver, judged by the userspace
core: every phase ends with `carryfs check`, and data phases with `scrub`
and a byte comparison after remount.

Needs root, the module loaded (`make build && insmod carryfs.ko`), and the
`carryfs` CLI (`cargo build --release -p carryfs-core`). Images live in a
temporary directory on loop devices.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import errno
import hashlib
import itertools
import mmap
import os
import random
import resource
import shutil
import signal
import struct
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


def case_names(volume):
    """Names match ignoring case by their comparison keys, through the dcache
    and through the catalog: `d_hash` must give a name and its key the same
    hash, or a lookup by another spelling misses the cached dentry."""
    pairs = [("A" * n, "a" * n) for n in (1, 7, 8, 9, 15, 16, 17)]
    pairs += [("Straße-0123456789", "STRASSE-0123456789"), ("\u212aelvin", "kELVIN"),
              ("Ärger", "äRGER")]
    root = volume.mount()
    for stored, other in pairs:
        (root / stored).write_text(stored)
    stored_names = sorted(os.listdir(root))
    for cold in (False, True):
        if cold:
            volume.unmount()
            root = volume.mount()
        for stored, other in pairs:
            fd = os.open(root / other, os.O_RDONLY)
            try:
                found = os.readlink(f"/proc/self/fd/{fd}").rsplit("/", 1)[1]
                assert os.read(fd, 100).decode() == stored, (other, cold)
            finally:
                os.close(fd)
            # A warm lookup finds the cached dentry, which carries the
            # spelling it was created or listed under, not the one looked up.
            assert cold or found in (stored, *stored_names), (other, found)
        # Dotless i has no ASCII case partner.
        assert not (root / "\u0131d").exists() and not (root / "ID").exists()
    assert sorted(os.listdir(root)) == stored_names
    volume.unmount()
    print(f"  case names: {volume.check()}")


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


def namespace_times(volume):
    """Open descriptors observe namespace ctime changes without inode eviction."""
    root = volume.mount()
    path = root / "before"
    path.write_bytes(b"content")
    with path.open("rb") as held:
        before = os.fstat(held.fileno()).st_ctime_ns
        time.sleep(0.02)
        path.rename(root / "after")
        renamed = os.fstat(held.fileno()).st_ctime_ns
        assert renamed > before, "rename left cached ctime unchanged"
        assert (root / "after").stat().st_ctime_ns == renamed
        time.sleep(0.02)
        (root / "after").unlink()
        assert os.fstat(held.fileno()).st_ctime_ns > renamed, "unlink left cached ctime unchanged"
    volume.unmount()
    print(f"  namespace times: {volume.check()}")


def writeback_times(volume, iterations=100):
    """Metadata timestamps survive completion of an already-started data write."""
    root = volume.mount()
    expected = {}
    with ThreadPoolExecutor(max_workers=2) as pool:
        for iteration in range(iterations):
            for action in ("rename", "unlink", "replace", "mtime"):
                path = root / "source"
                fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_TRUNC, 0o644)
                victim = None
                try:
                    assert os.write(fd, b"x" * 65536) == 65536
                    before = os.fstat(fd).st_ctime_ns
                    syncing = pool.submit(os.fsync, fd)
                    if action == "rename":
                        destination = root / f"renamed-{iteration}"
                        path.rename(destination)
                    elif action == "unlink":
                        path.unlink()
                        destination = None
                    elif action == "replace":
                        destination = root / "destination"
                        destination.write_bytes(b"old")
                        victim = os.open(destination, os.O_RDONLY)
                        victim_before = os.fstat(victim).st_ctime_ns
                        victim_syncing = pool.submit(os.fsync, victim)
                        path.replace(destination)
                    else:
                        destination = root / f"mtime-{iteration}"
                        # A backdated mtime must not be replaced by the write's time.
                        mtime = 946684800_000000000 + iteration
                        os.utime(path, ns=(mtime, mtime))
                    syncing.result(timeout=30)
                    os.fsync(fd)
                    if victim is not None:
                        victim_syncing.result(timeout=30)
                        os.fsync(victim)
                        assert os.fstat(victim).st_ctime_ns > victim_before, "writeback lost victim ctime"
                    after = os.fstat(fd)
                    assert after.st_ctime_ns > before, f"{action}: writeback lost ctime"
                    if action == "mtime":
                        assert after.st_mtime_ns == mtime, "writeback lost explicit mtime"
                        path.rename(destination)
                        after = os.fstat(fd)
                    if destination is not None:
                        expected[destination.name] = (after.st_mtime_ns, after.st_ctime_ns)
                finally:
                    if victim is not None:
                        os.close(victim)
                    os.close(fd)
    volume.unmount()
    root = volume.mount("ro")
    for name, times in expected.items():
        observed = (root / name).stat()
        assert (observed.st_mtime_ns, observed.st_ctime_ns) == times, name
    volume.unmount()
    print(f"  writeback times: {iterations} iterations of rename, unlink, replacement, and mtime; {volume.check(scrub=True)}")


def concurrent_operations(volume):
    """Concurrent namespace, mmap, size, and writeback operations survive remount."""
    root = volume.mount()

    def lane(index):
        path = root / f"lane-{index}"
        for iteration in range(24):
            data = bytes([index, iteration]) * 4096
            with path.open("w+b") as handle:
                handle.write(data)
                handle.flush()
                handle.truncate(65536)
                with mmap.mmap(handle.fileno(), 65536) as mapped:
                    mapped[32768:32768 + len(data)] = data
                os.fsync(handle.fileno())
                moved = root / f"moved-{index}"
                path.rename(moved)
                moved.unlink()
                handle.write(data)
            with path.open("ab") as handle:
                handle.write(data)
        return path.name, data

    # Hold a directory descriptor so unmount attempts must fail even if the
    # worker threads finish before the mount utility reaches the syscall.
    held = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
    try:
        with ThreadPoolExecutor(max_workers=4) as pool:
            futures = [pool.submit(lane, index) for index in range(4)]
            for _ in range(4):
                result = subprocess.run(["umount", root], capture_output=True)
                assert result.returncode != 0 and os.path.ismount(root), "unmounted a busy volume"
                os.fsync(held)
            expected = dict(future.result(timeout=120) for future in futures)
    finally:
        os.close(held)
    volume.unmount()
    root = volume.mount("ro")
    for name, data in expected.items():
        assert (root / name).read_bytes() == data, f"concurrent lane {name} differs after remount"
    volume.unmount()
    print(f"  concurrent operations: {volume.check(scrub=True)}")


def fragmented_orphan(volume):
    """Deletion spans batches of nonadjacent backed checksum units."""
    root = volume.mount()
    path = root / "fragmented"
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        for index in range(600):
            assert os.pwrite(fd, bytes([index % 256]) * 4096, index * 2 * 65536) == 4096
        os.fsync(fd)
    finally:
        os.close(fd)
    path.unlink()
    os.sync()
    volume.unmount()
    print(f"  fragmented orphan: {volume.check(scrub=True)}")


def unsupported_mount_options(volume):
    """Reject unsupported durability flags on initial mounts and remounts."""
    loop = subprocess.check_output(["losetup", "-f", "--show", volume.image], text=True).strip()
    try:
        for option in ("sync", "dirsync"):
            result = subprocess.run(["mount", "-i", "-t", "carryfs", "-o", option, loop, volume.point],
                                    capture_output=True)
            if result.returncode == 0:
                run("umount", volume.point)
                raise AssertionError(f"unsupported {option} mount succeeded")
    finally:
        run("losetup", "-d", loop)
    volume.mount()
    for option in ("sync", "dirsync"):
        result = subprocess.run(["mount", "-i", "-o", f"remount,{option}", volume.point], capture_output=True)
        assert result.returncode != 0, f"unsupported {option} remount succeeded"
    volume.unmount()
    print(f"  unsupported mount options: {volume.check()}")


def corrupt_records(volume):
    """Refuse cross-record corruption before repairing an invalid root mirror."""
    from fuzz import reseal

    root = volume.mount()
    for name in ("first", "second"):
        (root / name).write_bytes(b"x" * 65536)
    volume.unmount()

    def records(page):
        offset = 80
        for _ in range(struct.unpack_from("<H", page, 32)[0]):
            key_len, value_len = struct.unpack_from("<HH", page, offset)
            value = offset + 4 + key_len
            yield page[offset + 4:value], value, value_len
            offset = value + value_len

    baseline = volume.image.with_suffix(".baseline")
    run("cp", "--sparse=always", volume.image, baseline)
    for case in ("allocation count", "data cross-link", "directory cross-link", "allocation bitmap", "detached tree"):
        run("cp", "--sparse=always", baseline, volume.image)
        with volume.image.open("r+b") as image:
            def read(block):
                image.seek(block * 4096)
                return bytearray(image.read(4096))

            def write(block, page):
                reseal(page)
                image.seek(block * 4096)
                image.write(page)

            root = read(256)
            catalog = struct.unpack_from("<Q", root, 64)[0]
            page = read(catalog)
            assert page[14] == 0, "fixture catalog must fit in one leaf"
            inodes = [value for key, value, _ in records(page) if key[8] == 1 and page[value] == 1]
            entries = [value for key, value, _ in records(page) if key[8] == 2]
            extents = [value for key, value, _ in records(page) if key[8] == 3]
            assert len(inodes) == len(entries) == len(extents) == 2
            first = struct.unpack_from("<Q", page, extents[0])[0]
            if case == "allocation count":
                struct.pack_into("<Q", page, inodes[0] + 24, 0)
                write(catalog, page)
            elif case in ("data cross-link", "directory cross-link"):
                values = extents if case == "data cross-link" else entries
                page[values[1]:values[1] + 8] = page[values[0]:values[0] + 8]
                write(catalog, page)
            elif case == "allocation bitmap":
                block = struct.unpack_from("<Q", root, 80)[0]
                while True:
                    page = read(block)
                    candidates = [(key, value) for key, value, _ in records(page)
                                  if int.from_bytes(key, "big") <= first // 16384]
                    key, value = candidates[-1]
                    if page[14] == 0:
                        assert int.from_bytes(key, "big") == first // 16384
                        page[value + first % 16384 // 8] ^= 1 << (first % 8)
                        write(block, page)
                        break
                    block = struct.unpack_from("<Q", page, value)[0]
            else:
                block = struct.unpack_from("<Q", root, 96)[0]
                page = read(block)
                assert page[14] == 0, "fixture cleanup must fit in one leaf"
                jobs = [(bytes(key), bytes(page[value:value + length])) for key, value, length in records(page)]
                job = struct.unpack_from("<Q", root, 152)[0]
                jobs.append((b"\x02" + job.to_bytes(8, "big"),
                             struct.pack("<QBB6x", 2, 1, 4) + root[64:80]))
                payload = b"".join(struct.pack("<HH", len(key), len(value)) + key + value
                                   for key, value in sorted(jobs))
                page[80:] = payload + bytes(4096 - 80 - len(payload))
                struct.pack_into("<HH", page, 32, len(jobs), len(payload))
                generation = struct.unpack_from("<Q", root, 48)[0]
                struct.pack_into("<Q", page, 24, generation)
                struct.pack_into("<Q", root, 104, generation)
                write(block, page)
                struct.pack_into("<Q", root, 152, job + 1)
            write(256, root)
            blocks = volume.image.stat().st_size // 4096
            image.seek((blocks - 257) * 4096 + 4092)
            crc = bytearray(image.read(4))
            crc[0] ^= 1
            image.seek((blocks - 257) * 4096 + 4092)
            image.write(crc)
        checked = subprocess.run([volume.cli, "check", volume.image], capture_output=True, text=True)
        assert checked.returncode != 0, f"userspace checker accepted {case}"
        with volume.image.open("rb") as image:
            before = hashlib.file_digest(image, "sha256").digest()
        loop = subprocess.check_output(["losetup", "-f", "--show", volume.image], text=True).strip()
        try:
            result = subprocess.run(["mount", "-i", "-t", "carryfs", loop, volume.point], capture_output=True)
            if result.returncode == 0:
                run("umount", volume.point)
                raise AssertionError(f"native driver accepted {case}")
        finally:
            run("losetup", "-d", loop)
        with volume.image.open("rb") as image:
            assert hashlib.file_digest(image, "sha256").digest() == before, f"{case}: failed mount changed the image"
        print(f"  {case}: rejected without writes or root repair; {checked.stderr.strip()}")


def direct_append(volume):
    """An O_DIRECT append writes back the EOF range before returning."""
    root = volume.mount()
    path = root / "append"
    payload = os.urandom(2 << 20)
    path.write_bytes(payload)
    with path.open("rb") as held:
        os.fsync(held.fileno())
    before = path.stat().st_blocks
    tail = os.urandom(65536)
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_DIRECT)
    try:
        assert os.lseek(fd, 0, os.SEEK_CUR) == 0
        assert os.write(fd, tail) == len(tail)
        assert os.fstat(fd).st_blocks >= before + len(tail) // 512, "direct append was not written back"
    finally:
        os.close(fd)
    limit = path.stat().st_size + 8192
    previous_limit = resource.getrlimit(resource.RLIMIT_FSIZE)
    try:
        resource.setrlimit(resource.RLIMIT_FSIZE, (limit, previous_limit[1]))
        fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_DIRECT)
        try:
            assert os.write(fd, tail) == 8192, "expected a partial direct append at the file-size limit"
            assert os.fstat(fd).st_blocks >= before + (len(tail) + 8192) // 512
        finally:
            os.close(fd)
    finally:
        resource.setrlimit(resource.RLIMIT_FSIZE, previous_limit)
    volume.unmount()
    root = volume.mount("ro")
    assert (root / "append").read_bytes() == payload + tail + tail[:8192]
    volume.unmount()
    print(f"  direct append: {volume.check(scrub=True)}")


TRACING = Path("/sys/kernel/tracing")


def folio_orders(path):
    """Read `path` cold and return the orders of the page-cache folios the
    read added, from the filemap tracepoint."""
    ino = os.stat(path).st_ino
    fd = os.open(path, os.O_RDONLY)
    os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    event = TRACING / "events/filemap/mm_filemap_add_to_page_cache"
    was_on = (TRACING / "tracing_on").read_text().strip()
    (TRACING / "trace").write_text("")
    (event / "filter").write_text(f"i_ino == {ino}")
    (event / "enable").write_text("1")
    (TRACING / "tracing_on").write_text("1")
    try:
        while os.read(fd, 1 << 20):
            pass
    finally:
        (TRACING / "tracing_on").write_text(was_on)
        (event / "enable").write_text("0")
        (event / "filter").write_text("0")
        os.close(fd)
    return [int(field.split("=")[1]) for line in (TRACING / "trace").read_text().splitlines()
            for field in line.split() if field.startswith("order=")]


def large_folios(volume):
    """A file read cold sits in folios of up to one 64 KiB checksum unit,
    and writes, mmap writes, and truncation inside such folios keep every
    byte, across a remount. Readahead grows folio orders as it goes, so the
    changes are made 12 MiB into a 16 MiB file, past the first windows."""
    size, base = 16 << 20, 12 << 20
    model = bytearray(os.urandom(size))
    root = volume.mount()
    path = root / "large"
    path.write_bytes(model)
    os.sync()
    volume.unmount()
    root = volume.mount()
    path = root / "large"
    orders = folio_orders(path)
    assert max(orders, default=0) >= 2, f"no large folios: orders {sorted(set(orders))}"
    assert max(orders) <= 4, f"a folio larger than a unit: orders {sorted(set(orders))}"
    with open(path, "r+b") as f:
        for offset, length in ((8192, 4096), (70_000, 100), (65536 - 10, 20)):
            data = os.urandom(length)
            f.seek(base + offset)
            f.write(data)
            model[base + offset:base + offset + length] = data
        f.flush()
        with mmap.mmap(f.fileno(), size) as mapped:
            mapped[base + 130_000:base + 130_500] = b"m" * 500
        model[base + 130_000:base + 130_500] = b"m" * 500
        os.fsync(f.fileno())
    end = base + 200_123
    os.truncate(path, end)
    del model[end:]
    with open(path, "r+b") as f:
        f.seek(base + 300_000)
        f.write(b"grown")
    model.extend(bytes(base + 300_000 - len(model)))
    model.extend(b"grown")
    assert path.read_bytes() == model, "content differs before remount"
    volume.unmount()
    root = volume.mount()
    assert (root / "large").read_bytes() == model, "content differs after remount"
    volume.unmount()
    print(f"  large folios (orders {sorted(set(orders))}): {volume.check(scrub=True)}")


def fsx(volume, operations, seed):
    """Random writes, truncates, mmap writes, reads, fsyncs, cache drops and
    remounts on one file, compared with an in-memory copy, as xfstests' fsx
    does. Whole-file reads after cache drops let readahead build large
    folios for the changes after them. Prints the seed, which reproduces a
    failure."""
    rng = random.Random(seed)
    limit = 8 << 20
    model = bytearray()
    root = volume.mount()
    path = root / "fsx"
    path.write_bytes(b"")
    for step in range(operations):
        choice = rng.random()
        if choice < 0.35:
            offset, length = rng.randrange(limit), rng.randrange(1, 70_000)
            data = rng.randbytes(length)
            with open(path, "r+b") as f:
                f.seek(offset)
                f.write(data)
            if offset > len(model):
                model.extend(bytes(offset - len(model)))
            model[offset:offset + length] = data
        elif choice < 0.45:
            size = rng.randrange(limit)
            os.truncate(path, size)
            del model[size:]
            model.extend(bytes(size - len(model)))
        elif choice < 0.6 and model:
            offset = rng.randrange(len(model))
            length = rng.randrange(1, min(20_000, len(model) - offset) + 1)
            data = rng.randbytes(length)
            with open(path, "r+b") as f, mmap.mmap(f.fileno(), len(model)) as mapped:
                mapped[offset:offset + length] = data
            model[offset:offset + length] = data
        elif choice < 0.65:
            assert path.read_bytes() == model, f"seed {seed} step {step}: content differs"
        elif choice < 0.8 and model:
            offset = rng.randrange(len(model))
            length = rng.randrange(1, 200_000)
            with open(path, "rb") as f:
                f.seek(offset)
                got = f.read(length)
            assert got == bytes(model[offset:offset + length]), f"seed {seed} step {step}: read differs"
        elif choice < 0.9:
            with open(path, "r+b") as f:
                os.fsync(f.fileno())
        elif choice < 0.98:
            fd = os.open(path, os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            os.close(fd)
        else:
            volume.unmount()
            root = volume.mount()
            path = root / "fsx"
        assert os.path.getsize(path) == len(model), f"seed {seed} step {step}: size differs"
    assert path.read_bytes() == model, f"seed {seed}: content differs"
    volume.unmount()
    root = volume.mount()
    assert (root / "fsx").read_bytes() == model, f"seed {seed}: content differs after remount"
    volume.unmount()
    print(f"  fsx seed {seed}, {operations} operations: {volume.check(scrub=True)}")


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


def fsync_crash(volume, work):
    """A snapshot after fsync contains the synced file.
    This does not simulate interrupted device writes or flushes.
    """
    root = volume.mount()
    (root / "before").write_bytes(b"b")
    os.sync()
    payload = os.urandom(200_000)
    with open(root / "synced", "wb") as f:
        f.write(payload)
        os.fsync(f.fileno())
    crashed = work / "fsync-crashed.img"
    run("cp", "--sparse=always", volume.image, crashed)
    with crashed.open("rb") as image:
        blocks = crashed.stat().st_size // 4096
        image.seek(256 * 4096)
        a = bytearray(image.read(4096))
        image.seek((blocks - 257) * 4096)
        b = bytearray(image.read(4096))
        for page in (a, b):
            page[40:48] = bytes(8)
            page[4092:4096] = bytes(4)
        assert a == b, "fsync returned before root B mirrored root A"
    volume.unmount()
    copy = Volume.__new__(Volume)
    copy.cli, copy.image, copy.point, copy.loop = volume.cli, crashed, volume.point, None
    result = copy.check()
    root = copy.mount("ro")
    assert (root / "synced").read_bytes() == payload
    copy.unmount()
    print(f"  fsync crash: synced file kept; {result}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[2] / "target/release/carryfs")
    parser.add_argument("--entries", type=int, default=3000)
    parser.add_argument("--fsx-operations", type=int, default=2000)
    parser.add_argument("--seed", type=int, default=random.randrange(1 << 32))
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
    print("case names")
    case_names(Volume(args.cli, work, "case-names", 512))
    print("namespace times")
    namespace_times(Volume(args.cli, work, "namespace-times", 512))
    print("writeback times")
    writeback_times(Volume(args.cli, work, "writeback-times", 512))
    print("concurrent operations")
    concurrent_operations(Volume(args.cli, work, "concurrent", 512))
    print("fragmented orphan")
    fragmented_orphan(Volume(args.cli, work, "fragmented-orphan", 512))
    print("corrupt records")
    corrupt_records(Volume(args.cli, work, "corrupt-records", 256))
    print("unsupported mount options")
    unsupported_mount_options(Volume(args.cli, work, "unsupported-options", 256))
    print("direct append")
    direct_append(Volume(args.cli, work, "direct-append", 512))
    print("large folios")
    large_folios(Volume(args.cli, work, "large-folios", 512))
    print("fsx")
    fsx(Volume(args.cli, work, "fsx", 512), args.fsx_operations, args.seed)
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
    print("fsync crash")
    fsync_crash(Volume(args.cli, work, "fsync-crash", 512), work)


def module_loaded():
    return any(line.split()[-1] == "carryfs" for line in Path("/proc/filesystems").read_text().splitlines())


if __name__ == "__main__":
    main()
