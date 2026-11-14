#!/usr/bin/env python3
"""Check macFUSE F_PREALLOCATE on a disposable image.

Run after cargo build: python3 fuse/tests/preallocate_macos.py [binaries]
"""

import fcntl
import os
from pathlib import Path
import plistlib
import struct
import subprocess
import sys
import tempfile
import time


def run(*args):
    return subprocess.run(list(map(str, args)), check=True, capture_output=True, timeout=60)


def main():
    if sys.platform != "darwin":
        raise SystemExit("This test requires macOS and macFUSE.")
    repo = Path(__file__).resolve().parents[2]
    binaries = Path(sys.argv[1]) if len(sys.argv) > 1 else repo / "target/debug"
    cli, fuse = binaries / "carryfs", binaries / "carryfs-fuse"
    work = Path(tempfile.mkdtemp(prefix="carryfs-preallocation-"))
    print(f"Test image and logs: {work}", flush=True)
    image, point = work / "disk.img", work / "mnt"
    point.mkdir()
    run(cli, "mkfs", image, 256 << 20)
    attached = plistlib.loads(run("hdiutil", "attach", "-plist", "-imagekey",
                                 "diskimage-class=CRawDiskImage", "-nomount", image).stdout)
    device = attached["system-entities"][0]["dev-entry"]
    daemon = None
    try:
        with (work / "daemon.log").open("w") as log:
            daemon = subprocess.Popen([str(fuse), "serve", device, str(point),
                                       f"rw,uid={os.getuid()},gid={os.getgid()}"],
                                      stdout=log, stderr=log)
            deadline = time.monotonic() + 30
            while not os.path.ismount(point):
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(f"mount failed; see {work / 'daemon.log'}")
                time.sleep(.02)

            def reserve(fd, length, persist):
                # F_ALLOCATEALL, optional F_ALLOCATEPERSIST, F_PEOFPOSMODE.
                request = struct.pack("=Iiqqq", 4 | (8 if persist else 0), 3, 0, length, 0)
                result = fcntl.fcntl(fd, 42, request)
                assert struct.unpack("=Iiqqq", result)[4] == length

            unit = 65536
            fd = os.open(point / "persistent", os.O_CREAT | os.O_RDWR, 0o600)
            try:
                reserve(fd, 3 * unit, True)
                assert os.fstat(fd).st_size == 0
                reserve(fd, unit, True)
                assert os.write(fd, b"keep") == 4
                os.fsync(fd)
            finally:
                os.close(fd)
            assert (point / "persistent").read_bytes() == b"keep"

            fd = os.open(point / "temporary", os.O_CREAT | os.O_RDWR, 0o600)
            try:
                reserve(fd, 2 * unit, False)
                assert os.fstat(fd).st_size == 0
            finally:
                os.close(fd)
            deadline = time.monotonic() + 5
            while (point / "temporary").stat().st_blocks != 0 and time.monotonic() < deadline:
                time.sleep(.05)
            assert (point / "temporary").stat().st_blocks == 0
            run("umount", point)
            assert daemon.wait(timeout=60) == 0
            # macFUSE 5.5 reports allocation only up to EOF through stat.
            # The checker counts the persistent tail and proves close reclaimed
            # the temporary reservation without discarding the persistent one.
            checked = run(cli, "check", "--bytes", 256 << 20, device)
            assert b"64 data blocks" in checked.stdout, checked.stdout
            run(cli, "scrub", "--bytes", 256 << 20, device)
    finally:
        if os.path.ismount(point):
            run("umount", point)
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=60)
        run("hdiutil", "detach", device)
    print(f"PASS: macFUSE preallocation; logs in {work}")


if __name__ == "__main__":
    main()
