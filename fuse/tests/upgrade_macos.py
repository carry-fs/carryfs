#!/usr/bin/env python3
"""Check upgrade refuses a mounted macOS volume, using a disposable image.

Run after building release binaries: python3 fuse/tests/upgrade_macos.py
"""
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[2]


def run(*args):
    return subprocess.run(list(map(str, args)), check=True, capture_output=True, timeout=60)


def main():
    if sys.platform != "darwin":
        raise SystemExit("This test requires macOS and macFUSE.")
    binaries = Path(sys.argv[1]) if len(sys.argv) > 1 else REPO / "target/release"
    cli, fuse = binaries / "carryfs", binaries / "carryfs-fuse"
    work = Path(tempfile.mkdtemp(prefix="upgrade-macos-", dir=REPO / "build"))
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
                time.sleep(.01)
            refused = subprocess.run([str(fuse), "upgrade", device], capture_output=True, timeout=30)
            assert refused.returncode != 0, "upgrade must refuse a mounted volume"
            (point / "kept").write_bytes(b"still writable")
            run("umount", point)
            assert daemon.wait(timeout=60) == 0
            run(fuse, "upgrade", device)
            assert b"already stores" in run(fuse, "upgrade", device).stdout
            run(cli, "check", "--bytes", 256 << 20, device)
    finally:
        if os.path.ismount(point):
            run("umount", point)
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            daemon.wait(timeout=60)
        run("hdiutil", "detach", device)
    print(f"PASS: mounted upgrade refused, unmounted upgrade succeeds; logs in {work}")


if __name__ == "__main__":
    main()
