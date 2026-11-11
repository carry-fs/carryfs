#!/usr/bin/env python3
"""Compare release adapters on disposable FreeBSD md devices.

Run as root, with one or more directories containing carryfs and carryfs-fuse:
    python3 benchmarks/freebsd.py build/baseline build/candidate --out build/compare

Samples alternate adapter order. Remounts clear FUSE caches, not host disk caches.
Compilation, formatting, verification and process startup are outside workload timers.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import select
import subprocess
import sys
import time

REPO = Path(__file__).resolve().parents[1]
WORKLOAD = REPO / "benchmarks/workloads/bench.py"
BYTES = 512 << 20


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, timeout=300, **kwargs)


def mount(binary, device, point, log):
    daemon = subprocess.Popen(
        [str(binary), "serve", device, str(point), "rw"], stdout=subprocess.PIPE, stderr=log
    )
    try:
        # A stat during nmount can hold the vnode that the FUSE handshake needs.
        with daemon.stdout:
            ready, _, _ = select.select([daemon.stdout], [], [], 60)
            if not ready or daemon.stdout.readline().strip() != b"ready":
                raise RuntimeError(f"mount failed; see {log.name}")
        return daemon
    except BaseException:
        daemon.kill()
        daemon.wait(timeout=30)
        raise


def unmount(daemon, point):
    run("umount", point)
    if daemon.wait(timeout=30) != 0:
        raise RuntimeError("daemon failed during unmount")


def raw_reads(device, label, metrics):
    """An optimistic I/O floor, without FUSE, checksum or namespace work."""
    descriptor = os.open(device, os.O_RDONLY)
    try:
        for size in (4096, 65536, 1 << 20):
            started = time.perf_counter()
            total = 0
            for offset in range(0, 32 << 20, size):
                block = os.pread(descriptor, size, offset)
                if len(block) != size:
                    raise RuntimeError("short raw device read")
                total += len(block)
            elapsed = time.perf_counter() - started
            metrics.write(f"{label}\traw\tpread_{size}\t{total / (1 << 20) / elapsed:.3f}\tMiB/s\n")
        metrics.flush()
    finally:
        os.close(descriptor)


def overwrites(point, label, metrics):
    """Include scattered updates and repeated read/modify/write of one unit."""
    path = point / "bench/seq.bin"
    expected = bytearray(path.read_bytes())
    descriptor = os.open(path, os.O_RDWR)
    try:
        for size in (4096, 1):
            updates = [
                ((index * 65537) % (len(expected) - size) if size == 4096 else index * 503,
                 bytes([index % 251]) * size)
                for index in range(128)
            ]
            for offset, data in updates:
                expected[offset:offset + len(data)] = data
            started = time.perf_counter()
            for offset, data in updates:
                if os.pwrite(descriptor, data, offset) != len(data):
                    raise RuntimeError("short overwrite")
            os.fsync(descriptor)
            elapsed = time.perf_counter() - started
            metrics.write(f"{label}\tupdate\toverwrite_{size}_durable\t{128 / elapsed:.3f}\tops/s\n")
    finally:
        os.close(descriptor)
    if path.read_bytes() != expected:
        raise RuntimeError("overwrites changed unexpected bytes")
    return hashlib.sha256(expected).hexdigest()


def sample(binaries, work, metrics):
    work.mkdir()
    point = work / "mnt"
    point.mkdir()
    image = work / "disk.img"
    daemon = device = None
    with (work / "daemon.log").open("w") as log:
        try:
            run(binaries / "carryfs", "mkfs", image, BYTES, stdout=log)
            device = "/dev/" + subprocess.check_output(
                ["mdconfig", "-a", "-t", "vnode", "-f", str(image)], text=True
            ).strip()
            raw_reads(device, work.name, metrics)
            for phase in ("write", "read"):
                daemon = mount(binaries / "carryfs-fuse", device, point, log)
                run(sys.executable, WORKLOAD, point, work.name, "--phase", phase,
                    "--seq-mib", "32", "--files", "400", stdout=metrics)
                if phase == "read":
                    expected = overwrites(point, work.name, metrics)
                rss = int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(daemon.pid)]))
                metrics.write(f"{work.name}\t{phase}\tdaemon_rss\t{rss / 1024:.3f}\tMiB\n")
                metrics.flush()
                unmount(daemon, point)
                daemon = None
            run("mdconfig", "-d", "-u", Path(device).name)
            device = None
            run(binaries / "carryfs", "check", image, stdout=log)
            run(binaries / "carryfs", "scrub", image, stdout=log)
            recovered = work / "recovered.bin"
            run(binaries / "carryfs", "get", image, "/bench/seq.bin", recovered, stdout=log)
            if hashlib.sha256(recovered.read_bytes()).hexdigest() != expected:
                raise RuntimeError("overwrites changed after unmount")
        finally:
            if daemon is not None and daemon.poll() is None:
                daemon.kill()
                daemon.wait(timeout=30)
            if os.path.ismount(point):
                run("umount", "-f", point)
            if device is not None:
                run("mdconfig", "-d", "-u", Path(device).name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, nargs="+")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=5)
    options = parser.parse_args()
    if platform.system() != "FreeBSD" or os.geteuid() != 0:
        parser.error("run as root on FreeBSD with fusefs loaded")
    if options.rounds < 1:
        parser.error("--rounds must be positive")
    binaries = [directory.resolve(strict=True) for directory in options.binaries]
    if len({directory.name for directory in binaries}) != len(binaries):
        parser.error("binary directories must have distinct names")
    hashes = {
        str(directory): {
            name: hashlib.sha256((directory / name).read_bytes()).hexdigest()
            for name in ("carryfs", "carryfs-fuse")
        }
        for directory in binaries
    }
    options.out.mkdir(parents=True, exist_ok=False)
    metadata = {
        "command": sys.argv,
        "uname": list(platform.uname()),
        "cpu": subprocess.check_output(["sysctl", "hw.model", "hw.ncpu"], text=True),
        "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
        "binaries_sha256": hashes,
        "rounds": options.rounds,
        "image_bytes": BYTES,
    }
    (options.out / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    with (options.out / "metrics.tsv").open("w") as metrics:
        for index in range(options.rounds):
            for directory in (binaries if index % 2 == 0 else binaries[::-1]):
                sample(directory, options.out / f"{directory.name}-{index}", metrics)
    return 0


if __name__ == "__main__":
    sys.exit(main())
