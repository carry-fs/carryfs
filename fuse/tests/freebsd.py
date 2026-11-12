#!/usr/bin/env python3
"""Exercise FreeBSD FUSE against disposable file-backed GEOM devices."""

import errno
import hashlib
import os
from pathlib import Path
import select
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[2]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")) / "debug"
CLI = TARGET / "carryfs"
FUSE = TARGET / "carryfs-fuse"
BYTES = 256 << 20


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, timeout=90, **kwargs)


class MountedVolume(unittest.TestCase):
    def setUp(self):
        (REPO / "build").mkdir(exist_ok=True)
        self.work = Path(tempfile.mkdtemp(prefix="freebsd-", dir=REPO / "build"))
        self.work.chmod(0o755)
        self.image = self.work / "disk.img"
        self.point = self.work / "mnt"
        self.point.mkdir()
        self.log = (self.work / "daemon.log").open("w")
        self.daemon = None
        self.device = None
        self.addCleanup(self.cleanup)
        run(CLI, "mkfs", self.image, BYTES, stdout=self.log)
        self.device = "/dev/" + subprocess.check_output(
            ["mdconfig", "-a", "-t", "vnode", "-f", str(self.image)], text=True
        ).strip()

    def cleanup(self):
        if self.daemon is not None and self.daemon.poll() is None:
            self.daemon.kill()
            self.daemon.wait(timeout=20)
        if os.path.ismount(self.point):
            run("umount", "-f", self.point)
        if self.device is not None:
            run("mdconfig", "-d", "-u", Path(self.device).name)
        self.log.close()
        # Extraction creates private directories; VM synchronization runs as the CI user.
        run("chmod", "-R", "-P", "a+rX", self.work)

    def mount(self, options="rw", extra_env=None):
        env = os.environ.copy()
        env.update(extra_env or {})
        self.daemon = subprocess.Popen(
            [str(FUSE), "serve", self.device, str(self.point), options],
            stdout=subprocess.PIPE, stderr=self.log, env=env,
        )
        # A stat during nmount can hold the vnode that the FUSE handshake needs.
        with self.daemon.stdout:
            ready, _, _ = select.select([self.daemon.stdout], [], [], 45)
            if not ready or self.daemon.stdout.readline().strip() != b"ready":
                raise AssertionError(f"mount failed; see {self.work / 'daemon.log'}")

    def unmount(self):
        run("umount", self.point)
        self.assertEqual(self.daemon.wait(timeout=20), 0)
        self.daemon = None

    def recover(self):
        run("mdconfig", "-d", "-u", Path(self.device).name)
        self.device = None
        run(CLI, "check", self.image, stdout=self.log)
        run(CLI, "scrub", self.image, stdout=self.log)
        destination = self.work / "recovered"
        run(CLI, "get", self.image, "/", destination, stdout=self.log)
        return destination

    def test_namespace_io_and_clean_unmount(self):
        self.mount()
        with self.assertRaises(OSError):
            os.mknod(self.point / "device", stat.S_IFCHR | 0o600, os.makedev(0, 0))
        self.assertFalse((self.point / "device").exists())
        directory = self.point / "Folder"
        directory.mkdir()
        payload = bytes(range(251)) * 12000
        path = directory / "Notes.txt"
        path.write_bytes(payload)
        self.assertEqual((self.point / "folder/NOTES.TXT").read_bytes(), payload)
        with path.open("r+b") as file:
            file.seek(4093)
            file.write(b"changed")
            file.truncate(len(payload) - 13)
            file.flush()
            os.fsync(file.fileno())
        expected = payload[:4093] + b"changed" + payload[4100:-13]
        path.rename(directory / "Renamed")
        with self.assertRaises(FileNotFoundError):
            path.stat()
        link = self.point / "link"
        link.symlink_to("Folder/Renamed")
        self.assertEqual(link.read_bytes(), expected)
        (self.point / "temporary").write_bytes(b"delete me")
        (self.point / "TEMPORARY").unlink()
        self.unmount()
        recovered = self.recover()
        self.assertEqual((recovered / "Folder/Renamed").read_bytes(), expected)
        self.assertEqual(os.readlink(recovered / "link"), "Folder/Renamed")
        self.assertFalse((recovered / "temporary").exists())

    def test_case_aliases_do_not_leave_stale_names(self):
        self.mount()
        path = self.point / "Stored"
        path.write_bytes(b"old")
        path.stat()
        (self.point / "STORED").unlink()
        with self.assertRaises(FileNotFoundError):
            path.stat()
        path.write_bytes(b"new")
        (self.point / "stored").rename(self.point / "Changed")
        with self.assertRaises(FileNotFoundError):
            path.stat()
        self.assertEqual((self.point / "changed").read_bytes(), b"new")
        self.unmount()

    def test_read_only_mount_rejects_writes_and_preserves_image(self):
        def digest():
            hasher = hashlib.sha256()
            with self.image.open("rb") as image:
                for chunk in iter(lambda: image.read(1 << 20), b""):
                    hasher.update(chunk)
            return hasher.digest()
        before = digest()
        self.mount("ro")
        with self.assertRaises(OSError) as error:
            (self.point / "forbidden").write_bytes(b"no")
        self.assertEqual(error.exception.errno, errno.EROFS)
        self.unmount()
        self.assertEqual(digest(), before)
        self.assertEqual(list(self.recover().iterdir()), [])

    def test_competing_mount_formatter_and_offline_reader_are_rejected(self):
        self.mount()
        other = self.work / "other"
        other.mkdir()
        alias = self.work / "alias"
        alias.symlink_to(self.device)
        for args in [
            [FUSE, "serve", alias, other, "rw"],
            [FUSE, "mkfs", "-f", alias],
            [CLI, "check", "--bytes", BYTES, alias],
        ]:
            result = subprocess.run([str(arg) for arg in args], capture_output=True, timeout=20)
            self.assertNotEqual(result.returncode, 0, args)
            self.assertIn(b"in use", result.stderr.lower(), result.stderr)
        self.unmount()
        self.recover()

    def test_unmount_waits_for_publication_and_holds_the_device(self):
        marker = self.work / "pause-unmount"
        marker.touch()
        self.mount(extra_env={"CARRYFS_PAUSE_BEFORE_DESTROY": str(marker)})
        (self.point / "unmount").write_bytes(b"preserved")
        unmount = subprocess.Popen(["umount", str(self.point)], stdout=self.log, stderr=self.log)
        try:
            deadline = time.monotonic() + 30
            while "paused before the unmount publication" not in (self.work / "daemon.log").read_text():
                if unmount.poll() is not None or time.monotonic() > deadline:
                    self.fail("unmount never reached the publication hook")
                time.sleep(.01)
            self.assertIsNone(unmount.poll())
            reader = subprocess.run([str(CLI), "check", "--bytes", str(BYTES), self.device], capture_output=True, timeout=20)
            self.assertNotEqual(reader.returncode, 0)
            self.assertIn(b"in use", reader.stderr.lower())
        finally:
            marker.unlink(missing_ok=True)
            unmount.wait(timeout=30)
        self.assertEqual(unmount.returncode, 0)
        self.assertEqual(self.daemon.wait(timeout=20), 0)
        self.daemon = None
        self.assertEqual((self.recover() / "unmount").read_bytes(), b"preserved")

    def test_signal_publishes_and_unmounts(self):
        self.mount()
        (self.point / "signal").write_bytes(b"preserved")
        self.daemon.send_signal(signal.SIGTERM)
        self.assertEqual(self.daemon.wait(timeout=30), 0)
        self.assertFalse(os.path.ismount(self.point))
        self.daemon = None
        self.assertEqual((self.recover() / "signal").read_bytes(), b"preserved")

    def test_directory_fsync_survives_daemon_crash(self):
        for operation in ("create", "rename", "unlink"):
            for datasync in (False, True):
                with self.subTest(operation=operation, datasync=datasync):
                    self.mount()
                    before = self.point / "before"
                    with before.open("wb") as file:
                        file.write(b"preserved")
                        file.flush()
                        os.fsync(file.fileno())
                    if operation == "create":
                        (self.point / "after").touch()
                    elif operation == "rename":
                        before.rename(self.point / "after")
                    else:
                        before.unlink()
                    directory = os.open(self.point, os.O_RDONLY | os.O_DIRECTORY)
                    try:
                        (os.fdatasync if datasync else os.fsync)(directory)
                        self.daemon.kill()
                    finally:
                        os.close(directory)
                    self.daemon.wait(timeout=20)
                    self.daemon = None
                    run("umount", "-f", self.point)
                    result = run(CLI, "get", "--bytes", BYTES, self.device, "/", self.work / f"{operation}-{datasync}", stdout=self.log)
                    self.assertEqual(result.returncode, 0)
                    expected = {"create": {"before", "after"}, "rename": {"after"}, "unlink": {}}[operation]
                    self.assertEqual({p.name for p in (self.work / f"{operation}-{datasync}").iterdir()}, set(expected))
                    # Reset the disposable device for the next independent operation.
                    run(FUSE, "mkfs", "-f", self.device, stdout=self.log)

    def test_flush_failure_reaches_fsync(self):
        marker = self.work / "fail-flush"
        self.mount(extra_env={"CARRYFS_FAIL_FLUSHES": str(marker)})
        with (self.point / "failure").open("wb") as file:
            file.write(b"unsynced")
            file.flush()
            marker.touch()
            with self.assertRaises(OSError) as error:
                os.fsync(file.fileno())
            self.assertEqual(error.exception.errno, errno.EIO)
        self.unmount()
        self.recover()

    def test_formatter_refuses_a_partitioned_disk(self):
        run("gpart", "create", "-s", "GPT", self.device, stdout=self.log)
        with self.image.open("rb") as image:
            before = image.read(1 << 20)
        result = subprocess.run([str(FUSE), "mkfs", "-f", self.device], capture_output=True, timeout=20)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"partitions", result.stderr.lower(), result.stderr)
        with self.image.open("rb") as image:
            self.assertEqual(image.read(1 << 20), before)

    def test_formatter_refuses_a_native_read_only_mount(self):
        run("newfs", "-U", self.device, stdout=self.log)
        run("mount", "-t", "ufs", "-o", "ro", self.device, self.point)
        result = subprocess.run([str(FUSE), "mkfs", "-f", self.device], capture_output=True, timeout=20)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"in use", result.stderr.lower(), result.stderr)
        run("umount", self.point)

    def wait_until_released(self, device):
        """Poll until the daemon exits and an offline reader may open `device`."""
        deadline = time.monotonic() + 20
        while True:
            result = subprocess.run([str(CLI), "check", "--bytes", str(BYTES), device], capture_output=True)
            if result.returncode == 0:
                return
            if time.monotonic() > deadline:
                self.fail(result.stderr.decode())
            time.sleep(.01)

    def test_mount_helper_and_raw_formatter(self):
        run(FUSE, "mkfs", "-f", "-L", "FreeBSD test", self.device, stdout=self.log)
        helper = self.work / "mount_carryfs"
        helper.symlink_to(FUSE)
        logs = self.work / "helper-logs"
        logs.mkdir()
        env = {**os.environ, "TMPDIR": str(logs)}
        run("mount", "-t", "carryfs", "-o", f"mountprog={helper},rw,sync", self.device, self.point, env=env)
        self.assertTrue(os.path.ismount(self.point))
        (self.point / "helper").write_bytes(b"mounted")
        run("umount", self.point)
        self.wait_until_released(self.device)
        # A daemon that exits cleanly with nothing to report leaves no log.
        self.assertEqual(list(logs.iterdir()), [])
        self.assertEqual((self.recover() / "helper").read_bytes(), b"mounted")

    def test_fstab_entry_mounts(self):
        helper = self.work / "mount_carryfs"
        helper.symlink_to(FUSE)
        fstab = self.work / "fstab"
        fstab.write_text(f"{self.device} {self.point} carryfs rw,noauto,late,failok,mountprog={helper} 0 0\n")
        run("mount", "-F", fstab, self.point)
        self.assertTrue(os.path.ismount(self.point))
        (self.point / "fstab").write_bytes(b"mounted")
        run("umount", self.point)
        self.wait_until_released(self.device)
        self.assertEqual((self.recover() / "fstab").read_bytes(), b"mounted")

    def test_other_accounts_cannot_write(self):
        # A mount point every account can reach, so a refusal is the mount's.
        point = Path(tempfile.mkdtemp(prefix="carryfs-users-"))
        point.chmod(0o755)
        self.addCleanup(lambda: os.path.ismount(point) or point.rmdir())
        self.point = point
        self.mount()
        (point / "shared").write_bytes(b"readable")

        def nobody(*command):
            def drop():
                os.setgroups([])
                os.setgid(65534)
                os.setuid(65534)
            return subprocess.run(command, capture_output=True, timeout=20, preexec_fn=drop)

        self.assertEqual(nobody("cat", str(point / "shared")).stdout, b"readable")
        for command in [("touch", str(point / "new")),
                        ("rm", "-f", str(point / "shared")),
                        ("truncate", "-s", "0", str(point / "shared"))]:
            result = nobody(*command)
            self.assertNotEqual(result.returncode, 0, command)
        self.assertEqual((point / "shared").read_bytes(), b"readable")
        self.assertFalse((point / "new").exists())
        self.unmount()

    def test_label_and_provider_names_share_one_lock(self):
        # A CarryFS partition needs at least 256 MiB, so grow the disk first.
        run("mdconfig", "-d", "-u", Path(self.device).name)
        os.truncate(self.image, 2 * BYTES)
        self.device = "/dev/" + subprocess.check_output(
            ["mdconfig", "-a", "-t", "vnode", "-f", str(self.image)], text=True
        ).strip()
        run("gpart", "create", "-s", "GPT", self.device, stdout=self.log)
        label = f"carryfs{os.getpid()}"
        run("gpart", "add", "-t", "freebsd-ufs", "-a", "1m", "-s", f"{BYTES >> 20}m", "-l", label,
            self.device, stdout=self.log)
        partition = self.device + "p1"
        run(FUSE, "mkfs", "-f", partition, stdout=self.log)
        # GEOM removes a label while its provider is open for writing.
        labelled = Path("/dev/gpt") / label
        deadline = time.monotonic() + 20
        while not labelled.exists():
            if time.monotonic() > deadline:
                self.fail(f"{labelled} did not appear")
            time.sleep(.05)
        self.device, md = str(labelled), self.device
        try:
            self.mount()
            (self.point / "kept").write_bytes(b"preserved")
            other = self.work / "other"
            other.mkdir()
            for args in [
                [FUSE, "serve", partition, other, "rw"],
                [FUSE, "mkfs", "-f", partition],
                [CLI, "check", "--bytes", BYTES, partition],
            ]:
                result = subprocess.run([str(arg) for arg in args], capture_output=True, timeout=20)
                self.assertNotEqual(result.returncode, 0, args)
                self.assertIn(b"in use", result.stderr.lower(), result.stderr)
            self.assertEqual((self.point / "kept").read_bytes(), b"preserved")
            self.unmount()
        finally:
            self.device = md


if __name__ == "__main__":
    if not sys.platform.startswith("freebsd"):
        raise SystemExit("Run on FreeBSD with the fusefs kernel module loaded.")
    if os.geteuid() != 0:
        raise SystemExit("Run as root; the tests allocate and remove their own md devices.")
    unittest.main(verbosity=2)
