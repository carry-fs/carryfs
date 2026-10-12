"""Run one requested metric with its setup, remount verification and checks."""

import json
import shutil
import sys
import time

from benchmarks import full


def catalog(w):
    result = {}
    for operation, metrics in (
        ("sequential-write", {"sequential_write": "MiB/s", "fsync_after_write": "ms", "sequential_write_durable": "MiB/s"}),
        ("small-files", {"small_file_create": "files/s", "small_file_latency": "ms", "fsync_after_small_files": "ms", "small_file_create_durable": "files/s"}),
        ("tree", {"tree_create": "files/s"}),
        ("listing", {"listdir": "entries/s", "stat_each": "entries/s"}),
    ):
        for metric, unit in metrics.items():
            result[metric] = (operation, None, ("write",), unit)
    result["sequential_read"] = ("sequential-read", None, ("ro-cold", "ro-warm", "rw-cold", "rw-warm"), "MiB/s")
    for metric, directory in (("remove_small_files", "files"), ("remove_tree", "tree")):
        result[metric] = ("remove", directory, ("read",), "ms")
    result["fsync_after_remove"] = ("remove-sync", None, ("delete",), "ms")
    for operation, verb, phase in (("batch-create", "create", "write"), ("batch-remove", "remove", "delete")):
        result[f"{verb}_{w['many_files']}_batch{w['batch']}_durable"] = (operation, None, (phase,), "ms")
    for percentile in ("p50", "p95", "p99", "max"):
        result["batch_fsync_" + percentile] = ("batch-latency", None, ("write", "delete"), "ms")
    phases = ("ro-cold", "ro-warm", "ro-stale", "rw-cold", "rw-warm", "rw-stale")
    for directory, count in (("files", w["files"]), ("many", w["many_files"])):
        for pattern in full.listing.PATTERNS:
            result[f"list_{count}_{pattern}"] = ("listing-cell", (directory, count, pattern), phases, "ms")
    return result


def select(w, metric, phase):
    if metric is None:
        if phase is not None:
            raise ValueError("--phase requires --metric")
        return None
    options = catalog(w)
    if metric not in options:
        raise ValueError(f"unknown metric {metric!r}; run 'python3 benchmarks/run.py metrics' for names")
    phases = options[metric][2]
    phase = phase or phases[0]
    if phase not in phases:
        raise ValueError(f"{metric} accepts phases: {', '.join(phases)}")
    return dict(metric=metric, phase=phase)


def operation(w, selection):
    kind = catalog(w)[selection["metric"]][0]
    if kind == "batch-latency":
        return "batch-remove" if selection["phase"] == "delete" else "batch-create"
    return kind


def save_manifest(root):
    with (root / "manifest.json").open("w") as sink:
        json.dump(full.bench.manifest(str(root)), sink)
        sink.flush()
        full.bench.os.fsync(sink.fileno())
    full.bench.sync_path(root)


def listing_pass(directory, count, pattern):
    result, names, attributes = full.listing.measure(directory, pattern)
    full.validate_listing(names, attributes, count, inode_ids=not (sys.platform == "win32" and pattern == "interleaved"))
    return result, names


def run(sample):
    selection, w = sample.runner.config["selection"], sample.runner.workload
    metric, phase = selection["metric"], selection["phase"]
    kind = operation(w, selection)
    parameter = catalog(w)[metric][1]
    sample.phase = "setup"
    root = sample.mount("rw", "setup")
    root.mkdir()
    writes = kind in ("sequential-write", "small-files", "tree", "batch-create")
    if not writes:
        if kind == "sequential-read":
            full.bench.sequential_write(str(root), "", w["seq_mib"])
        elif kind == "batch-remove" or kind == "listing-cell" and parameter[0] == "many":
            sample.batch_files(root)
        elif kind == "remove" and parameter == "tree":
            full.bench.tree(str(root), "", w["dirs"], w["per_dir"])
        else:
            full.bench.small_files(str(root), "", w["files"])
            if kind == "remove-sync":
                full.bench.tree(str(root), "", w["dirs"], w["per_dir"])
        save_manifest(root)
        sample.unmount("setup")
        sample.mount("ro", "verify-setup")
        sample.verify(root)
        sample.unmount("verify-setup")
        mode = phase.split("-")[0] if kind in ("sequential-read", "listing-cell") else "rw"
        sample.mount(mode, "selected")
    sample.phase = phase
    if kind == "sequential-write":
        full.bench.sequential_write(str(root), "", w["seq_mib"])
        if (root / "seq.bin").stat().st_size != w["seq_mib"] << 20:
            raise ValueError("sequential write size differs")
    elif kind == "small-files":
        full.bench.small_files(str(root), "", w["files"])
    elif kind == "tree":
        full.bench.tree(str(root), "", w["dirs"], w["per_dir"])
    elif kind in ("batch-create", "batch-remove"):
        sample.batch_files(root, remove=kind == "batch-remove")
    elif kind == "sequential-read":
        if phase.endswith("warm"):
            sample.phase = "setup"
            full.bench.sequential_read(str(root), "", w["seq_mib"])
            sample.phase = phase
        full.bench.sequential_read(str(root), "", w["seq_mib"])
    elif kind == "listing":
        full.bench.listing(str(root), "", "write")
    elif kind == "listing-cell":
        directory, count, pattern = parameter
        cache = phase.split("-")[1]
        for _ in range({"cold": 0, "warm": 1, "stale": 2}[cache]):
            listing_pass(root / directory, count, pattern)
        if cache == "stale":
            time.sleep(w["stale_seconds"])
        result, names = listing_pass(root / directory, count, pattern)
        sample.record("listing", mode=mode, cache=cache, **result)
        if sys.platform == "win32" and pattern == "interleaved":
            full.validate_listing(names, [full.os.stat(root / directory / name) for name in names], count)
    elif kind == "remove":
        elapsed, _ = full.bench.timed(lambda: shutil.rmtree(root / parameter))
        sample.emit(None, "read", metric, elapsed * 1000, "ms")
        full.bench.sync_path(root)
    elif kind == "remove-sync":
        shutil.rmtree(root / "files")
        shutil.rmtree(root / "tree")
        elapsed, _ = full.bench.timed(lambda: full.bench.sync_path(root))
        sample.emit(None, "delete", metric, elapsed * 1000, "ms")
    sample.phase = "setup"
    if writes:
        save_manifest(root)
        sample.verify(root)
    sample.unmount("selected")
    sample.mount("ro", "verify-final")
    remaining = () if kind in ("remove", "remove-sync", "batch-remove") else None
    sample.verify(root, remaining=remaining)
    sample.unmount("verify-final")
    sample.check()
