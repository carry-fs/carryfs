#!/usr/bin/env python3
"""Measure one mounted filesystem, for comparison against other filesystems.

Sequential copying and mixed small-file workloads, with throughput and sync
latency recorded. Mount, unmount, and memory are the driver's job; this script
covers the file operations.

    python3 benchmarks/workloads/bench.py MOUNTPOINT LABEL --phase write|read|symlinks

Results go to stdout as TSV: label, phase, metric, value, unit.
"""

import argparse
import hashlib
import json
import os
import shutil
import sys
import time

CHUNK = 1 << 20


def positive(text):
    value = int(text)
    if value <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return value


def sync_path(path):
    handle = os.open(path, os.O_RDONLY)
    try:
        os.fsync(handle)
    finally:
        os.close(handle)


def manifest(root):
    """Hash outside timed regions, before unmount and after the cold read."""
    def digest(path):
        result = hashlib.sha256()
        with open(path, "rb") as handle:
            while block := handle.read(CHUNK):
                result.update(block)
        return result.hexdigest()

    result = {}
    for directory, dirs, files in os.walk(root):
        for name in dirs:
            result[os.path.relpath(os.path.join(directory, name), root)] = "directory"
        for name in files:
            if directory == root and name == "manifest.json":
                continue
            path = os.path.join(directory, name)
            result[os.path.relpath(path, root)] = digest(path)
    return result


def emit(label, phase, metric, value, unit):
    print(f"{label}\t{phase}\t{metric}\t{value:.3f}\t{unit}", flush=True)


def timed(function):
    start = time.perf_counter()
    result = function()
    return time.perf_counter() - start, result


def sequential_write(root, label, mib):
    payload = os.urandom(CHUNK)
    path = os.path.join(root, "seq.bin")
    handle = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        def write():
            for _ in range(mib):
                remaining = memoryview(payload)
                while remaining:
                    written = os.write(handle, remaining)
                    if written == 0:
                        raise OSError("write made no progress")
                    remaining = remaining[written:]

        elapsed, _ = timed(write)
        emit(label, "write", "sequential_write", mib / elapsed, "MiB/s")
        # Sync latency is measured on its own, because a deferred-checkpoint
        # filesystem does most of its publication work here.
        sync, _ = timed(lambda: os.fsync(handle))
        emit(label, "write", "fsync_after_write", sync * 1000, "ms")
        # exFAT and FAT32 buffer the whole write and pay at fsync, so only the
        # sum compares filesystems on real media.
        emit(label, "write", "sequential_write_durable", mib / (elapsed + sync), "MiB/s")
    finally:
        os.close(handle)


def small_files(root, label, count):
    directory = os.path.join(root, "files")
    os.makedirs(directory, exist_ok=True)
    payload = b"x" * 4096

    def create():
        for index in range(count):
            with open(os.path.join(directory, f"f{index:05d}.dat"), "wb") as handle:
                handle.write(payload)

    elapsed, _ = timed(create)
    emit(label, "write", "small_file_create", count / elapsed, "files/s")
    emit(label, "write", "small_file_latency", elapsed / count * 1000, "ms")

    def publish():
        for index in range(count):
            sync_path(os.path.join(directory, f"f{index:05d}.dat"))
        for path in (directory, root, os.path.dirname(root)):
            sync_path(path)

    sync, _ = timed(publish)
    emit(label, "write", "fsync_after_small_files", sync * 1000, "ms")
    emit(label, "write", "small_file_create_durable", count / (elapsed + sync), "files/s")


def tree(root, label, dirs, per_dir):
    base = os.path.join(root, "tree")

    def build():
        for d in range(dirs):
            directory = os.path.join(base, f"d{d:03d}")
            os.makedirs(directory, exist_ok=True)
            for f in range(per_dir):
                with open(os.path.join(directory, f"f{f:03d}"), "wb") as handle:
                    handle.write(b"mixed")

    elapsed, _ = timed(build)
    emit(label, "write", "tree_create", (dirs * per_dir) / elapsed, "files/s")


def listing(root, label, phase):
    directory = os.path.join(root, "files")
    elapsed, names = timed(lambda: sorted(os.listdir(directory)))
    emit(label, phase, "listdir", len(names) / elapsed, "entries/s")
    elapsed, _ = timed(
        lambda: [os.stat(os.path.join(directory, name)) for name in names]
    )
    emit(label, phase, "stat_each", len(names) / elapsed, "entries/s")


def sequential_read(root, label, mib):
    path = os.path.join(root, "seq.bin")
    handle = os.open(path, os.O_RDONLY)
    try:
        def drain():
            total = 0
            while True:
                block = os.read(handle, CHUNK)
                if not block:
                    return total
                total += len(block)

        elapsed, total = timed(drain)
        if total != mib * CHUNK:
            raise ValueError(f"sequential read: expected {mib * CHUNK} bytes, got {total}")
        emit(label, "read", "sequential_read", total / CHUNK / elapsed, "MiB/s")
    finally:
        os.close(handle)


def remove(root, label):
    elapsed, _ = timed(lambda: shutil.rmtree(os.path.join(root, "files")))
    emit(label, "read", "remove_small_files", elapsed * 1000, "ms")
    elapsed, _ = timed(lambda: shutil.rmtree(os.path.join(root, "tree")))
    emit(label, "read", "remove_tree", elapsed * 1000, "ms")


def symlinks(root, label, count):
    """Create, stat, read, and remove relative links; one phase, one mount."""
    directory = os.path.join(root, "links")
    os.makedirs(directory)
    paths = [os.path.join(directory, f"l{index:05d}") for index in range(count)]
    elapsed, _ = timed(lambda: [os.symlink(f"../targets/t{index:05d}", path)
                                for index, path in enumerate(paths)])
    emit(label, "symlinks", "symlink_create", count / elapsed, "links/s")
    elapsed, _ = timed(lambda: [os.lstat(path) for path in paths])
    emit(label, "symlinks", "symlink_lstat", count / elapsed, "links/s")
    elapsed, targets = timed(lambda: [os.readlink(path) for path in paths])
    emit(label, "symlinks", "symlink_readlink", count / elapsed, "links/s")
    if targets != [f"../targets/t{index:05d}" for index in range(count)]:
        raise ValueError("symlink targets changed")
    elapsed, _ = timed(lambda: shutil.rmtree(directory))
    emit(label, "symlinks", "symlink_remove", count / elapsed, "links/s")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mountpoint")
    parser.add_argument("label")
    parser.add_argument("--phase", choices=("write", "read", "symlinks"), required=True)
    parser.add_argument("--seq-mib", type=positive, default=32)
    parser.add_argument("--files", type=positive, default=400)
    parser.add_argument("--dirs", type=positive, default=10)
    parser.add_argument("--per-dir", type=positive, default=20)
    options = parser.parse_args()

    root = os.path.join(options.mountpoint, "bench")
    if options.phase == "symlinks":
        os.makedirs(root, exist_ok=True)
        symlinks(root, options.label, options.files)
    elif options.phase == "write":
        os.mkdir(root)
        sequential_write(root, options.label, options.seq_mib)
        small_files(root, options.label, options.files)
        tree(root, options.label, options.dirs, options.per_dir)
        listing(root, options.label, "write")
        with open(os.path.join(root, "manifest.json"), "w") as handle:
            json.dump(manifest(root), handle)
            handle.flush()
            os.fsync(handle.fileno())
        sync_path(root)
    else:
        sequential_read(root, options.label, options.seq_mib)
        listing(root, options.label, "read")
        with open(os.path.join(root, "manifest.json")) as handle:
            expected = json.load(handle)
        if manifest(root) != expected:
            raise ValueError("file contents or names changed across remount")
        remove(root, options.label)
    return 0


if __name__ == "__main__":
    sys.exit(main())
