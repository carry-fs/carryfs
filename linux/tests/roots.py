#!/usr/bin/env python3
"""Recover both root-write stages after simulated sector-sized torn writes."""

import os
from pathlib import Path
import subprocess
import tempfile

from check import Volume, run
from faults import Device


def snapshot(device, source, destination):
    run("dmsetup", "suspend", "--noflush", device.name)
    try:
        run("cp", "--sparse=always", source, destination)
    finally:
        run("dmsetup", "resume", device.name)


def roots(image):
    blocks = image.stat().st_size // 4096
    slots = (256, blocks - 257)
    with image.open("rb") as source:
        pages = []
        for slot in slots:
            source.seek(slot * 4096)
            pages.append(source.read(4096))
    return slots, pages


def main():
    cli = Path(__file__).resolve().parents[2] / "target/release/carryfs"
    run("modprobe", "dm_mod")
    with tempfile.TemporaryDirectory(prefix="carryfs-roots-") as directory:
        work = Path(directory)
        volume = Volume(cli, work, "source", 256)
        device = Device(volume.image, f"carryfs-roots-{os.getpid()}")
        mounted = False
        try:
            run("mount", "-i", "-t", "carryfs", device.path, volume.point)
            mounted = True
            path = volume.point / "file"
            old_data, new_data = b"old!" * 16384, b"new!" * 16384
            old_image, new_image = work / "old.img", work / "new.img"
            with path.open("wb") as output:
                output.write(old_data)
                output.flush()
                os.fsync(output.fileno())
            snapshot(device, volume.image, old_image)
            with path.open("r+b") as output:
                output.write(new_data)
                output.flush()
                os.fsync(output.fileno())
            snapshot(device, volume.image, new_image)
            run("umount", volume.point)
            mounted = False
        finally:
            if mounted:
                run("umount", volume.point)
            device.close()
        slots, old_roots = roots(old_image)
        _, new_roots = roots(new_image)
        count = 0
        for slot in range(2):
            for length in (*range(512, 4096, 512), -512):
                image = work / "torn.img"
                run("cp", "--sparse=always", new_image, image)
                if length > 0:
                    torn = new_roots[slot][:length] + old_roots[slot][length:]
                else:
                    torn = old_roots[slot][:length] + new_roots[slot][length:]
                with image.open("r+b") as output:
                    if slot == 0:
                        # Root A is interrupted before root B is issued.
                        output.seek(slots[1] * 4096)
                        output.write(old_roots[1])
                    output.seek(slots[slot] * 4096)
                    output.write(torn)
                checked = subprocess.run([cli, "check", image], capture_output=True, text=True)
                assert checked.returncode == 0, (slot, length, checked.stdout, checked.stderr)
                copy = Volume.__new__(Volume)
                copy.cli, copy.image, copy.point, copy.loop = cli, image, volume.point, None
                root = copy.mount("ro")
                try:
                    expected = old_data if slot == 0 else new_data
                    assert (root / "file").read_bytes() == expected, (slot, length)
                finally:
                    copy.unmount()
                count += 1
        print(f"root tears: {count} interrupted A/B writes recovered the expected file bytes", flush=True)


if __name__ == "__main__":
    main()
