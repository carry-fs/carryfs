#!/usr/bin/env python3
"""Run the shared BSD mounted contract against NetBSD PUFFS and vnd disks."""

import os
from pathlib import Path
import subprocess
import signal
import sys
import tempfile
import unittest

import freebsd
from freebsd import BYTES, CLI, FUSE, REPO, run


class MountedVolume(freebsd.MountedVolume):
    def setUp(self):
        def timed_out(_signal, _frame):
            raise TimeoutError("NetBSD mounted test exceeded 300 seconds")
        signal.signal(signal.SIGALRM, timed_out)
        signal.alarm(300)
        self.addCleanup(signal.alarm, 0)
        (REPO / "build").mkdir(exist_ok=True)
        self.work = Path(tempfile.mkdtemp(prefix="netbsd-", dir=REPO / "build"))
        self.work.chmod(0o755)
        self.image = self.work / "disk.img"
        self.point = self.work / "mnt"
        self.point.mkdir()
        self.log = (self.work / "daemon.log").open("w")
        self.daemon = None
        self.device = None
        self.addCleanup(self.cleanup)
        run(CLI, "mkfs", self.image, BYTES, stdout=self.log)
        # Select an unused unit; never reconfigure a host's existing disk.
        listing = run("vndconfig", "-l", "-m", "32", capture_output=True, text=True).stdout
        self.unit = next(line.split(":", 1)[0] for line in listing.splitlines()
                         if line.endswith(": not in use"))
        self.assertRegex(self.unit, r"^vnd[0-9]+$")
        run("vndconfig", "-c", self.unit, self.image)
        # amd64's raw whole-disk partition is d.
        self.device = f"/dev/r{self.unit}d"

    def cleanup(self):
        if self.daemon is not None and self.daemon.poll() is None:
            self.daemon.kill()
            self.daemon.wait(timeout=20)
        if os.path.ismount(self.point):
            run("umount", "-f", self.point)
        if self.device is not None:
            run("vndconfig", "-u", self.unit)
        self.log.close()
        run("chmod", "-R", "-P", "a+rX", self.work)

    def recover(self):
        run("vndconfig", "-u", self.unit)
        self.device = None
        run(CLI, "check", self.image, stdout=self.log)
        run(CLI, "scrub", self.image, stdout=self.log)
        destination = self.work / "recovered"
        run(CLI, "get", self.image, "/", destination, stdout=self.log)
        return destination

    def test_formatter_refuses_a_partitioned_disk(self):
        run("gpt", "create", "-f", self.device, stdout=self.log)
        run("gpt", "add", "-t", "ffs", self.device, stdout=self.log)
        run("dkctl", self.device, "makewedges", stdout=self.log)
        with self.image.open("rb") as image:
            before = image.read(1 << 20)
        result = subprocess.run([str(FUSE), "mkfs", "-f", self.device], capture_output=True, timeout=20)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"partitions", result.stderr.lower(), result.stderr)
        with self.image.open("rb") as image:
            self.assertEqual(image.read(1 << 20), before)

    def test_gpt_wedge_mounts_and_preserves_data(self):
        run("vndconfig", "-u", self.unit)
        self.device = None
        os.truncate(self.image, 2 * BYTES)
        run("vndconfig", "-c", self.unit, self.image)
        self.device = f"/dev/r{self.unit}d"
        run("gpt", "create", "-f", self.device, stdout=self.log)
        run("gpt", "add", "-t", "ffs", "-s", BYTES // 512, self.device, stdout=self.log)
        run("dkctl", self.device, "makewedges", stdout=self.log)
        listing = run("dkctl", self.device, "listwedges", capture_output=True, text=True).stdout
        wedge = next(line.split(":", 1)[0] for line in listing.splitlines() if line.startswith("dk"))
        parent = self.device
        self.device = f"/dev/{wedge}"
        try:
            run(FUSE, "mkfs", "-f", self.device, stdout=self.log)
            self.mount()
            (self.point / "wedge").write_bytes(b"preserved")
            self.unmount()
            destination = self.work / "wedge-recovered"
            run(CLI, "check", "--bytes", BYTES, self.device, stdout=self.log)
            run(CLI, "get", "--bytes", BYTES, self.device, "/", destination, stdout=self.log)
            self.assertEqual((destination / "wedge").read_bytes(), b"preserved")
        finally:
            self.device = parent

    def test_read_only_root_permissions_use_the_mount_owner(self):
        run("vndconfig", "-u", self.unit)
        self.device = None
        run(CLI, "attr", self.image, "/", "mode=0700", stdout=self.log)
        run("vndconfig", "-c", self.unit, self.image)
        self.device = f"/dev/r{self.unit}d"
        self.shared_point()
        self.mount("ro,uid=4242,gid=4242")
        self.assertEqual(self.user(4242, 'ls -A "$MNT"')[0], 0)
        self.assertNotEqual(self.user(65534, 'ls -A "$MNT"')[0], 0)
        self.unmount()

    def test_noexec_mount_rejects_execution(self):
        self.mount("rw,noexec")
        script = self.point / "script"
        script.write_text("#!/bin/sh\nexit 0\n")
        script.chmod(0o755)
        with self.assertRaises(PermissionError):
            subprocess.run([str(script)], check=True, timeout=20)
        self.unmount()

    def test_offline_raw_repair_restores_a_damaged_root_mirror(self):
        offset = 256 * 4096 + 512
        with self.image.open("r+b") as image:
            image.seek(offset)
            original = image.read(1)
            image.seek(offset)
            image.write(bytes([original[0] ^ 1]))
        fsck = CLI.with_name("fsck-carryfs")
        repair = subprocess.run([str(fsck), "--repair", "--report", str(self.work / "repair.txt"),
                                 "--bytes", str(BYTES), self.device],
                                capture_output=True, text=True, timeout=90)
        self.assertEqual(repair.returncode, 1, repair.stdout + repair.stderr)
        run(CLI, "check", "--bytes", BYTES, self.device, stdout=self.log)
        with self.image.open("rb") as image:
            image.seek(offset)
            self.assertEqual(image.read(1), original)
        self.assertEqual(list(self.recover().iterdir()), [])

    def test_formatter_refuses_a_native_read_only_mount(self):
        run("newfs", "-I", self.device, stdout=self.log)
        block = f"/dev/{self.unit}d"
        run("mount", "-t", "ffs", "-o", "ro", block, self.point)
        result = subprocess.run([str(FUSE), "mkfs", "-f", self.device], capture_output=True, timeout=20)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"in use", result.stderr.lower(), result.stderr)
        run("umount", self.point)

    def install_helper(self):
        # NetBSD mount(8) searches /sbin and /usr/sbin, rather than /usr/local/sbin.
        helper = Path("/sbin/mount_carryfs")
        self.assertFalse(helper.exists(), f"refusing to replace {helper}")
        helper.symlink_to(FUSE)
        self.addCleanup(helper.unlink)

    def test_mount_helper_and_raw_formatter(self):
        run(FUSE, "mkfs", "-f", "-L", "NetBSD test", self.device, stdout=self.log)
        self.install_helper()
        run("mount", "-t", "carryfs", "-o", "rw,sync", self.device, self.point)
        self.assertTrue(os.path.ismount(self.point))
        (self.point / "helper").write_bytes(b"mounted")
        run("umount", self.point)
        self.wait_until_released(self.device)
        self.assertEqual((self.recover() / "helper").read_bytes(), b"mounted")

    def test_fstab_entry_mounts(self):
        self.install_helper()
        fstab = Path("/etc/fstab")
        original = fstab.read_bytes()
        self.addCleanup(fstab.write_bytes, original)
        with fstab.open("a") as file:
            file.write(f"\n{self.device} {self.point} carryfs rw,noauto 0 0\n")
        run("mount", self.point)
        self.assertTrue(os.path.ismount(self.point))
        (self.point / "fstab").write_bytes(b"mounted")
        run("umount", self.point)
        self.wait_until_released(self.device)
        self.assertEqual((self.recover() / "fstab").read_bytes(), b"mounted")

    def test_label_and_provider_names_share_one_lock(self):
        # Both block and raw names, plus symlinks, must share the raw-node lease.
        self.mount()
        block = f"/dev/{self.unit}d"
        other = self.work / "other"
        other.mkdir()
        for args in ([FUSE, "serve", block, other, "rw"],
                     [FUSE, "mkfs", "-f", block],
                     [CLI, "check", "--bytes", BYTES, block]):
            result = subprocess.run([str(arg) for arg in args], capture_output=True, timeout=20)
            self.assertNotEqual(result.returncode, 0, args)
            self.assertIn(b"in use", result.stderr.lower(), result.stderr)
        self.unmount()


if __name__ == "__main__":
    if not sys.platform.startswith("netbsd") or os.geteuid() != 0:
        raise SystemExit("Run as root on NetBSD; tests allocate their own vnd disks.")
    unittest.main(verbosity=2)
