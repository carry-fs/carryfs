CarryFS comparison with bbtree

The measurements do not support replacing all production core BTreeMaps. Metadata workloads are within roughly 1% of std maps, with most medians slightly slower. Deferred large writes show a 1.030x median ratio, but individual paired ratios range from 0.976x to 1.339x. Scratch-file results also vary enough that they do not establish a consistent I/O improvement.

Large ordered maps benefit substantially in isolation. At 1,024 and 4,096 entries, the synthetic data-index and assignment workloads reach roughly 2.3x to 2.5x. Small or shuffled maps can regress. These results explain why the library's map-level gains do not translate into a broad filesystem gain here.

The comparison uses CarryFS `df656f3d053167f4788d53e33a3ce7a8fdb0b03e` and [bbtree](https://github.com/XiangpengHao/bbtree) version 0.1.5, commit `8adae97a0ed97e5c98fa2790312eb0aec9719846`. The host is Apple M1, 16 GB RAM, macOS 15.6, arm64, with `rustc 1.99.0 (b940084d7 2026-09-28)`. Both variants use Cargo's release optimization and identical build settings, with binary stripping disabled because the installed strip helper could not load LLVM. RUSTFLAGS is unset.

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
| create | deferred | 20,000 | 92.990 | 93.376 | 0.999x | 0.985–1.004x |
| create | eager | 20,000 | 1188.553 | 1190.458 | 1.001x | 0.953–1.005x |
| small-write | deferred | 4,000 | 54.267 | 54.580 | 0.995x | 0.977–1.004x |
| small-write | eager | 4,000 | 423.666 | 426.001 | 0.994x | 0.947–1.005x |
| churn | deferred | 4,000 | 79.985 | 80.810 | 0.989x | 0.970–1.006x |
| churn | eager | 4,000 | 640.781 | 644.205 | 0.995x | 0.990–1.008x |
| large-write | deferred | 512 | 139.273 | 135.739 | 1.030x | 0.976–1.339x |
| large-write | eager | 64 | 19.053 | 18.809 | 1.002x | 0.985–1.050x |
| partial-overwrite | deferred | 256 | 92.939 | 90.960 | 1.022x | 0.947–1.078x |
| partial-overwrite | eager | 32 | 11.139 | 11.104 | 1.010x | 0.938–1.322x |

The scratch-file cases use ImageDevice on newly created regular files under this temporary directory, with real reads, writes, and durable flush calls. Every case uses deferred publication. These measurements exercise core image I/O directly and exclude a FUSE or WinFsp mount.

| Scratch-file workload | Files or 1 MiB writes | std median, ms | bbtree median, ms | Speedup | Paired range |
|---|---:|---:|---:|---:|---|
| create | 300 | 12.721 | 12.596 | 1.006x | 0.695–1.097x |
| small-write | 100 | 12.861 | 13.013 | 0.988x | 0.860–1.072x |
| churn | 100 | 25.528 | 25.760 | 0.960x | 0.896–1.319x |
| large-write | 32 | 55.514 | 53.811 | 1.031x | 0.947–1.180x |

The following map benchmarks model the production key and value types at selected sizes. Each cycle includes map construction and destruction. Inputs are precomputed ordered or deterministically shuffled unique keys. Each timed sample runs for at least 120 ms.

- `data_index` uses u64 keys and usize values, duplicate checks, inserts, range counts and scans, and removal of a suffix of inserted keys.
- `assignments` collects usize keys and u64 values, checks remaining keys, extends the map, then iterates and resolves entries.
- `updates` inserts and overwrites 17-byte keys and optional 32-byte values, then iterates in order. Key and value allocations are timed.
- `pending` inserts Vec write buffers and uses the actual FUSE adapter's overlay and run-grouping functions, copied without algorithm changes. Buffer allocation and destruction are timed. The four MiB buffer cap corresponds to 1,024 entries of four KiB each.

These are synthetic operation mixes. Production map-size and access distributions were not traced. In particular, pending-map results describe the helper workload alone; the core candidate does not include a mounted FUSE adapter.

| Map workload | Entries | Order | std cycle, µs | bbtree cycle, µs | Speedup | Paired range |
|---|---:|---|---:|---:|---:|---|
| data_index | 32 | ordered | 0.864 | 1.022 | 0.846x | 0.822–0.857x |
| assignments | 32 | ordered | 0.642 | 0.563 | 1.146x | 1.064–1.174x |
| updates | 32 | ordered | 4.931 | 4.577 | 1.074x | 0.981–1.092x |
| pending | 32 | ordered | 5.053 | 5.093 | 1.002x | 0.858–1.054x |
| data_index | 32 | shuffled | 0.932 | 1.350 | 0.700x | 0.612–0.728x |
| assignments | 32 | shuffled | 0.794 | 0.714 | 1.109x | 0.890–1.227x |
| updates | 32 | shuffled | 5.218 | 5.732 | 0.911x | 0.872–0.989x |
| pending | 32 | shuffled | 5.076 | 5.721 | 0.894x | 0.878–0.907x |
| data_index | 256 | ordered | 10.377 | 9.128 | 1.132x | 1.107–1.190x |
| assignments | 256 | ordered | 6.994 | 5.170 | 1.347x | 1.277–1.431x |
| updates | 256 | ordered | 49.245 | 38.135 | 1.283x | 0.830–1.328x |
| pending | 256 | ordered | 44.304 | 37.753 | 1.169x | 1.107–1.207x |
| data_index | 256 | shuffled | 10.341 | 19.091 | 0.540x | 0.516–0.551x |
| assignments | 256 | shuffled | 7.926 | 10.012 | 0.793x | 0.743–0.822x |
| updates | 256 | shuffled | 53.007 | 49.277 | 1.078x | 1.023–2.145x |
| pending | 256 | shuffled | 47.402 | 51.219 | 0.929x | 0.900–1.082x |
| data_index | 1,024 | ordered | 91.286 | 36.668 | 2.468x | 2.154–2.904x |
| assignments | 1,024 | ordered | 54.828 | 23.961 | 2.297x | 2.173–2.365x |
| updates | 1,024 | ordered | 215.056 | 162.624 | 1.323x | 1.283–2.594x |
| pending | 1,024 | ordered | 186.085 | 151.289 | 1.216x | 1.077–1.335x |
| data_index | 1,024 | shuffled | 92.401 | 88.565 | 1.038x | 0.968–1.079x |
| assignments | 1,024 | shuffled | 69.805 | 45.373 | 1.541x | 1.402–1.839x |
| updates | 1,024 | shuffled | 244.969 | 214.575 | 1.144x | 1.123–1.168x |
| pending | 1,024 | shuffled | 200.720 | 212.916 | 0.943x | 0.922–1.362x |
| data_index | 4,096 | ordered | 445.058 | 182.618 | 2.426x | 2.405–2.982x |
| assignments | 4,096 | ordered | 265.633 | 104.769 | 2.534x | 2.503–2.559x |
| updates | 4,096 | ordered | 947.048 | 654.565 | 1.433x | 1.223–1.491x |
| data_index | 4,096 | shuffled | 560.210 | 460.761 | 1.221x | 1.196–1.335x |
| assignments | 4,096 | shuffled | 401.140 | 227.445 | 1.764x | 1.645–1.865x |
| updates | 4,096 | shuffled | 1130.091 | 922.389 | 1.229x | 1.123–1.297x |

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
