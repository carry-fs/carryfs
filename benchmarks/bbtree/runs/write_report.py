"""Generate the report tables from the measured samples."""
from pathlib import Path
import json
ROOT = Path(__file__).resolve().parent
meta = json.loads((ROOT/'metadata.json').read_text())
core = json.loads((ROOT/'core-summary.json').read_text())
maps = json.loads((ROOT/'maps-summary.json').read_text())
file = json.loads((ROOT/'file-summary.json').read_text())
text = f'''CarryFS comparison with bbtree

The measurements do not support replacing all production core BTreeMaps. Metadata workloads are within roughly 1% of std maps, with most medians slightly slower. Deferred large writes show a 1.030x median ratio, but individual paired ratios range from 0.976x to 1.339x. Scratch-file results also vary enough that they do not establish a consistent I/O improvement.

Large ordered maps benefit substantially in isolation. At 1,024 and 4,096 entries, the synthetic data-index and assignment workloads reach roughly 2.3x to 2.5x. Small or shuffled maps can regress. These results explain why the library's map-level gains do not translate into a broad filesystem gain here.

The comparison uses CarryFS `{meta['carryfs_commit']}` and [bbtree] (https://github.com/XiangpengHao/bbtree) version {meta['bbtree_version']}, commit `{meta['bbtree_commit']}`. The host is {meta['machine']}, macOS {meta['macos']}, {meta['architecture']}, with `{meta['rustc']}`. Both variants use Cargo's release optimization and identical build settings, with binary stripping disabled because the installed strip helper could not load LLVM. RUSTFLAGS is unset.

The candidate replaces only `Forest::data_index`, transaction cleanup `updates`, and page `assignments` with `bbtree::unsync::BBTreeMap`. BTreeSet, HashMap, persistent tree logic, and the simulated device are unchanged. The exact edits are in [candidate.patch](candidate.patch). Only `src/tree.rs` and `src/allocation.rs` differ between the copied core sources.

Each case has nine measured pairs after one warmup pair. Runs execute sequentially, with variant order alternating between pairs. No build or other benchmark runs concurrently. The host uses normal scheduling, without CPU pinning. The speedup is the median of each paired std duration divided by its bbtree duration. A ratio above 1 means bbtree is faster. Range gives the minimum and maximum paired ratios. The duration columns contain independent medians, so their quotient can differ from the median paired ratio.

The following workloads use the real Volume API over a 256 MiB byte-array block device. Device reads and writes copy bytes, and durable flush is a no-op. Format, name and payload preparation, post-run namespace verification, eject, reopen, and structural checking are outside the timer. File-content verification is part of the timed read workloads.

- `create` creates and closes empty files, then syncs.
- `small-write` creates files, writes 4 KiB per file, closes them, syncs, then reopens and reads each file.
- `churn` runs the small-write workload, removes every file, and syncs again.
- `large-write` writes 1 MiB chunks to a rolling 32 MiB file, with a sync every 16 writes.
- `partial-overwrite` adds an unaligned 4 KiB overwrite at offset 123 after every 1 MiB write, with the same sync schedule.

Deferred mode batches publications as the adapters do. Eager mode publishes each mutation. Counts for the two eager overwrite cases are smaller because the longer cases exhaust the 256 MiB scratch image in both variants. The failed probes are excluded from results. The completed cases were retained without rerunning them.

| RAM core workload | Mode | Files or 1 MiB writes | std median, ms | bbtree median, ms | Speedup | Paired range |
|---|---|---:|---:|---:|---:|---|
'''
for r in core:
    text += f"| {r['case']} | {'deferred' if r['mode']=='true' else 'eager'} | {r['size']:,} | {r['std_ns']/1e6:.3f} | {r['bb_ns']/1e6:.3f} | {r['speedup']:.3f}x | {r['min_ratio']:.3f}–{r['max_ratio']:.3f}x |\n"
text += '''
The scratch-file cases use ImageDevice on newly created regular files under this temporary directory, with real reads, writes, and durable flush calls. Every case uses deferred publication. These measurements exercise core image I/O directly and exclude a FUSE or WinFsp mount.

| Scratch-file workload | Files or 1 MiB writes | std median, ms | bbtree median, ms | Speedup | Paired range |
|---|---:|---:|---:|---:|---|
'''
for r in file:
    text += f"| {r['case']} | {r['size']} | {r['std_ns']/1e6:.3f} | {r['bb_ns']/1e6:.3f} | {r['speedup']:.3f}x | {r['min_ratio']:.3f}–{r['max_ratio']:.3f}x |\n"
text += '''
The following map benchmarks model the production key and value types at selected sizes. Each cycle includes map construction and destruction. Inputs are precomputed ordered or deterministically shuffled unique keys. Each timed sample runs for at least 120 ms.

- `data_index` uses u64 keys and usize values, duplicate checks, inserts, range counts and scans, and removal of a suffix of inserted keys.
- `assignments` collects usize keys and u64 values, checks remaining keys, extends the map, then iterates and resolves entries.
- `updates` inserts and overwrites 17-byte keys and optional 32-byte values, then iterates in order. Key and value allocations are timed.
- `pending` inserts Vec write buffers and uses the actual FUSE adapter's overlay and run-grouping functions, copied without algorithm changes. Buffer allocation and destruction are timed. The four MiB buffer cap corresponds to 1,024 entries of four KiB each.

These are synthetic operation mixes. Production map-size and access distributions were not traced. In particular, pending-map results describe the helper workload alone; the core candidate does not include a mounted FUSE adapter.

| Map workload | Entries | Order | std cycle, µs | bbtree cycle, µs | Speedup | Paired range |
|---|---:|---|---:|---:|---:|---|
'''
for r in maps:
    text += f"| {r['case']} | {r['size']:,} | {r['mode']} | {r['std_ns']/1e3:.3f} | {r['bb_ns']/1e3:.3f} | {r['speedup']:.3f}x | {r['min_ratio']:.3f}–{r['max_ratio']:.3f}x |\n"
text += '''
Correctness checks passed for both variants' workload runs: file data matched the expected payload, namespace lookups matched the operations, and reopened images passed the structural check. Every recorded pair had matching checksums. The candidate also passed 18 tree tests and 19 allocation tests. Four allocation tests retain their existing ignored status. The full suite, mounted filesystem behavior, Linux, Windows, and other CPU architectures were not checked.

All 180 core, 540 map, and 72 scratch-file measured samples are retained in [core-raw.tsv](core-raw.tsv), [maps-raw.tsv](maps-raw.tsv), and [file-raw.tsv](file-raw.tsv). Summary JSON files include the coefficient of variation for each variant. Some cases have substantial outliers, so small ratios should be treated as inconclusive. No samples were removed as outliers.

The original repository remains clean. All dependency downloads, source copies, executables, test logs, and results are under this directory. No tool was installed globally and no mounted or physical filesystem was formatted.

The rerun commands use the retained source snapshots and pinned Cargo.lock:

```sh
python3 build.py
python3 run.py core --repeats 9
python3 run.py maps --repeats 9
python3 run.py file --repeats 9
python3 write_report.py
```

`build.py` records this machine's Rust toolchain paths. On another machine, update its cargo and rustc paths. Dependencies are cached in `cargo-home`; the upstream git source is pinned at the commit above. The comparison harness is in [harness/src/bin/core.rs](harness/src/bin/core.rs) and [harness/src/bin/maps.rs](harness/src/bin/maps.rs). The existing upstream and CarryFS source copies and their licenses remain available beside the harness.
'''
text=text.replace('[bbtree] (','[bbtree](')
(ROOT/'report.md').write_text(text)
print(ROOT/'report.md')
