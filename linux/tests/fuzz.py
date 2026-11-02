#!/usr/bin/env python3
"""Mount corrupted images through the kernel driver and fail on any oops.

Each case copies a populated image, changes random bytes of random metadata
pages, and recomputes their CRC32C so the corruption reaches the record
checks instead of stopping at the checksum. It then mounts the copy, walks
it, reads every file, and on writable mounts writes and removes a file.
Errors from the driver are expected; a kernel BUG, oops, or warning is not.

Needs root, the module loaded, and the `carryfs` CLI.
"""

import argparse
import os
import random
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

BLOCK = 4096
TABLE = []
for byte in range(256):
    crc = byte
    for _ in range(8):
        crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    TABLE.append(crc)


def crc32c(data):
    crc = 0xFFFFFFFF
    for byte in data:
        crc = TABLE[(crc ^ byte) & 0xFF] ^ (crc >> 8)
    return crc ^ 0xFFFFFFFF


def reseal(page):
    """Recompute the CRC of a root or tree page in place."""
    if page[:8] == b"CFSXROOT":
        page[4092:4096] = b"\0\0\0\0"
        page[4092:4096] = struct.pack("<I", crc32c(page))
    elif page[:8] == b"CFSXPAGE":
        page[64:68] = b"\0\0\0\0"
        page[64:68] = struct.pack("<I", crc32c(page))


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], **kwargs)


def kernel_log_mark():
    return len(subprocess.check_output(["dmesg"], text=True).splitlines())


def kernel_problems(mark):
    lines = subprocess.check_output(["dmesg"], text=True).splitlines()[mark:]
    return [line for line in lines if any(word in line for word in ("BUG", "Oops", "WARNING", "panicked", "Call Trace"))]


def populate(cli, image, point):
    run(cli, "mkfs", image, 256 << 20, check=True, stdout=subprocess.DEVNULL)
    loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
    run("mount", "-i", "-t", "carryfs", loop, point, check=True)
    for directory in range(6):
        base = point / f"dir{directory}"
        base.mkdir()
        for index in range(60):
            (base / f"File-{index}-{'é' * (index % 3)}").write_bytes(os.urandom(random.choice((0, 100, 5000, 70000))))
        os.symlink("File-1-é", base / "link")
    run("umount", point, check=True)
    run("losetup", "-d", loop, check=True)


def metadata_blocks(image):
    """Blocks whose content starts with a root or page magic."""
    found = []
    with open(image, "rb") as source:
        block = 0
        while chunk := source.read(BLOCK):
            if chunk[:8] in (b"CFSXROOT", b"CFSXPAGE"):
                found.append(block)
            block += 1
    return found


def exercise(point, writable):
    for directory, _, files in os.walk(point):
        for name in files:
            path = os.path.join(directory, name)
            try:
                os.lstat(path)
                if os.path.islink(path):
                    os.readlink(path)
                else:
                    with open(path, "rb") as handle:
                        while handle.read(1 << 20):
                            pass
            except OSError:
                pass
    if writable:
        try:
            (point / "fuzz-new").write_bytes(os.urandom(100_000))
            os.sync()
            os.unlink(point / "fuzz-new")
            os.sync()
        except OSError:
            pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=Path(__file__).resolve().parents[2] / "target/release/carryfs")
    parser.add_argument("--cases", type=int, default=200)
    parser.add_argument("--seed", type=int, default=1)
    args = parser.parse_args()
    random.seed(args.seed)
    with tempfile.TemporaryDirectory(prefix="carryfs-fuzz-") as directory:
        work = Path(directory)
        point = work / "mnt"
        point.mkdir()
        base = work / "base.img"
        populate(args.cli, base, point)
        blocks = metadata_blocks(base)
        mounted = rejected = 0
        for case in range(args.cases):
            image = work / "case.img"
            run("cp", "--sparse=always", base, image, check=True)
            with open(image, "r+b") as target:
                for _ in range(random.randint(1, 4)):
                    block = random.choice(blocks)
                    target.seek(block * BLOCK)
                    page = bytearray(target.read(BLOCK))
                    for _ in range(random.randint(1, 3)):
                        # Favor the header and the first records.
                        offset = random.choice((random.randrange(16, 80), random.randrange(80, 400), random.randrange(BLOCK)))
                        page[offset] = random.choice((0, 0xFF, page[offset] ^ (1 << random.randrange(8)), random.randrange(256)))
                    if random.random() < 0.9:
                        reseal(page)
                    target.seek(block * BLOCK)
                    target.write(page)
            writable = random.random() < 0.5
            mark = kernel_log_mark()
            loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
            mode = "rw" if writable else "ro"
            status = run("mount", "-i", "-t", "carryfs", "-o", mode, loop, point,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode
            if status == 0:
                mounted += 1
                exercise(point, writable)
                run("umount", point, check=True)
            else:
                rejected += 1
            run("losetup", "-d", loop, check=True)
            backing = Path("/sys/block") / Path(loop).name / "loop/backing_file"
            while backing.exists():
                time.sleep(0.02)
            problems = kernel_problems(mark)
            if problems:
                kept = Path.cwd() / f"fuzz-failure-{args.seed}-{case}.img"
                run("cp", "--sparse=always", image, kept)
                sys.exit(f"case {case}: kernel problem; image kept at {kept}\n" + "\n".join(problems[:40]))
        print(f"{args.cases} cases: {mounted} mounted, {rejected} refused, no kernel problems")


if __name__ == "__main__":
    main()
