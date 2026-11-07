#!/usr/bin/env python3
"""Check native read-only mounts in a disposable VM with the module loaded."""

import errno
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def run(*args):
    return subprocess.run([str(arg) for arg in args], check=True)


def main():
    cli = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[2] / "target/release/carryfs"
    run("modprobe", "loop")
    run("modprobe", "fuse")
    with tempfile.TemporaryDirectory(prefix="carryfs-ro-") as directory:
        work = Path(directory)
        image, point = work / "volume.img", work / "mnt"
        point.mkdir()
        run(cli, "mkfs", image, 256 << 20)
        loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
        daemon = None
        try:
            daemon = subprocess.Popen([cli.with_name("carryfs-fuse"), "serve", loop, point, "rw"])
            deadline = time.monotonic() + 30
            while not os.path.ismount(point):
                assert daemon.poll() is None, "FUSE population mount exited"
                assert time.monotonic() < deadline, "FUSE population mount timed out"
                time.sleep(0.02)
            payload = bytes(range(256)) * 1025
            (point / "folder").mkdir()
            with (point / "folder/MixedCase").open("wb") as output:
                output.write(payload)
                output.seek(5 * 65536 + 13)
                output.write(b"tail")
                output.flush()
                os.fsync(output.fileno())
            expected = (point / "folder/MixedCase").read_bytes()
            run("umount", point)
            assert daemon.wait(timeout=30) == 0
            daemon = None
            before = hashlib.sha256(image.read_bytes()).digest()
            refused = subprocess.run(["mount", "-w", "-i", "-t", "carryfs", loop, str(point)], capture_output=True)
            assert refused.returncode != 0, "native writable mount was accepted"
            run("mount", "-i", "-t", "carryfs", "-o", "ro", loop, point)
            assert sorted(p.name for p in point.iterdir()) == ["folder"]
            assert (point / "folder/mixedcase").read_bytes() == expected
            try:
                (point / "forbidden").write_bytes(b"no")
            except OSError as error:
                assert error.errno == errno.EROFS, error
            else:
                raise AssertionError("native read-only mount accepted a write")
            run("umount", point)
            assert hashlib.sha256(image.read_bytes()).digest() == before, "read-only mount changed the image"
        finally:
            if os.path.ismount(point):
                run("umount", point)
            if daemon is not None:
                daemon.terminate()
                daemon.wait(timeout=30)
            run("losetup", "-d", loop)
            backing = Path("/sys/block") / Path(loop).name / "loop/backing_file"
            deadline = time.monotonic() + 30
            while backing.exists():
                assert time.monotonic() < deadline, "loop device did not detach"
                time.sleep(0.02)
        run(cli, "check", image)
        run(cli, "scrub", image)
    print("native read-only mounts preserved bytes, names, holes, and the original image")


if __name__ == "__main__":
    main()
