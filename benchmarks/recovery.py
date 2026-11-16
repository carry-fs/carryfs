#!/usr/bin/env python3
"""Alternate release recovery binaries; never build or modify source images."""

import argparse
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import time


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").digest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("baseline-bench", "candidate-bench", "baseline-cli", "candidate-cli", "inputs", "out"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--cpu", type=int)
    args = parser.parse_args()
    if args.rounds < 5:
        parser.error("use at least five samples per side")
    if args.cpu is not None:
        os.sched_setaffinity(0, {args.cpu})
    args.out.mkdir(parents=True, exist_ok=False)
    cases = [("orphan", 1000), ("clean", 1000), ("missing", 1000), ("data", 1), ("sparse", 1)]
    with (args.out / "memory.tsv").open("w") as output:
        writer = None
        for case, files in cases:
            for sample in range(1, args.rounds + 1):
                for side in ("baseline", "candidate"):
                    binary = getattr(args, f"{side}_bench").resolve()
                    result = subprocess.run(
                        [binary, "--case", case, "--files", str(files), "--mib", "16", "--rounds", "1"],
                        text=True, capture_output=True, check=True,
                    )
                    for row in csv.DictReader(io.StringIO(result.stdout), delimiter="\t"):
                        row.update(side=side, sample=sample)
                        if writer is None:
                            writer = csv.DictWriter(output, fieldnames=list(row), delimiter="\t", lineterminator="\n")
                            writer.writeheader()
                        writer.writerow(row)
                        output.flush()
            print(f"memory {case}: passed", flush=True)
    with (args.out / "cli.tsv").open("w") as output:
        writer = csv.writer(output, delimiter="\t", lineterminator="\n")
        writer.writerow(["case", "side", "sample", "elapsed_ms", "exit_code"])
        for case, files in cases:
            source = (args.inputs / f"{case}-{files}.cfs").resolve()
            original = digest(source)
            reference = None
            for sample in range(0, args.rounds + 1):
                for side in ("baseline", "candidate"):
                    binary = getattr(args, f"{side}_cli").resolve()
                    command = [binary]
                    if binary.name not in ("fsck.carryfs", "fsck-carryfs", "fsck.carryfs.exe", "fsck-carryfs.exe"):
                        command.append("fsck")
                    destination = args.out / f"{case}-{side}-{sample}.cfs"
                    start = time.perf_counter()
                    result = subprocess.run(
                        command + ["--repair", "--output", destination, "--verify-data", source],
                        text=True, capture_output=True,
                    )
                    elapsed = (time.perf_counter() - start) * 1000
                    assert result.returncode == (4 if case == "missing" else 1), result.stderr
                    manifest = json.loads(Path(f"{destination}.recovery.json").read_text())
                    assert manifest["verification"] == "passed"
                    assert manifest["summary"]["files"] == files
                    assert manifest["state"] == ("partial" if case == "missing" else "complete")
                    del manifest["destination"]["uuid"]
                    if reference is None:
                        reference = manifest
                    assert manifest == reference, (case, side, "manifest mismatch")
                    if sample:
                        writer.writerow([case, side, sample, f"{elapsed:.6f}", result.returncode])
                        output.flush()
                    # Timed command has closed and durably verified the image.
                    destination.unlink()
                    Path(f"{destination}.recovery.txt").unlink()
                    Path(f"{destination}.recovery.json").unlink()
            assert digest(source) == original
            print(f"cli {case}: passed; matching manifests; source unchanged", flush=True)


if __name__ == "__main__":
    main()
