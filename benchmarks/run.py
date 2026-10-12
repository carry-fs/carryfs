#!/usr/bin/env python3
"""Build, run and compare the same filesystem benchmark on Linux, macOS and Windows."""

import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO))
from benchmarks import full  # noqa: E402


def positive(text):
    value = int(text)
    if value <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return value


def digest(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def command(args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def output_directory(path, prefix):
    if path is not None:
        path = path.resolve()
        path.mkdir(parents=True, exist_ok=False)
        return path
    parent = REPO / "build"
    parent.mkdir(exist_ok=True)
    return Path(tempfile.mkdtemp(prefix=prefix, dir=parent))


def build(args):
    out = output_directory(args.out, "benchmarks-build-")
    source = args.source.resolve()
    print(f"Build output: {out}", flush=True)
    with (out / "build.log").open("w") as log:
        adapter = "carryfs-windows" if sys.platform == "win32" else "carryfs-fuse"
        command(["cargo", "build", "--locked", "--offline", "--release",
                 "--target-dir", out, "-p", "carryfs-core", "-p", adapter],
                cwd=source, stdout=log, stderr=subprocess.STDOUT)
    if sys.platform == "linux":
        full.build_mount(out, source)
    from benchmarks import native
    manifest = dict(source=str(source), commit=full.capture(["git", "-C", source, "rev-parse", "HEAD"]).strip(),
                    binaries={executable: digest(out / "release" / executable)
                              for executable in native.binaries("user")})
    (out / "build.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Binaries: {out / 'release'}")


def compare(args):
    base_metadata, base_rows = full.load(args.base)
    candidate_metadata, candidate_rows = full.load(args.candidate)
    if base_metadata["configuration"].get("selection") != candidate_metadata["configuration"].get("selection"):
        raise ValueError("runs differ in metric selection; compare matching full or focused runs")
    for field in ("workload", "native_ro", "latency"):
        if base_metadata["configuration"][field] != candidate_metadata["configuration"][field]:
            raise ValueError(f"runs differ in {field}")
    if base_metadata["configuration"]["mount_mode"] != candidate_metadata["configuration"]["mount_mode"]:
        if not getattr(args, "allow_environment_diff", False):
            raise ValueError("runs differ in mount_mode")
        print("Environment differs in mount_mode; comparison includes that difference.", file=sys.stderr)
    fields = ("suite", "size_mib", "cache", "flush", "platform", "platform_version", "sources", "media", "backing_mount", "host", "kernel", "governors", "devices", "mount_settings")
    for field in fields:
        if base_metadata.get(field) != candidate_metadata.get(field):
            if getattr(args, "allow_environment_diff", False) and field in ("host", "kernel", "governors", "devices", "backing_mount", "mount_settings", "platform", "platform_version", "cache", "flush"):
                print(f"Environment differs in {field}; comparison includes that difference.", file=sys.stderr)
                continue
            raise ValueError(f"runs differ in {field}; use matching workloads and environments")
    values = [{}, {}]
    for groups, rows in zip(values, (base_rows, candidate_rows)):
        for row in rows:
            key = tuple(row[field] for field in ("disk", "label", "phase", "metric", "unit"))
            values_ = groups.setdefault(key, [])
            if row["value"] is not None:
                values_.append(float(row["value"]))
    if values[0].keys() != values[1].keys():
        raise ValueError("runs contain different metrics")
    print("| Disk | Filesystem | Phase | Metric | Unit | Base median (range) | Candidate median (range) | Change |")
    print("|---|---|---|---|---|---:|---:|---:|")
    for key, base in values[0].items():
        candidate = values[1][key]
        spread = lambda runs: f"{statistics.median(runs):.3f} ({min(runs):.3f}-{max(runs):.3f})" if runs else "n/a"
        median = statistics.median(base) if base else None
        change = f"{(statistics.median(candidate) / median - 1) * 100:+.1f}%" if median and candidate else "n/a"
        print(f"| {' | '.join(key)} | {spread(base)} | {spread(candidate)} | {change} |")
    print("\nChange is numerical: higher rates are better; lower times are better.")
    print("Separate runs are descriptive comparisons, not alternating base/candidate pairs.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    builder = commands.add_parser("build", help="build release binaries into a new directory, offline")
    builder.add_argument("--out", type=Path)
    builder.add_argument("--source", type=Path, default=REPO, help="source checkout to build")
    builder.set_defaults(action=build)
    for command_name, action in (("run", full.run), ("plan", full.plan)):
        runner = commands.add_parser(command_name, help="full suite" if command_name == "run" else "read-only full-suite preflight")
        target = runner.add_mutually_exclusive_group(required=True)
        target.add_argument("--config", type=Path, help="JSON disk/partition/mount configuration")
        target.add_argument("--images", action="store_true", help="disposable loop/raw/VHD images; Linux root, Windows administrator")
        runner.add_argument("--systems", nargs="+", choices=full.SYSTEMS, default=["carryfs", "exfat"])
        runner.add_argument("--rounds", type=positive, help="override configuration rounds, default 3")
        runner.add_argument("--size-mib", type=positive, default=1024, help="image capacity")
        runner.add_argument("--bin-dir", type=Path, default=REPO / "target/release")
        runner.add_argument("--out", type=Path)
        runner.add_argument("--format-devices", action="store_true", help="explicitly format configured physical partitions")
        runner.add_argument("--latency", action="store_true", help="add per-batch fsync percentiles; timing instrumentation is recorded")
        runner.add_argument("--native-ro", action="store_true", help="use native read-only mounts for RO phases; Linux root, Windows images only")
        runner.add_argument("--metric", help="run only this metric and its setup/checks; see the metrics command")
        runner.add_argument("--phase", help="selected metric phase, such as ro-cold or rw-warm")
        runner.set_defaults(action=action)
    comparison = commands.add_parser("compare", help="compare two completed runs; prints Markdown")
    comparison.add_argument("base", type=Path)
    comparison.add_argument("candidate", type=Path)
    comparison.add_argument("--allow-environment-diff", action="store_true", help="explicitly compare different hosts or physical disks")
    comparison.set_defaults(action=compare)
    metrics = commands.add_parser("metrics", help="list selectable metric names and phases with default workload sizes")
    def list_metrics(args):
        from benchmarks import focused
        for metric, (_, _, phases, unit) in focused.catalog(full.DEFAULTS).items():
            print(f"{metric}\t{unit}\t{', '.join(phases)}")
    metrics.set_defaults(action=list_metrics)
    summary = commands.add_parser("summary", help="recompute a full or focused table from verified raw samples")
    summary.add_argument("run", type=Path)
    summary.set_defaults(action=lambda args: print(full.report(args.run)[0]))
    args = parser.parse_args()
    try:
        args.action(args)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr, end="")
        parser.exit(1, f"benchmark failed: {error}\nIf a results directory was printed, its logs are retained there.\n")


if __name__ == "__main__":
    main()
