#!/usr/bin/env python3
"""Fail each publication flush through QEMU's blkdebug backend."""

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile

from check import run
from faults import expect_eio

REPO = Path(__file__).resolve().parents[2]


def guest():
    run("insmod", REPO / "linux/carryfs.ko")
    with tempfile.TemporaryDirectory(prefix="carryfs-flush-mount-") as directory:
        point = Path(directory)
        for index in range(1, 4):
            device = Path(f"/dev/disk/by-id/virtio-carryfs-flush-{index}")
            assert device.exists(), device
            run("mount", "-i", "-t", "carryfs", device, point)
            try:
                (point / "pending").touch()
                fd = os.open(point, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    for _ in range(4):
                        expect_eio(fd)
                finally:
                    os.close(fd)
            finally:
                run("umount", point)
            cli = REPO / "target/release/carryfs"
            run(cli, "check", "--bytes", 256 << 20, device)
            run("mount", "-i", "-t", "carryfs", device, point)
            try:
                (point / "recovered").touch()
                fd = os.open(point, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(fd)
                finally:
                    os.close(fd)
            finally:
                run("umount", point)
            print(f"publication flush {index}: fsync failed persistently; remount recovered", flush=True)
    diagnostics = subprocess.check_output(["dmesg"], text=True)
    assert not any(label in diagnostics for label in ("BUG:", "Oops:", "WARNING:", "panicked", "Call Trace:")), diagnostics


def main():
    if sys.argv[1:] == ["--guest"]:
        guest()
        return
    os.chdir(REPO)
    output = REPO / "build/check-kernel"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="flush-", dir=output) as directory:
        args = ["virtme-run", "--kimg", os.environ.get("KERNEL_IMAGE", f"/usr/lib/modules/{os.uname().release}/vmlinuz"),
                "--mods", "auto", "--memory", "2G", "--cpus", "2", "--pwd", "--disable-microvm",
                "--kopt", "panic=-1", "--script-sh", shlex.join(["python3", str(Path(__file__).resolve()), "--guest"])]
        if os.environ.get("CARRYFS_VM_TCG") == "1":
            args.append("--disable-kvm")
        args.append("--qemu-opts")
        for index in range(1, 4):
            image = Path(directory) / f"flush-{index}.img"
            run(REPO / "target/release/carryfs", "mkfs", image, 256 << 20, stdout=subprocess.DEVNULL)
            config = Path(directory) / f"flush-{index}.conf"
            rules = []
            for state in range(1, index):
                rules.append(f'[set-state]\nevent = "flush_to_disk"\nstate = "{state}"\nnew_state = "{state + 1}"\n')
            rules.append(f'[inject-error]\nevent = "flush_to_disk"\nstate = "{index}"\nerrno = "5"\nonce = "on"\n')
            config.write_text("\n".join(rules))
            node = f"carryfs-flush-{index}"
            backend = {"driver": "raw", "node-name": node,
                       "file": {"driver": "blkdebug", "config": str(config),
                                "image": {"driver": "file", "filename": str(image)}}}
            args += ["-blockdev", json.dumps(backend), "-device", f"virtio-blk-pci,drive={node},serial={node}"]
        with (output / "flush-failures.log").open("w") as log:
            result = subprocess.run(args, stdout=log, stderr=subprocess.STDOUT, timeout=180)
        assert result.returncode == 0, "flush-failure VM failed; see build/check-kernel/flush-failures.log"
    print("all three publication flush failures passed")


if __name__ == "__main__":
    main()
