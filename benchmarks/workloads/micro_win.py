"""One workload per run, so each gets its own mount and adapter statistics.
    python micro_win.py ROOT create|stat|list|delete|tree|seqwrite|seqread [COUNT]
Prints: workload, count, wall seconds, rate. Run in the listed order on one
volume; each needs what the previous ones left.
"""
import os
import sys
import time

root, workload = sys.argv[1], sys.argv[2]
count = int(sys.argv[3]) if len(sys.argv) > 3 else 1000
files = os.path.join(root, "files")
tree = os.path.join(root, "tree")
payload = b"x" * 4096
MIB = 1 << 20
SEQ = 512

start = time.perf_counter()
if workload == "create":
    os.makedirs(files, exist_ok=True)
    for i in range(count):
        with open(os.path.join(files, f"f{i:05d}.dat"), "wb") as handle:
            handle.write(payload)
elif workload == "stat":
    for i in range(count):
        os.stat(os.path.join(files, f"f{i:05d}.dat"))
elif workload == "list":
    entries = [(entry.name, entry.stat().st_size) for entry in os.scandir(files)]
    assert len(entries) == count, len(entries)
elif workload == "delete":
    for i in range(count):
        os.remove(os.path.join(files, f"f{i:05d}.dat"))
elif workload == "tree":
    for d in range(20):
        directory = os.path.join(tree, f"d{d:02d}")
        os.makedirs(directory, exist_ok=True)
        for i in range(count // 20):
            with open(os.path.join(directory, f"f{i:04d}.dat"), "wb") as handle:
                handle.write(payload)
elif workload == "seqwrite":
    count = SEQ
    block = os.urandom(MIB)
    with open(os.path.join(root, "seq.bin"), "wb") as handle:
        for _ in range(SEQ):
            handle.write(block)
        handle.flush()
        os.fsync(handle.fileno())
elif workload == "seqread":
    count = SEQ
    with open(os.path.join(root, "seq.bin"), "rb", buffering=0) as handle:
        while handle.read(MIB):
            pass
else:
    sys.exit(__doc__)
elapsed = time.perf_counter() - start
print(f"{workload}\t{count}\t{elapsed:.3f}\t{count / elapsed:.1f}")
