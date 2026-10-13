"""Mixed-size backup workload for a mounted filesystem.

  python3 benchmarks/workloads/mixed.py ROOT LABEL --phase write|verify|update|walk|delete [--scale S]

A deterministic corpus of files from 1 KiB to 2 GiB in a nested tree, about
10.5 GiB and 5,139 files at scale 1. Content is generated per 1 MiB chunk from (file, chunk,
version), so `verify` recomputes every expected byte without a stored copy.
Update state lives in build/mixed-state-LABEL.json, so run the phases in
order: write, verify, update, verify, walk, delete. Output is bench.py's TSV:
label, phase, metric, value, unit. Durability uses F_FULLFSYNC where the
platform has it (macOS, where plain fsync leaves the drive cache unflushed)
and fsync elsewhere; `fullfsync_fallbacks` counts the fsync calls.
"""
import argparse
import fcntl
import hashlib
import json
import os
import random
import time
from pathlib import Path

STATE_DIR = Path(__file__).resolve().parents[2] / "build"
CHUNK = 1 << 20
MIB = 1 << 20
# (count, smallest bytes, largest bytes); sizes are log-uniform in each bucket.
BUCKETS = [
    (3000, 1 << 10, 16 << 10),
    (1500, 16 << 10, 256 << 10),
    (500, 256 << 10, 4 << 20),
    (120, 4 << 20, 64 << 20),
    (16, 64 << 20, 512 << 20),
    (3, 1 << 30, 2 << 30),
]


def corpus(scale):
    """[(relative path, size)] in a fixed order."""
    rng = random.Random(33)
    files = []
    for count, low, high in BUCKETS:
        for _ in range(max(1, round(count * scale))):
            size = int(low * (high / low) ** rng.random())
            index = len(files)
            files.append((f"d{index % 20:02}/s{index // 20 % 10}/f{index:05}.bin", size))
    return files


_pool = random.Random(0)
POOL = b"".join(_pool.randbytes(CHUNK) for _ in range(256))


def chunk(path, index, version):
    # A slice of a fixed random pool at a per-(file, chunk, version) offset:
    # incompressible, distinct per chunk, and faster than any disk under test.
    seed = int.from_bytes(hashlib.sha256(f"{path}:{index}:{version}".encode()).digest()[:8], "little")
    offset = seed % (len(POOL) - CHUNK)
    return POOL[offset:offset + CHUNK]


def content(path, size, versions):
    """Yield the expected file bytes chunk by chunk."""
    for index in range(-(-size // CHUNK)):
        data = chunk(path, index, versions.get(str(index), 0))
        yield data[: min(CHUNK, size - index * CHUNK)]


def emit(label, phase, metric, value, unit):
    print(f"{label}\t{phase}\t{metric}\t{value:.3f}\t{unit}", flush=True)


FULL = {"used": 0, "fallback": 0}


def durable(descriptor):
    # macOS fsync() does not flush the drive cache; F_FULLFSYNC does. Fall back
    # to fsync where a filesystem rejects it, and report how often that happened.
    try:
        fcntl.fcntl(descriptor, fcntl.F_FULLFSYNC)
        FULL["used"] += 1
    except (AttributeError, OSError):
        os.fsync(descriptor)
        FULL["fallback"] += 1


def sync_tree(root, paths):
    for path in paths:
        descriptor = os.open(path, os.O_RDONLY)
        try:
            durable(descriptor)
        finally:
            os.close(descriptor)


def write_file(target, relative, size, versions):
    target.parent.mkdir(parents=True, exist_ok=True)
    with open(target, "wb") as file:
        for data in content(relative, size, versions):
            file.write(data)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    parser.add_argument("label")
    parser.add_argument("--phase", required=True,
                        choices=("write", "verify", "update", "walk", "delete"))
    parser.add_argument("--scale", type=float, default=1.0)
    options = parser.parse_args(argv)
    root, label = options.root / "mixed", options.label
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    state_path = STATE_DIR / f"mixed-state-{label}.json"

    if options.phase == "write":
        files = corpus(options.scale)
        state = {"files": {path: {"size": size, "versions": {}} for path, size in files}}
        started = time.perf_counter()
        for path, size in files:
            write_file(root / path, path, size, {})
        elapsed = time.perf_counter() - started
        started = time.perf_counter()
        sync_tree(root, [root / path for path, _ in files])
        sync = time.perf_counter() - started
        total = sum(size for _, size in files)
        emit(label, "write", "mixed_files", len(files), "files")
        emit(label, "write", "mixed_bytes", total / MIB, "MiB")
        emit(label, "write", "mixed_write", total / MIB / elapsed, "MiB/s")
        emit(label, "write", "mixed_fsync_all", sync, "s")
        emit(label, "write", "mixed_write_durable", total / MIB / (elapsed + sync), "MiB/s")
        emit(label, "write", "mixed_files_durable", len(files) / (elapsed + sync), "files/s")
        emit(label, "write", "fullfsync_fallbacks", FULL["fallback"], "calls")
        state_path.write_text(json.dumps(state))
        return

    state = json.loads(state_path.read_text())
    files = state["files"]

    if options.phase == "verify":
        started = time.perf_counter()
        total = wrong = 0
        for path, entry in files.items():
            with open(root / path, "rb") as file:
                for expected in content(path, entry["size"], entry["versions"]):
                    if file.read(len(expected)) != expected:
                        wrong += 1
                        break
                else:
                    if file.read(1):
                        wrong += 1
            total += entry["size"]
        elapsed = time.perf_counter() - started
        phase = f"verify{state.get('updates', 0)}"
        emit(label, phase, "mixed_read", total / MIB / elapsed, "MiB/s")
        emit(label, phase, "mixed_wrong_files", wrong, "files")

    elif options.phase == "update":
        # An incremental backup: rewrite one chunk in 10% of files, append a
        # chunk to 5%, delete 10%, add 10% new ones, then make it durable.
        rng = random.Random(34)
        paths = sorted(files)
        touched = set()
        started = time.perf_counter()
        for path in rng.sample(paths, len(paths) // 10):
            entry = files[path]
            index = rng.randrange(-(-entry["size"] // CHUNK))
            version = entry["versions"].get(str(index), 0) + 1
            entry["versions"][str(index)] = version
            data = chunk(path, index, version)[: min(CHUNK, entry["size"] - index * CHUNK)]
            with open(root / path, "r+b") as file:
                file.seek(index * CHUNK)
                file.write(data)
            touched.add(path)
        for path in rng.sample(paths, len(paths) // 20):
            entry = files[path]
            if entry["size"] % CHUNK:
                continue  # appending past a partial chunk would change its content
            with open(root / path, "ab") as file:
                file.write(chunk(path, entry["size"] // CHUNK, 0))
            entry["size"] += CHUNK
            touched.add(path)
        for path in rng.sample(paths, len(paths) // 10):
            if path in touched:
                continue
            (root / path).unlink()
            del files[path]
        for number in range(len(paths) // 10):
            path = f"new/n{number // 100:02}/g{number:05}.bin"
            size = int((1 << 10) * (64 << 10) ** rng.random())
            write_file(root / path, path, size, {})
            files[path] = {"size": size, "versions": {}}
            touched.add(path)
        sync_tree(root, [root / path for path in touched])
        elapsed = time.perf_counter() - started
        state["updates"] = state.get("updates", 0) + 1
        emit(label, "update", "mixed_update", elapsed, "s")
        state_path.write_text(json.dumps(state))

    elif options.phase == "walk":
        started = time.perf_counter()
        entries = 0
        for directory, names, filenames in os.walk(root):
            for name in names + filenames:
                os.stat(os.path.join(directory, name))
                entries += 1
        elapsed = time.perf_counter() - started
        emit(label, "walk", "mixed_walk_stat", entries / elapsed, "entries/s")

    elif options.phase == "delete":
        started = time.perf_counter()
        for directory, names, filenames in os.walk(root, topdown=False):
            for name in filenames:
                os.unlink(os.path.join(directory, name))
            for name in names:
                os.rmdir(os.path.join(directory, name))
        os.rmdir(root)
        descriptor = os.open(options.root, os.O_RDONLY)
        durable(descriptor)
        os.close(descriptor)
        emit(label, "delete", "mixed_delete", time.perf_counter() - started, "s")
        state_path.unlink()


if __name__ == "__main__":
    main()
