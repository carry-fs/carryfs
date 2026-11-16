#!/usr/bin/env python3
"""Stored modes on Linux, through FUSE and the kernel module alike.

    sudo python3 linux/tests/permissions.py [fuse] [kernel]

Both adapters by default; the kernel module must be loaded, preferably in a
VM (linux/tests/vm.sh). Needs a release build of carryfs and carryfs-fuse.

Every step runs through each adapter, and each command's exit status and
message are recorded: the transcripts must match, so the adapters agree on
errors too, not only on what the checks assert. Then each volume is listed
through every adapter: a tree one adapter wrote must read the same through
the other.

The mount belongs to uid 1000; uid 65534 stands in for a second account.
Work lives under /var/tmp, which the second account can reach.
"""
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[2]
CLI = REPO / "target/release/carryfs"
FUSE = REPO / "target/release/carryfs-fuse"
OWNER, OTHER = 1000, 65534


def run(*args, **options):
    return subprocess.run([str(arg) for arg in args], check=True, timeout=60, **options)


class Mount:
    def __init__(self, adapter, image, point):
        self.adapter, self.image, self.point = adapter, image, point
        self.loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
        options = f"rw,uid={OWNER},gid={OWNER}"
        self.daemon = None
        if adapter == "kernel":
            run("mount", "-i", "-t", "carryfs", "-o", options, self.loop, point)
        else:
            self.daemon = subprocess.Popen([FUSE, "serve", self.loop, point, options],
                                           stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 30
        while not os.path.ismount(point):
            if self.daemon.poll() is not None or time.monotonic() > deadline:
                self.close()
                raise RuntimeError("FUSE mount failed or timed out")
            time.sleep(.01)
        self.transcript = []

    def user(self, uid, script, umask="022"):
        """Run a shell snippet as `uid` with MNT set to the mount point."""
        done = subprocess.run(
            ["setpriv", f"--reuid={uid}", f"--regid={uid}", "--clear-groups",
             "sh", "-c", f"umask {umask}; {script}"],
            env={"PATH": os.environ["PATH"], "MNT": str(self.point), "LC_ALL": "C"},
            capture_output=True, text=True, timeout=60)
        out = (done.stdout + done.stderr).strip().replace(str(self.point), "MNT")
        self.transcript.append((uid, umask, script, done.returncode, out))
        return done.returncode, out

    def close(self):
        if os.path.ismount(self.point):
            run("umount", self.point)
        if self.daemon is not None and self.daemon.poll() is None:
            self.daemon.wait(timeout=60)
        run("losetup", "-d", self.loop)
        # The kernel detaches a loop device after the call returns.
        deadline = time.monotonic() + 30
        while subprocess.check_output(["losetup", "-j", self.image], text=True).strip():
            if time.monotonic() > deadline:
                raise RuntimeError("loop device did not detach")
            time.sleep(.05)


def check(adapter, condition, message):
    if not condition:
        raise AssertionError(f"{adapter}: {message}")
    print(f"ok [{adapter}]: {message}")


def mode(path):
    return stat.S_IMODE(os.lstat(path).st_mode)


def listing(point):
    """Every object's path, type, and permission bits."""
    found = []
    for root, directories, files in os.walk(point):
        for name in directories + files:
            path = Path(root) / name
            info = os.lstat(path)
            found.append((str(path.relative_to(point)), stat.S_IFMT(info.st_mode),
                          oct(stat.S_IMODE(info.st_mode))))
    return sorted(found)


def exercise(m):
    a, p = m.adapter, m.point
    check(a, mode(p) == 0o755, "the root reads 0755")

    status, _ = m.user(OWNER, "touch $MNT/a && mkdir $MNT/d", umask="077")
    check(a, status == 0 and (mode(p / "a"), mode(p / "d")) == (0o600, 0o700),
          "create and mkdir store the mode after the umask")
    status, _ = m.user(OWNER, "mkdir -m 2775 $MNT/shared && mkdir -m 1777 $MNT/sticky")
    check(a, status == 0 and (mode(p / "shared"), mode(p / "sticky")) == (0o775, 0o777),
          "setgid and sticky are dropped from mkdir modes")

    status, out = m.user(OWNER, "printf '#!/bin/sh\\necho ran\\n' > $MNT/script"
                                " && chmod +x $MNT/script && $MNT/script")
    check(a, status == 0 and out == "ran", "chmod +x then execve runs the script")
    status, _ = m.user(OWNER, "chmod -x $MNT/script && $MNT/script")
    check(a, status != 0, "chmod -x stops it")
    status, _ = m.user(OWNER, "chmod 4755 $MNT/script")
    check(a, status == 0 and mode(p / "script") == 0o755, "setuid is dropped, the rest stored")

    before = os.stat(p / "script")
    time.sleep(.02)
    m.user(OWNER, "chmod 750 $MNT/script")
    after = os.stat(p / "script")
    check(a, after.st_ctime_ns > before.st_ctime_ns and after.st_mtime_ns == before.st_mtime_ns,
          "chmod changes ctime and not mtime")

    m.user(OWNER, "echo hidden > $MNT/secret")
    status, out = m.user(OTHER, "cat $MNT/secret")
    check(a, status == 0 and out == "hidden", "another account reads a 0644 file")
    status, _ = m.user(OWNER, "chmod 600 $MNT/secret")
    status, _ = m.user(OTHER, "cat $MNT/secret")
    check(a, status != 0, "a 0600 file is unreadable to another account")
    status, out = m.user(OWNER, "cat $MNT/secret")
    check(a, status == 0 and out == "hidden", "the owner still reads it")
    status, _ = m.user(OTHER, "chmod 644 $MNT/secret")
    check(a, status != 0 and mode(p / "secret") == 0o600, "another account cannot chmod it")
    status, _ = m.user(OWNER, "chmod 000 $MNT/secret && cat $MNT/secret")
    check(a, status != 0, "mode 000 stops the owner too")

    status, _ = m.user(OWNER, "chmod 500 $MNT/d && touch $MNT/d/x")
    check(a, status != 0, "a directory without write refuses a create")
    status, _ = m.user(OWNER, "chmod 700 $MNT/d && touch $MNT/d/x && chmod 600 $MNT/d && stat $MNT/d/x")
    check(a, status != 0, "a directory without search refuses a lookup")
    m.user(OWNER, "chmod 755 $MNT/d")

    status, _ = m.user(OWNER, f"chown {OTHER} $MNT/a")
    check(a, status != 0, "chown to another ID is refused")
    status, _ = m.user(OWNER, f"chown {OWNER}:{OWNER} $MNT/a")
    check(a, status == 0, "chown to the mount owner succeeds")

    m.user(OWNER, "ln -s a $MNT/link")
    check(a, mode(p / "link") == 0o777, "a symlink reads 0777")
    status, _ = m.user(OWNER, "mv $MNT/script $MNT/d/moved && truncate -s 1 $MNT/d/moved")
    check(a, status == 0 and mode(p / "d" / "moved") == 0o750,
          "rename and truncate keep the mode")


def exercise_plain(m):
    a, p = m.adapter, m.point
    status, _ = m.user(OWNER, "touch $MNT/a", umask="077")
    check(a, status == 0 and mode(p / "a") == 0o644, "without the feature a new file reads 0644")
    status, _ = m.user(OWNER, "chmod 644 $MNT/a")
    check(a, status == 0, "a chmod that changes nothing succeeds")
    status, _ = m.user(OWNER, "chmod 600 $MNT/a")
    check(a, status != 0 and mode(p / "a") == 0o644, "a chmod that changes the mode fails")


def main():
    adapters = sys.argv[1:] or ["fuse", "kernel"]
    work = Path(tempfile.mkdtemp(prefix="carryfs-permissions-", dir="/var/tmp"))
    os.chmod(work, 0o755)
    transcripts = {}
    for upgrade, exercised in [(True, exercise), (False, exercise_plain)]:
        listings = {}
        for writer in adapters:
            folder = work / f"{writer}-{'upgraded' if upgrade else 'plain'}"
            point = folder / "mnt"
            point.mkdir(parents=True)
            os.chmod(folder, 0o755)
            image = folder / "disk.img"
            run(CLI, "mkfs", image, str(256 << 20), stdout=subprocess.DEVNULL)
            if upgrade:
                run(CLI, "upgrade", image, stdout=subprocess.DEVNULL)
            m = Mount(writer, image, point)
            try:
                exercised(m)
            finally:
                m.close()
            transcripts[writer, upgrade] = m.transcript
            run(CLI, "check", image, stdout=subprocess.DEVNULL)
            for reader in adapters:
                m = Mount(reader, image, point)
                try:
                    listings[writer, reader] = listing(point)
                finally:
                    m.close()
        if len(adapters) > 1:
            first, *rest = adapters
            for other in rest:
                theirs, ours = transcripts[first, upgrade], transcripts[other, upgrade]
                for step, (left, right) in enumerate(zip(theirs, ours)):
                    check("both", left == right, f"step {step} agrees: {left}" if left == right
                          else f"step {step} differs:\n  {first}: {left}\n  {other}: {right}")
                check("both", len(theirs) == len(ours), "the same number of steps")
        expected = next(iter(listings.values()))
        for (writer, reader), found in listings.items():
            check(f"{writer}->{reader}", found == expected,
                  f"{'upgraded' if upgrade else 'plain'} volume lists the same, {len(found)} objects")
    print("PASS:", work)


if __name__ == "__main__":
    main()
