import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
KERNEL = "7.2.0-test"


@unittest.skipUnless(os.name == "posix" and os.uname().sysname == "Linux", "Linux installer")
class NativeInstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.host = self.work / "host"
        self.payload = self.work / "payload"
        self.payload.mkdir()
        for directory in ("bin", "var/run", "var/tmp", "usr/sbin", "etc/udisks2", f"lib/modules/{KERNEL}"):
            (self.host / directory).mkdir(parents=True, exist_ok=True)
        self.kernel_config()
        for name, content in {
            "carryfs": "cli-v1", "carryfs-fuse": "fuse-v1", "VERSION": "0.1.0\n",
            "61-carryfs.rules": "CarryFS rule\n",
            "mount_options.conf": "[defaults]\ncarryfs_defaults=uid=$UID\ncarryfs_allow=uid=$UID\n",
        }.items():
            (self.payload / name).write_text(content)
        (self.host / "etc/udisks2/mount_options.conf").write_text("[defaults]\nother_defaults=keep\n")
        self.env = dict(os.environ, PATH=str(self.host / "bin") + os.pathsep + os.environ["PATH"],
                        TEST_KERNEL=KERNEL, EVENT_LOG=str(self.work / "events"),
                        FAILURE_MARKER=str(self.work / "failed"))
        self.tool("id", "echo 0")
        self.tool("pgrep", 'exit "${PGREP_STATUS:-1}"')
        self.tool("uname", 'case $1 in -r) echo "$TEST_KERNEL" ;; *) exit 99 ;; esac')
        self.tool("modinfo", 'printf "%s SMP\\n" "${MODULE_KERNEL:-$TEST_KERNEL}"')
        for name in ("depmod", "udevadm"):
            self.tool(name, f'''printf '{name} %s\\n' "$*" >> "$EVENT_LOG"
if [ "${{FAIL_COMMAND:-}}" = {name} ] && {{ [ ! -e "$FAILURE_MARKER" ] || [ "${{FAIL_ALWAYS:-0}}" = 1 ]; }}; then
    if [ "${{FAIL_KERNEL:-${{2:-}}}}" = "${{2:-}}" ]; then
        : > "$FAILURE_MARKER"
        exit 1
    fi
fi''')
        for name in ("modprobe", "insmod", "rmmod", "sudo"):
            self.tool(name, f"echo 'unexpected {name}' >&2; exit 99")
        self.installer = self.work / "install-system.sh"
        self.installer.write_text(self.scoped((ROOT / "packaging/install-unix.sh").read_text()))
        self.functions = self.scoped((ROOT / "install.sh").read_text().split("\nmain() {", 1)[0])

    def tool(self, name, source):
        path = self.host / "bin" / name
        path.write_text("#!/bin/sh\nset -eu\n" + source + "\n")
        path.chmod(0o755)

    def scoped(self, source):
        # Run the real shell code with every system destination inside the fixture.
        for prefix in ("/usr/local/bin", "/usr/libexec", "/usr/sbin", "/etc", "/lib/modules",
                       "/var/run", "/var/tmp", "/sys/module", "/sys/kernel"):
            source = source.replace(prefix, str(self.host) + prefix)
        return source

    def module(self, kernel=KERNEL):
        return self.host / f"lib/modules/{kernel}/updates/carryfs.ko"

    def kernel_config(self, kernel=KERNEL):
        path = self.host / f"lib/modules/{kernel}/build/include/config/auto.conf"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("CONFIG_RUST=y\n")
        return path

    def snapshot(self):
        result = {}
        for directory in ("usr", "etc", "lib"):
            for path in (self.host / directory).rglob("*"):
                if path.is_symlink():
                    result[str(path.relative_to(self.host))] = os.readlink(path)
                elif path.is_file():
                    result[str(path.relative_to(self.host))] = path.read_bytes()
        return result

    def install(self, expected="", native=True, kernel=KERNEL, module=b"native-v2", **env):
        if native:
            (self.payload / "carryfs.ko").write_bytes(module)
            (self.payload / "KERNEL_RELEASE").write_text(kernel + "\n")
        else:
            for name in ("carryfs.ko", "KERNEL_RELEASE"):
                (self.payload / name).unlink(missing_ok=True)
        return subprocess.run(["sh", str(self.installer), str(self.payload), "linux-x86_64", expected],
                              env=dict(self.env, TEST_KERNEL=kernel, **env), capture_output=True, text=True)

    def uninstall(self, **env):
        return subprocess.run(["sh", "-s"], input=self.functions + "\nuninstall_linux\n",
                              env=dict(self.env, **env), capture_output=True, text=True)

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.host / "var/run/carryfs-install.lock").exists())
        self.assertEqual(list((self.host / "var/tmp").iterdir()), [])

    def test_install_reinstall_and_uninstall_across_kernels(self):
        self.assert_success(self.install())
        self.assert_success(self.install(expected="0.1.0"))
        manifest = self.host / "usr/libexec/carryfs/native-kernels"
        self.assertEqual(manifest.read_text(), KERNEL + "\n")
        self.assertEqual(self.module().read_text(), "native-v2")
        self.assertEqual(self.module().stat().st_mode & 0o777, 0o644)
        self.assertEqual(os.readlink(self.host / "usr/sbin/mount.carryfs"),
                         str(self.host / "usr/libexec/carryfs/carryfs-fuse"))
        other = "7.3.0-test"
        (self.host / f"lib/modules/{other}").mkdir()
        self.kernel_config(other)
        self.assert_success(self.install(expected="0.1.0", kernel=other))
        self.assertEqual(manifest.read_text(), f"{KERNEL}\n{other}\n")
        unrelated = self.module().with_name("other.ko")
        unrelated.write_text("keep")
        self.assert_success(self.uninstall())
        self.assert_success(self.uninstall())
        self.assertFalse(self.module().exists())
        self.assertFalse(self.module(other).exists())
        self.assertFalse(self.module(other).parent.exists())
        self.assertFalse(manifest.parent.exists())
        self.assertFalse((self.host / "usr/local/bin/carryfs").exists())
        self.assertFalse((self.host / "usr/sbin/mount.carryfs").is_symlink())
        self.assertEqual(unrelated.read_text(), "keep")
        self.assertEqual((self.host / "etc/udisks2/mount_options.conf").read_text(),
                         "[defaults]\nother_defaults=keep\n")
        events = (self.work / "events").read_text()
        self.assertIn(f"depmod -a {KERNEL}", events)
        self.assertIn(f"depmod -a {other}", events)

    def test_fuse_update_preserves_native_installation(self):
        self.assert_success(self.install())
        before = self.module().read_bytes()
        self.assert_success(self.install(expected="0.1.0", native=False))
        self.assertEqual(self.module().read_bytes(), before)
        self.assertEqual((self.host / "usr/libexec/carryfs/native-kernels").read_text(), KERNEL + "\n")
        self.assert_success(self.uninstall())

    def test_uninstall_tolerates_a_removed_kernel_directory(self):
        self.assert_success(self.install())
        manifest = self.host / "usr/libexec/carryfs/native-kernels"
        manifest.write_text(KERNEL + "\n5.15.0-removed\n")
        self.assert_success(self.uninstall())
        self.assertFalse(self.module().exists())
        self.assertFalse(manifest.exists())

    def test_real_module_index_finds_install_and_forgets_uninstall(self):
        tools = {name: shutil.which(name) for name in ("cc", "depmod", "modinfo")}
        if not all(tools.values()):
            self.skipTest("cc and kmod tools are needed for the module-index check")
        source = self.work / "module.c"
        source.write_text('''const char info[] __attribute__((section(".modinfo"), used)) =
    "name=carryfs\\0license=GPL\\0vermagic=7.2.0-test SMP\\0";
''')
        module = self.work / "carryfs.ko"
        subprocess.run([tools["cc"], "-c", str(source), "-o", str(module)], check=True, capture_output=True)
        for name in ("modules.order", "modules.builtin", "modules.builtin.modinfo"):
            (self.host / f"lib/modules/{KERNEL}" / name).touch()
        self.tool("depmod", f'exec "{tools["depmod"]}" -b "{self.host}" "$@"')
        self.tool("modinfo", f'exec "{tools["modinfo"]}" "$@"')
        self.assert_success(self.install(module=module.read_bytes()))
        command = [tools["modinfo"], "-b", str(self.host), "-k", KERNEL, "-n", "carryfs"]
        found = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(found.returncode, 0, found.stderr)
        self.assertEqual(Path(found.stdout.strip()), self.module())
        self.assert_success(self.uninstall())
        missing = subprocess.run(command, capture_output=True, text=True)
        self.assertNotEqual(missing.returncode, 0)

    def test_failed_upgrade_restores_binaries_module_and_record(self):
        self.assert_success(self.install())
        self.module().write_text("old native module")
        before = self.snapshot()
        (self.payload / "carryfs").write_text("new cli")
        (self.payload / "VERSION").write_text("0.2.0\n")
        for command in ("depmod", "udevadm"):
            with self.subTest(command=command):
                (self.work / "failed").unlink(missing_ok=True)
                result = self.install(expected="0.1.0", FAIL_COMMAND=command)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("restoring", result.stderr)
                self.assertEqual(self.snapshot(), before)
                self.assertFalse((self.host / "var/run/carryfs-install.lock").exists())

    def test_failed_first_install_leaves_no_native_files(self):
        before = self.snapshot()
        result = self.install(FAIL_COMMAND="depmod")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot(), before)
        self.assertFalse(self.module().exists())
        self.assertFalse(self.module().parent.exists())

    def test_failed_cli_replacement_removes_staging_file(self):
        self.assert_success(self.install())
        before = self.snapshot()
        real_mv = shutil.which("mv")
        self.tool("mv", f'''if [ "$3" = "{self.host / 'usr/local/bin/carryfs'}" ]; then exit 1; fi
exec "{real_mv}" "$@"''')
        result = self.install(expected="0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("restoring", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_failed_uninstall_restores_all_kernel_modules_and_configuration(self):
        self.assert_success(self.install())
        other = "7.3.0-test"
        (self.host / f"lib/modules/{other}").mkdir()
        self.kernel_config(other)
        self.assert_success(self.install(expected="0.1.0", kernel=other))
        before = self.snapshot()
        for command in ("depmod", "udevadm"):
            with self.subTest(command=command):
                (self.work / "failed").unlink(missing_ok=True)
                result = self.uninstall(FAIL_COMMAND=command, FAIL_KERNEL=other if command == "depmod" else "")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("restoring", result.stderr)
                self.assertEqual(self.snapshot(), before)
                self.assertFalse((self.host / "var/run/carryfs-install.lock").exists())

    def test_failed_index_restoration_retains_backup(self):
        self.assert_success(self.install())
        before = self.snapshot()
        result = self.uninstall(FAIL_COMMAND="depmod", FAIL_ALWAYS="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Restoration failed. Keep the backup", result.stderr)
        self.assertEqual(self.snapshot(), before)
        backups = list((self.host / "var/tmp").iterdir())
        self.assertEqual(len(backups), 1)
        self.assertTrue(any(backups[0].iterdir()))
        self.assertFalse((self.host / "var/run/carryfs-install.lock").exists())

    def test_loaded_module_blocks_install_and_uninstall(self):
        self.assert_success(self.install())
        before = self.snapshot()
        (self.host / "sys/module/carryfs").mkdir(parents=True)
        for result in (self.install(expected="0.1.0"), self.uninstall()):
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("modprobe -r carryfs", result.stderr)
            self.assertEqual(self.snapshot(), before)

    def test_unmanaged_module_is_not_overwritten(self):
        self.module().parent.mkdir(parents=True)
        self.module().write_text("belongs to someone else")
        before = self.snapshot()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("another installation", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_wrong_module_vermagic_is_rejected_before_changes(self):
        before = self.snapshot()
        result = self.install(MODULE_KERNEL="7.1.0-other")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("vermagic", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_enforced_signatures_and_lockdown_are_rejected(self):
        for name, value in (("module/module/parameters/sig_enforce", "Y\n"),
                            ("kernel/security/lockdown", "none [integrity] confidentiality\n")):
            with self.subTest(policy=name):
                policy = self.host / "sys" / name
                policy.parent.mkdir(parents=True)
                policy.write_text(value)
                before = self.snapshot()
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("signed" if "lockdown" in name else "signatures", result.stderr)
                self.assertEqual(self.snapshot(), before)
                policy.unlink()
        self.kernel_config().write_text("CONFIG_MODULE_SIG_FORCE=y\n")
        before = self.snapshot()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires trusted signatures", result.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_corrupt_record_does_not_delete_files(self):
        self.assert_success(self.install())
        manifest = self.host / "usr/libexec/carryfs/native-kernels"
        for value in ("../../outside\n", "7.2 space\n", KERNEL + "\n\n"):
            with self.subTest(value=value):
                manifest.write_text(value)
                before = self.snapshot()
                for result in (self.install(expected="0.1.0"), self.uninstall()):
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("Invalid kernel release", result.stderr)
                    self.assertEqual(self.snapshot(), before)

    def test_native_flag_requires_local(self):
        for args in (("--native",), ("--uninstall", "--native")):
            result = subprocess.run(["sh", str(ROOT / "install.sh"), *args], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("--native requires --local", result.stderr)

    def test_native_flag_rejects_macos_before_building(self):
        self.tool("uname", "echo Darwin")
        result = subprocess.run(["sh", str(ROOT / "install.sh"), "--local", "--native"],
                                env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--native is supported on Linux only", result.stderr)

    def test_local_build_stages_native_module_and_propagates_failure(self):
        checkout = self.work / "checkout"
        for directory in ("scripts", "linux", "packaging", "fuse/packaging"):
            (checkout / directory).mkdir(parents=True, exist_ok=True)
        (checkout / "Cargo.toml").write_text('version = "0.1.0"\n')
        (checkout / "packaging/install-unix.sh").write_text("package installer")
        for name in ("61-carryfs.rules", "mount_options.conf"):
            (checkout / "fuse/packaging" / name).write_bytes((self.payload / name).read_bytes())
        (checkout / "scripts/kernel-build.sh").write_text('''#!/bin/bash
set -eu
printf '%s\\n' "$1" > "$BUILD_ARGUMENT"
[ "${BUILD_FAIL:-0}" = 0 ]
printf 'built module' > linux/carryfs.ko
''')
        self.tool("cargo", "mkdir -p target/release; echo cli > target/release/carryfs; echo fuse > target/release/carryfs-fuse")
        for native, failure in ((1, "0"), (1, "1"), (0, "1")):
            with self.subTest(native=native, failure=failure):
                stage = self.work / f"stage-{native}-{failure}"
                stage.mkdir()
                script = self.functions + '\ncheckout=$1 work=$2 native=$3 platform=linux-x86_64\nbuild_local\n'
                result = subprocess.run(["sh", "-s", "--", str(checkout), str(stage), str(native)], input=script,
                                        env=dict(self.env, BUILD_FAIL=failure, BUILD_ARGUMENT=str(self.work / "build-arg")),
                                        capture_output=True, text=True)
                if native and failure == "1":
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("Local build failed", result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertTrue((stage / "payload/carryfs").is_file())
                    self.assertEqual((stage / "payload/carryfs.ko").exists(), bool(native))
                    if native:
                        self.assertEqual((stage / "payload/KERNEL_RELEASE").read_text(), KERNEL + "\n")
                self.assertEqual((self.work / "build-arg").read_text().strip(),
                                 str(self.host / f"lib/modules/{KERNEL}/build"))


if __name__ == "__main__":
    unittest.main()
