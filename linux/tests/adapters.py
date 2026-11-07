#!/usr/bin/env python3
"""Verify default FUSE selection and deliberate native mounts on one image."""

import os
from pathlib import Path
import subprocess
import tempfile
import time

from check import Volume, run


def main():
    repo = Path(__file__).resolve().parents[2]
    run("modprobe", "fuse")
    with tempfile.TemporaryDirectory(prefix="carryfs-adapters-") as directory:
        work = Path(directory)
        volume = Volume(repo / "target/release/carryfs", work, "adapters", 256)
        helper = work / "mount.carryfs"
        helper.symlink_to(repo / "target/release/carryfs-fuse")
        # The VM has no systemd manager. Run the requested daemon in the
        # foreground so its status and diagnostics stay with this test.
        launcher = work / "systemd-run"
        launcher.write_text('#!/bin/sh\nwhile [ "${1#--}" != "$1" ]; do shift; done\nexec "$@"\n')
        launcher.chmod(0o755)
        loop = subprocess.check_output(["losetup", "-f", "--show", volume.image], text=True).strip()
        daemon = None
        try:
            daemon = subprocess.Popen([helper, loop, volume.point], env={**os.environ, "PATH": f"{work}:{os.environ['PATH']}"})
            deadline = time.monotonic() + 30
            while not os.path.ismount(volume.point):
                assert daemon.poll() is None, "default mount helper exited before mounting"
                assert time.monotonic() < deadline, "default mount helper timed out"
                time.sleep(0.02)
            kind = subprocess.check_output(["findmnt", "-n", "-o", "FSTYPE", "--target", volume.point], text=True).strip()
            assert kind == "fuseblk.carryfs", kind
            (volume.point / "from-fuse").write_bytes(b"fuse")
            run("umount", volume.point)
            assert daemon.wait(timeout=30) == 0
            run("mount", "-i", "-t", "carryfs", loop, volume.point)
            assert (volume.point / "from-fuse").read_bytes() == b"fuse"
            (volume.point / "from-native").write_bytes(b"native")
            run("umount", volume.point)
        finally:
            if os.path.ismount(volume.point):
                run("umount", volume.point)
            if daemon is not None and daemon.poll() is None:
                daemon.terminate()
                daemon.wait(timeout=30)
            run("losetup", "-d", loop)
        print(f"default helper used FUSE with native loaded; explicit native mount preserved data; {volume.check(scrub=True)}", flush=True)


if __name__ == "__main__":
    main()
