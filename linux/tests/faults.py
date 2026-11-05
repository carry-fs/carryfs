#!/usr/bin/env python3
"""Inject device errors inside the disposable VM, never on a host device."""

import errno
import os
from pathlib import Path
import subprocess
import struct
import tempfile

from check import Volume, run


class Device:
    def __init__(self, image, name):
        self.name = name
        self.sectors = image.stat().st_size // 512
        self.loop = subprocess.check_output(["losetup", "-f", "--show", image], text=True).strip()
        self.path = Path("/dev/mapper") / name
        try:
            run("dmsetup", "create", name, "--table", self.linear())
        except BaseException:
            run("losetup", "-d", self.loop)
            raise

    def linear(self):
        return f"0 {self.sectors} linear {self.loop} 0"

    def replace(self, table):
        run("dmsetup", "suspend", "--noflush", self.name)
        try:
            run("dmsetup", "reload", self.name, "--table", table)
        finally:
            run("dmsetup", "resume", self.name)

    def fail(self, first, count):
        segments = []
        if first:
            segments.append(f"0 {first} linear {self.loop} 0")
        segments.append(f"{first} {count} error")
        end = first + count
        if end < self.sectors:
            segments.append(f"{end} {self.sectors - end} linear {self.loop} {end}")
        self.replace("\n".join(segments))

    def close(self):
        run("dmsetup", "remove", self.name)
        run("losetup", "-d", self.loop)


def expect_eio(fd):
    try:
        os.fsync(fd)
    except OSError as error:
        assert error.errno == errno.EIO, error
    else:
        raise AssertionError("fsync succeeded on a faulted volume")


def publication_error(cli, work, phase):
    volume = Volume(cli, work, phase, 256)
    with volume.image.open("rb") as image:
        image.seek(256 * 4096 + 48)
        generation = struct.unpack("<Q", image.read(8))[0]
    device = Device(volume.image, f"carryfs-fault-{os.getpid()}-{phase}")
    mounted = False
    try:
        run("mount", "-i", "-t", "carryfs", device.path, volume.point)
        mounted = True
        fd = os.open(volume.point, os.O_RDONLY | os.O_DIRECTORY)
        try:
            # Empty-file creation changes only metadata. The next directory
            # fsync must publish it, with no file-data error to mask the result.
            (volume.point / "pending").touch()
            if phase == "metadata":
                device.fail(0, device.sectors)
            elif phase == "root-a":
                device.fail(256 * 8, 8)
            else:
                device.fail(device.sectors - 257 * 8, 8)
            expect_eio(fd)
            device.replace(device.linear())
            for _ in range(3):
                expect_eio(fd)
        finally:
            os.close(fd)
        run("umount", volume.point)
        mounted = False
    finally:
        if mounted:
            device.replace(device.linear())
            run("umount", volume.point)
        device.close()
    # A failed publication may have reached root A. Either outcome must be
    # structurally valid and recoverable; failure does not promise rollback.
    volume.check()
    with volume.image.open("rb") as image:
        image.seek(256 * 4096 + 48)
        a = struct.unpack("<Q", image.read(8))[0]
        image.seek((device.sectors // 8 - 257) * 4096 + 48)
        b = struct.unpack("<Q", image.read(8))[0]
    assert b == generation, f"{phase}: root B unexpectedly advanced"
    assert a == generation + (phase == "root-b"), f"{phase}: failure did not reach the intended publication stage"
    root = volume.mount()
    assert (root / "pending").exists() == (phase == "root-b")
    (root / "after-remount").touch()
    volume.unmount()
    print(f"  {phase}: repeated fsync errors persisted until remount; {volume.check()}", flush=True)


def direct_append_error(cli, work):
    volume = Volume(cli, work, "direct-append-error", 256)
    device = Device(volume.image, f"carryfs-direct-error-{os.getpid()}")
    mounted = False
    try:
        run("mount", "-i", "-t", "carryfs", device.path, volume.point)
        mounted = True
        path = volume.point / "file"
        path.write_bytes(b"x" * 65536)
        fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_DIRECT)
        try:
            os.fsync(fd)
            device.fail(0, device.sectors)
            try:
                os.write(fd, b"y" * 65536)
            except OSError as error:
                assert error.errno == errno.EIO, error
            else:
                raise AssertionError("direct append hid a writeback error")
        finally:
            device.replace(device.linear())
            os.close(fd)
        run("umount", volume.point)
        mounted = False
    finally:
        if mounted:
            device.replace(device.linear())
            run("umount", volume.point)
        device.close()
    print(f"  direct append: writeback error reached write; {volume.check(scrub=True)}", flush=True)


def main():
    cli = Path(__file__).resolve().parents[2] / "target/release/carryfs"
    run("modprobe", "dm_mod")
    with tempfile.TemporaryDirectory(prefix="carryfs-faults-") as directory:
        for phase in ("metadata", "root-a", "root-b"):
            publication_error(cli, Path(directory), phase)
        direct_append_error(cli, Path(directory))


if __name__ == "__main__":
    main()
