#!/usr/bin/env python3
"""Check durable prefixes of orphan deletion with the fstests log replay tool."""

from concurrent.futures import ThreadPoolExecutor
import os
import re
from pathlib import Path
import shlex
import struct
import subprocess
import sys
import tempfile
import threading
import time

from check import Volume, run
from faults import Device


def check_prefix(cli, baseline, checkpoint, generation, initial_blocks, evidence):
    from fuzz import crc32c

    run("cp", "--sparse=always", baseline, checkpoint)
    result = subprocess.check_output([cli, "check", checkpoint], text=True)
    print(result, end="", flush=True)
    generations = []
    with open(checkpoint, "rb") as image:
        blocks = os.fstat(image.fileno()).st_size // 4096
        for block in (256, blocks - 257):
            image.seek(block * 4096)
            page = bytearray(image.read(4096))
            expected = struct.unpack_from("<I", page, 4092)[0]
            page[4092:4096] = bytes(4)
            if crc32c(page) == expected:
                generations.append(struct.unpack_from("<Q", page, 48)[0])
    blocks = int(re.search(r", (\d+) data blocks,", result)[1])
    if (max(generations) > int(generation) and re.search(r"passed: 2 objects,", result)
            and 0 < blocks < int(initial_blocks)):
        if not Path(evidence).exists():
            run("cp", "--sparse=always", checkpoint, Path(evidence).with_suffix(".img"))
        with open(evidence, "a") as output:
            output.write(f"{max(generations)}\n")


def recover(cli, source, work, adapter):
    volume = Volume.__new__(Volume)
    volume.cli, volume.image = cli, work / f"recovery-{adapter}.img"
    volume.point, volume.loop = work / f"mnt-{adapter}", None
    volume.point.mkdir()
    run("cp", "--sparse=always", source, volume.image)
    daemon = None
    try:
        if adapter == "native":
            volume.mount()
        else:
            run("modprobe", "fuse")
            volume.loop = subprocess.check_output(["losetup", "-f", "--show", volume.image], text=True).strip()
            daemon = subprocess.Popen([cli.with_name("carryfs-fuse"), "serve", volume.loop, volume.point, "rw"])
            deadline = time.monotonic() + 30
            while not os.path.ismount(volume.point):
                assert daemon.poll() is None, "FUSE recovery mount exited"
                assert time.monotonic() < deadline, "FUSE recovery mount timed out"
                time.sleep(0.02)
        assert list(volume.point.iterdir()) == [], "recovery exposed an unlinked file"
        payload = b"recovered" * 1024
        with (volume.point / "after-recovery").open("wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        if adapter == "fuse":
            # FUSE recovers unpinned orphans through its background maintenance.
            free = volume.image.stat().st_size // 4096 - 1024 - 64
            deadline = time.monotonic() + 120
            while os.statvfs(volume.point).f_bfree < free:
                assert time.monotonic() < deadline, "FUSE did not reclaim the interrupted orphan"
                time.sleep(0.1)
    finally:
        if os.path.ismount(volume.point):
            volume.unmount()
        elif volume.loop is not None:
            run("losetup", "-d", volume.loop)
        if daemon is not None:
            try:
                status = daemon.wait(timeout=30)
            except subprocess.TimeoutExpired:
                daemon.terminate()
                daemon.wait(timeout=10)
                raise
            assert status == 0, "FUSE recovery daemon failed"
    checked = subprocess.check_output([cli, "check", volume.image], text=True)
    assert re.search(r"passed: 2 objects,", checked), "recovery retained the orphan inode"
    root = volume.mount("ro")
    try:
        assert (root / "after-recovery").read_bytes() == payload
    finally:
        volume.unmount()
    print(f"  {adapter} recovered the interrupted deletion and preserved a new durable file; {volume.check(scrub=True)}", flush=True)


def main():
    if len(sys.argv) == 8 and sys.argv[1] == "--check-prefix":
        check_prefix(*sys.argv[2:])
        return
    if len(sys.argv) != 2:
        sys.exit("usage: replay.py PATH_TO_FSTESTS_REPLAY_LOG")
    replay = Path(sys.argv[1]).resolve()
    cli = Path(__file__).resolve().parents[2] / "target/release/carryfs"
    run("modprobe", "dm_log_writes")
    with tempfile.TemporaryDirectory(prefix="carryfs-replay-") as directory:
        work = Path(directory)
        volume = Volume(cli, work, "deletion", 512)
        device = Device(volume.image, f"carryfs-replay-{os.getpid()}")
        log_image = work / "log.img"
        with log_image.open("wb") as image:
            image.truncate(512 << 20)
        log_loop = subprocess.check_output(["losetup", "-f", "--show", log_image], text=True).strip()
        mounted = False
        held = None
        directory_fd = None
        try:
            run("mount", "-i", "-t", "carryfs", device.path, volume.point)
            mounted = True
            directory_fd = os.open(volume.point, os.O_RDONLY | os.O_DIRECTORY)
            path = volume.point / "fragmented"
            held = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
            for index in range(6000):
                os.pwrite(held, bytes([index % 256]) * 4096, index * 2 * 65536)
            os.fsync(held)
            path.unlink()
            os.fsync(directory_fd)
            # Stop block I/O while taking the baseline; never inspect a
            # changing mounted image as though it were a coherent snapshot.
            run("dmsetup", "suspend", "--noflush", device.name)
            baseline = work / "baseline.img"
            try:
                run("cp", "--sparse=always", volume.image, baseline)
                run("dmsetup", "reload", device.name, "--table",
                    f"0 {device.sectors} log-writes {device.loop} {log_loop}")
            finally:
                run("dmsetup", "resume", device.name)
            checked = subprocess.check_output([cli, "check", baseline], text=True)
            print(checked, end="", flush=True)
            initial_blocks = re.search(r", (\d+) data blocks,", checked)[1]
            with baseline.open("rb") as image:
                image.seek(256 * 4096 + 48)
                generation = struct.unpack("<Q", image.read(8))[0]
            stopped = threading.Event()
            started = threading.Event()

            def publish():
                os.fsync(directory_fd)
                started.set()
                while not stopped.wait(0.001):
                    os.fsync(directory_fd)

            with ThreadPoolExecutor(max_workers=1) as pool:
                publishing = pool.submit(publish)
                try:
                    assert started.wait(30), "concurrent publisher did not start"
                    os.close(held)
                    held = None
                finally:
                    stopped.set()
                publishing.result(timeout=120)
            os.fsync(directory_fd)
            os.close(directory_fd)
            directory_fd = None
            run("umount", volume.point)
            mounted = False
        finally:
            if held is not None:
                os.close(held)
            if directory_fd is not None:
                os.close(directory_fd)
            if mounted:
                run("umount", volume.point)
            device.close()
            run("losetup", "-d", log_loop)
        with volume.image.open("rb") as image:
            image.seek(256 * 4096 + 48)
            final = struct.unpack("<Q", image.read(8))[0]
        assert final >= generation + 2, "deletion did not cross multiple publications"
        # The checker refuses images held open by another writer. Replay is
        # paused while this command copies and checks its immutable prefix.
        checkpoint = work / "checkpoint.img"
        evidence = work / "intermediate-generations"
        command = shlex.join([sys.executable, str(Path(__file__).resolve()), "--check-prefix",
                             str(cli), str(baseline), str(checkpoint), str(generation), initial_blocks, str(evidence)])
        run(replay, "--log", log_image, "--replay", baseline,
            "--fsck", command, "--check", "flush", "--no-discard")
        run(cli, "check", baseline)
        assert evidence.exists(), "replay did not check a committed intermediate orphan state"
        for adapter in ("native", "fuse"):
            recover(cli, evidence.with_suffix(".img"), work, adapter)
        print(f"orphan deletion: all flush checkpoints passed across {final - generation} publications, including committed intermediate orphan states", flush=True)


if __name__ == "__main__":
    main()
