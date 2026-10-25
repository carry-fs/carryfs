#!/usr/bin/env python3
"""Kill a disposable FUSE daemon immediately after a namespace durability barrier.

Run after a release build: sudo python3 fuse/tests/fsyncdir.py
Evidence remains under build/fsyncdir-*; no existing device or mount is used.
"""
import errno
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[2]


def run(*args, **options):
    return subprocess.run(args, check=True, timeout=60, **options)


def probe(work, operation, strict, datasync):
    folder = work / f'{operation}-{"strict" if strict else "default"}-{"datasync" if datasync else "sync"}'
    folder.mkdir()
    point = folder / "mnt"
    point.mkdir()
    image = folder / "disk.img"
    cli = REPO / "target/release/carryfs"
    loop = None
    daemon = None
    with (folder / "probe.log").open("w") as log:
        try:
            run(cli, "mkfs", image, str(256 << 20), stdout=log)
            loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
            daemon = subprocess.Popen(
                [REPO / "target/release/carryfs-fuse", "serve", loop, point,
                 "rw,uid=0,gid=0" + (",strict" if strict else "")], stdout=log, stderr=log)
            deadline = time.monotonic() + 30
            while not os.path.ismount(point):
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("FUSE mount failed or timed out")
                time.sleep(.005)
            # Publish the initial state through file fsync, independently of fsyncdir.
            with (point / "before").open("wb") as f:
                f.write(b"preserved")
                f.flush()
                os.fsync(f.fileno())
            directory = os.open(point, os.O_RDONLY | os.O_DIRECTORY)
            try:
                if operation == "create":
                    (point / "after").touch()
                elif operation == "rename":
                    (point / "before").rename(point / "after")
                else:
                    (point / "before").unlink()
                # Stop the process as soon as the barrier returns. An idle timer
                # must not turn a missing handler into a passing test.
                (os.fdatasync if datasync else os.fsync)(directory)
                daemon.kill()
            finally:
                try:
                    os.close(directory)
                except OSError as error:
                    if error.errno not in (errno.ECONNABORTED, errno.ENOTCONN):
                        raise
            daemon.wait(timeout=10)
            run("umount", point)
            run("losetup", "-d", loop)
            # LOOP_CLR_FD is lazy while udev still has the device open. The
            # offline CLI must wait until no loop uses the backing image.
            backing = Path("/sys/class/block") / Path(loop).name / "loop/backing_file"
            deadline = time.monotonic() + 30
            while backing.exists():
                if time.monotonic() > deadline:
                    raise RuntimeError("loop teardown timed out before offline check")
                time.sleep(.01)
            run("udevadm", "settle")
            loop = None
            run(cli, "check", image, stdout=log)
            run(cli, "get", image, "/", folder / "recovered", stdout=log)
            expected = {"create": {"before", "after"}, "rename": {"after"}, "unlink": set()}[operation]
            actual = {p.name for p in (folder / "recovered").iterdir()}
            assert actual == expected, (operation, strict, actual, expected)
            if operation != "unlink":
                kept = "before" if operation == "create" else "after"
                assert (folder / "recovered" / kept).read_bytes() == b"preserved"
            print(f"PASS {folder.name}", flush=True)
        finally:
            if daemon is not None and daemon.poll() is None:
                daemon.kill()
                daemon.wait(timeout=10)
            if os.path.ismount(point):
                run("umount", point)
            if loop is not None:
                run("losetup", "-d", loop)


if __name__ == "__main__":
    if sys.platform != "linux" or os.geteuid() != 0:
        raise SystemExit("Run as root on a disposable Linux host with FUSE and loop devices.")
    (REPO / "build").mkdir(exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="fsyncdir-", dir=REPO / "build"))
    # The unprivileged CI user uploads these generated logs after the root test.
    work.chmod(0o755)
    print(work, flush=True)
    for strict in (False, True):
        for datasync in (False, True):
            for operation in ("create", "rename", "unlink"):
                probe(work, operation, strict, datasync)
