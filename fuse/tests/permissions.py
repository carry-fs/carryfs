#!/usr/bin/env python3
"""Check stored modes through a real FUSE mount on Linux.

Run after a release build: sudo python3 fuse/tests/permissions.py
The mount belongs to uid 1000; uid 65534 stands in for a second account.
Evidence remains under /var/tmp/carryfs-permissions-*, a place the second
account can reach; no existing device or mount is used.
"""
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parents[2]
CLI = REPO / "target/release/carryfs"
FUSE = REPO / "target/release/carryfs-fuse"
OWNER, OTHER = 1000, 65534


def run(*args, **options):
    return subprocess.run([str(arg) for arg in args], check=True, timeout=60, **options)


def as_user(uid, script, umask="022"):
    """Run a shell snippet as `uid`; return (exit status, output)."""
    done = subprocess.run(
        ["setpriv", f"--reuid={uid}", f"--regid={uid}", "--clear-groups",
         "sh", "-c", f"umask {umask}; {script}"],
        capture_output=True, text=True, timeout=60)
    return done.returncode, (done.stdout + done.stderr).strip()


def mode(path):
    return stat.S_IMODE(os.lstat(path).st_mode)


def check(condition, message):
    if not condition:
        raise AssertionError(message)
    print("ok:", message)


class Mount:
    def __init__(self, image, point, log):
        self.image = image
        self.loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
        self.point = point
        self.daemon = subprocess.Popen(
            [FUSE, "serve", self.loop, point, f"rw,uid={OWNER},gid={OWNER}"],
            stdout=log, stderr=log)
        deadline = time.monotonic() + 30
        while not os.path.ismount(point):
            if self.daemon.poll() is not None or time.monotonic() > deadline:
                self.close()
                raise RuntimeError("FUSE mount failed or timed out")
            time.sleep(.01)

    def close(self):
        if os.path.ismount(self.point):
            run("umount", self.point)
        if self.daemon.poll() is None:
            self.daemon.wait(timeout=60)
        run("losetup", "-d", self.loop)
        # The kernel detaches a loop device after the call returns.
        deadline = time.monotonic() + 30
        while subprocess.check_output(["losetup", "-j", self.image], text=True).strip():
            if time.monotonic() > deadline:
                raise RuntimeError("loop device did not detach")
            time.sleep(.05)


def upgraded(folder, log):
    point = folder / "mnt"
    point.mkdir()
    os.chmod(folder, 0o755)
    m = Mount(folder / "disk.img", point, log)
    try:
        check(mode(point) == 0o755, "the root reads 0755")
        status, _ = as_user(OWNER, f"touch {point}/a && mkdir {point}/d", umask="077")
        check(status == 0 and mode(point / "a") == 0o600 and mode(point / "d") == 0o700,
              "create and mkdir store the mode after the umask")

        script = point / "script"
        status, out = as_user(OWNER, f"printf '#!/bin/sh\\necho ran\\n' > {script}"
                                     f" && chmod +x {script} && {script}")
        check(status == 0 and out == "ran", "chmod +x then execve runs the script")
        status, _ = as_user(OWNER, f"chmod -x {script} && {script}")
        check(status != 0, "chmod -x stops it")

        status, _ = as_user(OWNER, f"chmod 4755 {script}")
        check(status == 0 and mode(script) == 0o755, "setuid is dropped, the rest stored")

        secret = point / "secret"
        status, _ = as_user(OWNER, f"echo hidden > {secret}")
        status, out = as_user(OTHER, f"cat {secret}")
        check(status == 0 and out == "hidden", "another account reads a 0644 file")
        status, _ = as_user(OWNER, f"chmod 600 {secret}")
        check(status == 0, "the owner makes it 0600")
        status, _ = as_user(OTHER, f"cat {secret}")
        check(status != 0, "a 0600 file is unreadable to another account")
        status, out = as_user(OWNER, f"cat {secret}")
        check(status == 0 and out == "hidden", "the owner still reads it")
        status, _ = as_user(OWNER, f"chmod 000 {secret} && cat {secret}")
        check(status != 0, "mode 000 stops the owner too")

        status, _ = as_user(OWNER, f"chown {OTHER} {point}/a")
        check(status != 0, "chown to another ID is refused")

        os.symlink("a", point / "link")
        check(mode(point / "link") == 0o777, "a symlink reads 0777")
    finally:
        m.close()

    run(CLI, "check", folder / "disk.img", stdout=log)
    m = Mount(folder / "disk.img", point, log)
    try:
        check((mode(point / "a"), mode(point / "d"), mode(script), mode(secret))
              == (0o600, 0o700, 0o755, 0o000), "every mode survives a remount")
    finally:
        m.close()


def not_upgraded(folder, log):
    point = folder / "mnt"
    point.mkdir()
    os.chmod(folder, 0o755)
    m = Mount(folder / "disk.img", point, log)
    try:
        status, _ = as_user(OWNER, f"umask 077; touch {point}/a")
        check(status == 0 and mode(point / "a") == 0o644,
              "without the feature a new file reads 0644")
        status, _ = as_user(OWNER, f"chmod 644 {point}/a")
        check(status == 0, "a chmod that changes nothing succeeds")
        status, _ = as_user(OWNER, f"chmod 600 {point}/a")
        check(status != 0 and mode(point / "a") == 0o644, "a chmod that changes the mode fails")
    finally:
        m.close()
    run(CLI, "check", folder / "disk.img", stdout=log)


def main():
    work = Path(tempfile.mkdtemp(prefix="carryfs-permissions-", dir="/var/tmp"))
    os.chmod(work, 0o755)
    for name, test, upgrade in [("upgraded", upgraded, True), ("plain", not_upgraded, False)]:
        folder = work / name
        folder.mkdir()
        with (folder / "log").open("w") as log:
            run(CLI, "mkfs", folder / "disk.img", str(256 << 20), stdout=log)
            if upgrade:
                run(CLI, "upgrade", folder / "disk.img", stdout=log)
            test(folder, log)
    print("PASS:", work)


if __name__ == "__main__":
    main()
