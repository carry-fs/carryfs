import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def release(version, **changes):
    names = ["SHA256SUMS", f"carryfs-{version}-windows-x86_64.zip"]
    names += [f"carryfs-{version}-{platform}.tar.gz" for platform in ("linux-x86_64", "macos-arm64")]
    row = dict(tag_name=f"v{version}", draft=False, prerelease=False,
               assets=[dict(name=name, state="uploaded") for name in names])
    row.update(changes)
    return row


def publisher_step(name):
    source = (ROOT / ".github/workflows/publish-scripts.yml").read_text()
    source = source.split(f"- name: {name}\n", 1)[1].split("        run: |\n", 1)[1]
    lines = []
    for line in source.splitlines():
        if not line.startswith("          "):
            break
        lines.append(line[10:])
    return "\n".join(lines)


class ReleaseIndexTests(unittest.TestCase):
    def generate(self, pages):
        return subprocess.run([sys.executable, str(ROOT / "scripts/release-index.py")],
                              input=json.dumps(pages), capture_output=True, text=True, check=True).stdout

    def test_published_complete_releases_sorted_across_pages(self):
        pages = [[release("1.0.0"), release("0.1.10")], [release("0.1.9"), release("0.1.10")]]
        self.assertEqual(self.generate(pages), "0.1.9\n0.1.10\n1.0.0\n")

    def test_unpublished_invalid_and_incomplete_releases_are_not_advertised(self):
        uploading = release("0.1.5")
        uploading["assets"][0]["state"] = "open"
        pages = [[release("0.1.0", draft=True), release("0.1.1", prerelease=True),
                  release("0.1.2-rc.1"), release("00.1.3"), release("0.1.4", assets=[]), uploading]]
        self.assertEqual(self.generate(pages), "")

    def test_empty_repository_has_empty_index(self):
        self.assertEqual(self.generate([[]]), "")

    def test_every_package_and_checksum_must_be_uploaded(self):
        complete = release("0.1.0")
        for missing in complete["assets"]:
            with self.subTest(missing=missing["name"]):
                assets = [asset for asset in complete["assets"] if asset != missing]
                self.assertEqual(self.generate([[release("0.1.0", assets=assets)]]), "")

    def test_invalid_metadata_fails_without_emitting_partial_index(self):
        result = subprocess.run([sys.executable, str(ROOT / "scripts/release-index.py")],
                                input=json.dumps([[release("0.1.0")], [{}]]),
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")


@unittest.skipIf(os.name == "nt", "Website publication runs on Linux")
class PublisherTests(unittest.TestCase):
    def test_tag_must_appear_in_complete_index_before_publication(self):
        for pages, expected_code in [([[release("0.1.0")]], 0), ([[]], 1)]:
            with self.subTest(pages=pages), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "bin").mkdir()
                (root / "scripts").mkdir()
                (root / "scripts/release-index.py").write_text((ROOT / "scripts/release-index.py").read_text())
                (root / "metadata.json").write_text(json.dumps(pages))
                gh = root / "bin/gh"
                gh.write_text('#!/bin/sh\ncat metadata.json\n')
                gh.chmod(0o755)
                env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"],
                           GITHUB_REF="refs/tags/v0.1.0", GITHUB_REPOSITORY="carry-fs/carryfs")
                result = subprocess.run(["bash", "-c", publisher_step("Generate release index")],
                                        cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, expected_code, result.stderr)
                if expected_code:
                    self.assertIn("missing or incomplete", result.stdout)

    def test_tag_updates_index_without_overwriting_newer_website_scripts(self):
        for ref, expected_script in [("refs/tags/v0.1.0", "newer website script"),
                                     ("refs/heads/master", "checkout script")]:
            with self.subTest(ref=ref), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "website").mkdir()
                (root / "bin").mkdir()
                for name in ["install.sh", "install.ps1"]:
                    (root / name).write_text("checkout script")
                    (root / "website" / name).write_text("newer website script")
                (root / "releases.txt").write_text("0.1.0\n")
                git = root / "bin/git"
                # Stop after staging; never create a commit or contact a remote.
                git.write_text('#!/bin/sh\nexit 0\n')
                git.chmod(0o755)
                env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"], GITHUB_REF=ref)
                result = subprocess.run(["bash", "-c", publisher_step("Copy and push")],
                                        cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((root / "website/releases.txt").read_text(), "0.1.0\n")
                for name in ["install.sh", "install.ps1"]:
                    self.assertEqual((root / "website" / name).read_text(), expected_script)


@unittest.skipIf(os.name == "nt", "Unix installer runs on Linux and macOS")
class UnixSelectionTests(unittest.TestCase):
    def select(self, index, installed="", upgrade=0, requested="", download_status=0):
        # Load the real functions without invoking the installer's system entry point.
        functions = (ROOT / "install.sh").read_text().split("\nmain() {", 1)[0]
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary) / "index"
            fixture.write_bytes(index.encode())
            script = functions + '''
work=$1 installed=$2 upgrade=$3 requested=$4
download() {
    [ "$1" = https://carryfs.com/releases.txt ] || exit 99
    [ "$DOWNLOAD_STATUS" = 0 ] || exit "$DOWNLOAD_STATUS"
    cp "$INDEX_FIXTURE" "$2"
}
select_release
printf '%s\\n' "$selected"
'''
            env = dict(os.environ, INDEX_FIXTURE=str(fixture), DOWNLOAD_STATUS=str(download_status))
            return subprocess.run(["sh", "-s", "--", temporary, installed, str(upgrade), requested],
                                  input=script, env=env, capture_output=True, text=True)

    def test_fresh_install_chooses_highest_stable_version(self):
        result = self.select("0.1.9\n1.0.0\n0.1.10\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "1.0.0\n")

    def test_existing_major_is_preserved_unless_upgrade_requested(self):
        for upgrade, expected in [(0, "0.1.10"), (1, "1.0.0")]:
            with self.subTest(upgrade=upgrade):
                result = self.select("0.1.10\r\n\r\n1.0.0\r\n", "0.1.0", upgrade)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, expected + "\n")

    def test_missing_major_and_empty_index_fail(self):
        for index in ["", "1.0.0\n"]:
            result = self.select(index, "0.1.0")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("No published stable release", result.stderr)

    def test_invalid_index_fails_without_selecting_partial_contents(self):
        for invalid in ["<html>unavailable</html>", "0.1.1-rc.1", "v0.1.1", "01.1.1", " 0.1.1"]:
            result = self.select("0.1.10\n" + invalid + "\n")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Invalid release index", result.stderr)

    def test_explicit_version_bypasses_index_even_when_unavailable(self):
        result = self.select("", requested="0.1.0-rc.1", download_status=22)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "0.1.0-rc.1\n")

    def test_index_download_failure_is_not_a_successful_selection(self):
        result = self.select("0.1.10\n", download_status=22)
        self.assertEqual(result.returncode, 22)
        self.assertEqual(result.stdout, "")

    def test_current_and_newer_installed_versions_are_not_replaced_by_default(self):
        for installed in ["0.1.10", "0.1.11"]:
            result = self.select("0.1.10\n", installed)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("already current", result.stdout)


if __name__ == "__main__":
    unittest.main()
