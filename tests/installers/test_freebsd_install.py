"""Exercise FreeBSD payload installation without touching system paths."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


@unittest.skipUnless(os.name == "posix", "Unix shell installer")
class FreeBSDInstallTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.host = self.work / "host"
        self.payload = self.work / "payload"
        self.payload.mkdir()
        for directory in ("bin", "var/run", "var/tmp", "usr/local/sbin"):
            (self.host / directory).mkdir(parents=True, exist_ok=True)
        for name, text in {"carryfs": "cli", "carryfs-fuse": "fuse", "VERSION": "0.1.0\n"}.items():
            (self.payload / name).write_text(text)
        for name, command in {"id": "echo 0", "pgrep": "exit 1"}.items():
            tool = self.host / "bin" / name
            tool.write_text("#!/bin/sh\n" + command + "\n")
            tool.chmod(0o755)
        source = (ROOT / "packaging/install-unix.sh").read_text()
        for prefix in ("/usr/local", "/var/run", "/var/tmp"):
            source = source.replace(prefix, str(self.host) + prefix)
        self.installer = self.work / "install.sh"
        self.installer.write_text(source)

    def install(self, platform, expected=""):
        env = dict(os.environ, PATH=str(self.host / "bin") + os.pathsep + os.environ["PATH"])
        return subprocess.run(["sh", str(self.installer), str(self.payload), platform, expected],
                              env=env, capture_output=True, text=True)

    def test_installs_and_upgrades_each_freebsd_payload(self):
        for platform in ("freebsd14-x86_64", "freebsd14-arm64", "freebsd15-x86_64", "freebsd15-arm64"):
            with self.subTest(platform=platform):
                marker = self.host / "usr/local/libexec/carryfs/VERSION"
                result = self.install(platform, marker.read_text().strip() if marker.exists() else "")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(marker.read_text(), "0.1.0\n")
                self.assertEqual((self.host / "usr/local/bin/carryfs").read_text(), "cli")
                for name in ("mount_carryfs", "newfs_carryfs"):
                    helper = self.host / "usr/local/sbin" / name
                    self.assertTrue(helper.is_symlink())
                    self.assertEqual(helper.read_text(), "fuse")
                self.assertFalse((self.host / "var/run/carryfs-install.lock").exists())

    def test_refuses_to_replace_an_unrelated_helper(self):
        foreign = self.host / "usr/local/sbin/mount_carryfs"
        foreign.write_text("another filesystem")
        result = self.install("freebsd15-arm64")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("another installation", result.stderr)
        self.assertEqual(foreign.read_text(), "another filesystem")
        self.assertFalse((self.host / "usr/local/bin/carryfs").exists())

    def test_refuses_a_stale_expected_version(self):
        self.assertEqual(self.install("freebsd15-arm64").returncode, 0)
        (self.payload / "carryfs").write_text("replacement")
        result = self.install("freebsd15-arm64", "0.0.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changed during download", result.stderr)
        self.assertEqual((self.host / "usr/local/bin/carryfs").read_text(), "cli")
