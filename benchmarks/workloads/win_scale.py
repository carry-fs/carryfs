"""Scale workload for a mounted Windows volume.

    python win_scale.py ROOT LABEL populate [--files N] [--large N] [--frag N] [--depth N]
    python win_scale.py ROOT LABEL age [--rounds N] [--fill-fraction F] [--reclaim-timeout S]
    python win_scale.py ROOT LABEL sync

populate: N small files (1-8 KiB, 1,000 per directory), a directory chain of
the given depth, large files of 2 GiB, and fragmented files grown by
round-robin 64 KiB appends. age: near-full rounds of 256 MiB files, delete
every other one, refill, and time how long freed space takes to show up.
sync: FlushFileBuffers on the volume handle. Output is TSV like bench.py:
label, phase, metric, value, unit.
"""
import argparse
import ctypes
import os
import random
import sys
import time
from ctypes import wintypes

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
kernel32.CreateFileW.restype = wintypes.HANDLE
kernel32.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
                                 wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
INVALID = wintypes.HANDLE(-1).value
MIB = 1 << 20
POOL = random.Random(0).randbytes(8 * MIB)


def out(label, phase, metric, value, unit):
    print(f"{label}\t{phase}\t{metric}\t{value:.3f}\t{unit}", flush=True)


def flush(path):
    handle = kernel32.CreateFileW(path, 0xC0000000, 7, None, 3, 0x02000000, None)
    if handle == INVALID:
        raise ctypes.WinError(ctypes.get_last_error(), f"open for flush: {path}")
    try:
        if not kernel32.FlushFileBuffers(handle):
            raise ctypes.WinError(ctypes.get_last_error(), f"FlushFileBuffers: {path}")
    finally:
        kernel32.CloseHandle(handle)


def free_bytes(root):
    free = ctypes.c_ulonglong()
    if not kernel32.GetDiskFreeSpaceExW(root, None, None, ctypes.byref(free)):
        raise ctypes.WinError(ctypes.get_last_error())
    return free.value


def data(offset, size):
    start = offset % (len(POOL) - size)
    return POOL[start:start + size]


def write_file(path, size, seed):
    with open(path, "wb", buffering=0) as f:
        done = 0
        while done < size:
            n = min(8 * MIB - 4096, size - done)
            f.write(data(seed * 7919 + done, n))
            done += n


def populate(root, label, args):
    t = time.perf_counter()
    for i in range(args.files):
        d = os.path.join(root, "small", f"d{i // 1000:04}")
        if i % 1000 == 0:
            os.makedirs(d)
        with open(os.path.join(d, f"f{i % 1000:03}.bin"), "wb", buffering=0) as f:
            f.write(data(i * 131, 1024 * (1 + i % 8)))
        if (i + 1) % 100_000 == 0:
            elapsed = time.perf_counter() - t
            out(label, "populate", f"small_files_{i + 1}", (i + 1) / elapsed, "files/s")
    out(label, "populate", "small_files_total_time", time.perf_counter() - t, "s")

    t = time.perf_counter()
    # Past MAX_PATH; WinFsp's own limit is 1,024 characters.
    path = os.path.join("\\\\?\\" + root, "deep")
    os.makedirs(path)
    for level in range(args.depth):
        path = os.path.join(path, f"{level:03}")
        os.mkdir(path)
    with open(os.path.join(path, "leaf.bin"), "wb") as f:
        f.write(b"leaf")
    out(label, "populate", f"deep_chain_{args.depth}", time.perf_counter() - t, "s")

    os.makedirs(os.path.join(root, "large"))
    for i in range(args.large):
        t = time.perf_counter()
        write_file(os.path.join(root, "large", f"archive{i}.bin"), 2 << 30, i)
        out(label, "populate", f"large_{i}_write", 2048 / (time.perf_counter() - t), "MiB/s")

    t = time.perf_counter()
    os.makedirs(os.path.join(root, "frag"))
    handles = [open(os.path.join(root, "frag", f"g{i:03}.bin"), "ab", buffering=0) for i in range(args.frag)]
    for step in range(64):  # 64 x 64 KiB = 4 MiB each, interleaved
        for i, f in enumerate(handles):
            f.write(data(i * 64 + step, 64 << 10))
    for f in handles:
        f.close()
    out(label, "populate", f"fragmented_{args.frag}x4MiB", time.perf_counter() - t, "s")

    t = time.perf_counter()
    flush(volume_path(root))
    out(label, "populate", "volume_flush", (time.perf_counter() - t) * 1000, "ms")


def wait_for_free(root, target, timeout=120):
    t = time.perf_counter()
    while free_bytes(root) < target:
        if time.perf_counter() - t > timeout:
            return None
        time.sleep(0.05)
    return time.perf_counter() - t


def age(root, label, args):
    base = os.path.join(root, "age")
    os.makedirs(base, exist_ok=True)
    size = 256 * MIB
    for rnd in range(args.rounds):
        # Fill: new 256 MiB files until the given fraction of the volume is used.
        total = ctypes.c_ulonglong()
        kernel32.GetDiskFreeSpaceExW(root, None, ctypes.byref(total), None)
        t = time.perf_counter()
        written = 0
        names = sorted(os.listdir(base))
        index = int(names[-1][1:6]) + 1 if names else 0
        while free_bytes(root) - size > total.value * (1 - args.fill_fraction):
            try:
                write_file(os.path.join(base, f"a{index:05}.bin"), size, index)
            except OSError as error:
                out(label, f"age{rnd}", "fill_stopped_by_error", written / MIB, "MiB")
                print(f"# {error}", file=sys.stderr)
                break
            index += 1
            written += size
        elapsed = time.perf_counter() - t
        out(label, f"age{rnd}", "fill_written", written / MIB, "MiB")
        out(label, f"age{rnd}", "fill_rate", written / MIB / elapsed, "MiB/s")
        # Delete every other file, then time until the space is reported free.
        names = sorted(os.listdir(base))
        victims = names[rnd % 2::2]
        before = free_bytes(root)
        t = time.perf_counter()
        for name in victims:
            os.remove(os.path.join(base, name))
        deleted = time.perf_counter() - t
        freed = len(victims) * size
        out(label, f"age{rnd}", "delete_files", len(victims), "files")
        out(label, f"age{rnd}", "delete_time", deleted, "s")
        waited = wait_for_free(root, before + freed * 0.99, args.reclaim_timeout)
        out(label, f"age{rnd}", "reclaim_after_delete", -1 if waited is None else waited, "s")
        out(label, f"age{rnd}", "used_fraction", 1 - free_bytes(root) / total.value, "ratio")


def volume_path(root):
    return "\\\\.\\" + root.rstrip("\\")[:2]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root")
    parser.add_argument("label")
    parser.add_argument("phase", choices=["populate", "age", "sync"])
    parser.add_argument("--files", type=int, default=1_000_000)
    parser.add_argument("--large", type=int, default=4)
    parser.add_argument("--frag", type=int, default=200)
    parser.add_argument("--depth", type=int, default=200)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--fill-fraction", type=float, default=0.95)
    parser.add_argument("--reclaim-timeout", type=float, default=120, help="seconds to wait for deleted space to be reported free")
    args = parser.parse_args()
    root = args.root.rstrip("\\") + "\\"
    if args.phase == "populate":
        populate(root, args.label, args)
    elif args.phase == "age":
        age(root, args.label, args)
    else:
        t = time.perf_counter()
        flush(volume_path(root))
        out(args.label, "sync", "volume_flush", (time.perf_counter() - t) * 1000, "ms")


if __name__ == "__main__":
    main()
