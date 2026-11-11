#!/usr/bin/env python3
"""Compare vectored and contiguous GEOM writes on a disposable md device."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    options = parser.parse_args()
    if platform.system() != "FreeBSD" or os.geteuid() != 0:
        parser.error("run as root on FreeBSD")
    options.out.mkdir(parents=True, exist_ok=False)
    (options.out / "environment.json").write_text(json.dumps({
        "command": sys.argv,
        "uname": list(platform.uname()),
        "python": sys.version,
        "cpu": subprocess.check_output(["sysctl", "hw.model", "hw.ncpu"], text=True),
    }, indent=2) + "\n")
    image = (options.out / "disk.img").resolve()
    with image.open("wb") as file:
        file.truncate(512 << 20)
    device = subprocess.check_output(
        ["mdconfig", "-a", "-t", "vnode", "-f", str(image)], text=True
    ).strip()
    results = {}
    try:
        descriptor = os.open("/dev/" + device, os.O_RDWR)
        try:
            for pieces in (1, 16, 256):
                vector = [bytes(range(256)) * 16] * pieces
                contiguous = b"".join(vector)
                size = len(contiguous)
                for offset in range(0, 4 << 20, size):
                    if os.pwrite(descriptor, contiguous, offset) != size:
                        raise RuntimeError("short warmup write")
                fcntl.ioctl(descriptor, 0x20006487)
                samples = {"vectored": [], "contiguous": []}
                for sample in range(7):
                    order = ("vectored", "contiguous")
                    for method in (order if sample % 2 == 0 else order[::-1]):
                        started = time.perf_counter()
                        for offset in range(0, 4 << 20, size):
                            if method == "vectored":
                                written = os.pwritev(descriptor, vector, offset)
                            else:
                                written = os.pwrite(descriptor, contiguous, offset)
                            if written != size:
                                raise RuntimeError("short raw write")
                        elapsed = time.perf_counter() - started
                        # FreeBSD DIOCGFLUSH, also used by the production adapter.
                        fcntl.ioctl(descriptor, 0x20006487)
                        samples[method].append(4 / elapsed)
                        if os.pread(descriptor, size, 0) != contiguous:
                            raise RuntimeError("raw write changed bytes")
                results[size] = samples
        finally:
            os.close(descriptor)
    finally:
        subprocess.run(["mdconfig", "-d", "-u", device], check=True)
    (options.out / "MiB-per-second.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
