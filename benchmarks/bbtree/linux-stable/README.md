# Linux stable bbtree investigation

This experiment repeats the saved CarryFS comparison with Rust stable 1.99.1 on the Linux AMD EPYC 7543 KVM host. It keeps the original M1 records and the earlier Linux nightly results intact.

[Complete measurements](RESULTS.md) contain means, medians, coefficients of variation, paired confidence intervals, and the three block means. [Raw samples](results/raw.tsv), [metadata](results/metadata.json), and [diagnostic samples](diagnostics/probes.json) retain the evidence.

## Results and noise

The stable rerun still does not establish a broad CarryFS benefit from replacing all three temporary maps. Deferred core workloads average between 0.997× and 1.016×, and each interval includes parity. Eager create averages 1.024× and eager churn 1.031×, so there are small wins in this experiment. The churn block ratios vary from 1.004× to 1.068×. Those results do not imply a similar improvement for other workloads or a live mount.

Core timing variation is about 4–9%, measured as standard deviation divided by the mean. Map variation is about 4–11%, despite process CPU time and CPU pinning. The average map CPU/wall fractions exceed 99.7%, so descheduling is not the main source of their measured CPU-time differences. File I/O varies much more: create has 32% std variation and 61% bbtree variation. Its ratio of means is 0.909×, but its median paired ratio is 1.019×. The retained outliers make a single average misleading; the report includes both statistics and each block's ratio.

Selected complete-lifecycle results, before allocator intervention:

| Map workload | Entries | Average speedup | 95% paired interval |
|---|---:|---:|---|
| Ordered data index | 1 | 0.170× | 0.165–0.175× |
| Ordered data index | 1,024 | 1.494× | 1.446–1.538× |
| Ordered assignments | 4,096 | 1.797× | 1.755–1.839× |
| Ordered buffered writes | 1,024 | 0.127× | 0.124–0.131× |
| Ordered buffered writes | 4,096 | 2.900× | 2.792–2.998× |

The small-map cost is real in these models. A one-entry data-index lifecycle takes about 5.9 times as much CPU with bbtree. Counting allocations found one 192-byte request for std and one zeroed, highly aligned 1,089-byte request for bbtree. The allocation counter establishes a fixed-cost difference; it does not attribute every nanosecond of the regression to allocation.

The fresh [Linux occupancy trace](diagnostics/calibration.json) reproduces the original histogram. Small-write data indexes have one entry; churn cleanup maps have 32. Large-write data indexes have 256 entries, while their assignment maps have a median of four and a maximum of seven. These counts describe observations at the calibration's instrumentation points in these Volume API workloads. They are not a trace of a live filesystem or time-weighted CPU shares.

## Why the buffered-write numbers reverse

The extreme buffer-heavy ratios are caused largely by allocation history and glibc heap trimming, rather than by tree searches. Each lifecycle allocates and releases 4 KiB buffers. Node layout and destruction order change the allocator's heap state. glibc can return free space at the top of the heap to the OS; later allocations must extend and fault those pages in again. Its thresholds can also adapt to the process's allocation history.

The controlled probe changes only the child process's trimming threshold. The map code, compiler, keys, CPU affinity, warmup, and measurement duration stay fixed:

| Ordered pending entries | Default allocator ratio | Trimming threshold raised to 1 GiB |
|---|---:|---:|
| 1,024 | 0.131×, interval 0.125–0.138× | 1.440×, interval 1.382–1.505× |
| 4,096 | 3.072×, interval 2.926–3.219× | 1.049×, interval 0.945–1.171× |

For the 1,024-entry bbtree process, `strace` counted 9,030 `brk` calls by default and 35 with trimming suppressed. `perf stat` counted 373,462 minor faults by default and 1,142 after the change. The corresponding std runs had 37 `brk` calls and 1,136 minor faults by default. These are whole-process counts including setup and warmup, from separate traced runs. They are not normalized per timed lifecycle. Tracing adds overhead, so the untraced alternating pairs above supply the timing ratios.

A separate [user/system CPU split](diagnostics/cpu-time-split.json) confirms where that work goes. The default bbtree process spends about 82% of its CPU time in the kernel. With trimming suppressed, that share falls below 1%. Both observations include setup and warmup.

The [phase probes](RESULTS.md#buffered-write-phases) locate most of the 1,024-entry bbtree penalty in allocation and initialization, with a smaller release cost. Raising the threshold removes it. At 4,096 entries the default allocation behavior hurts std more, inflating bbtree's apparent advantage. The instrumented phase binaries change layout and timing overhead, so their absolute times are not substituted for the primary measurements.

The original short harness runs many configurations in one process. The scaling harness starts a fresh process for each point and warms it up. In the stable short harness, shuffled pending writes at 1,024 entries reach 7.193× by default, but 0.863× with trimming suppressed. Ordered pending writes change from 0.967× to 1.254×. This explains why changing process history can produce a very different apparent winner even with identical lifecycle functions. Raising the threshold changes memory retention and is a diagnostic intervention, not a production recommendation.

The earlier chat comparison also mixed two key orders: the 7.451× nightly short-run result was shuffled, while the 0.123× isolated result was ordered. They were not directly comparable. This report keeps order and process setup explicit. The corrected comparison still shows allocator-driven reversals.

The matched compiler probe measures 0.133× on stable and 0.128× on nightly for the isolated 1,024-entry ordered case. Both show the same severe default-allocator regression. Compiler changes can affect individual timings, especially tiny cases, but they do not explain this reversal. The M1-to-EPYC comparison also changes architecture, operating system, allocator, and scheduling, so its differences cannot be attributed to Linux alone.

The sampled user-space profiles have incomplete library symbol resolution and few samples in the slow case, where much CPU time is spent in the kernel. They are retained for inspection; the causal conclusion relies on the allocator intervention, syscall counts, page-fault counts, and phase probes.

## Why the headline does not predict CarryFS

The main differences are map size, map lifetime, key types, operation mix, batching, and the fraction of filesystem work spent in these containers. Large trees can amortize bbtree's setup costs and benefit from fewer node accesses and inline-key comparisons. A one-entry map cannot. Buffer allocation can dominate both implementations, as the controlled probes demonstrate.

In this host's diagnostic, a prebuilt million-entry integer map reaches 1.252× for scalar 50% hit/miss lookups and 2.085× with 64-key batches. At 1,024 entries both variants of that lookup model favor std. These probes use a fixed 16,384-key query sample and are not an exact reproduction of the upstream's larger, randomized workload. They show the value of isolating a steady lookup from a short-lived map's full lifecycle.

I would keep std for the current CarryFS maps. A future profile that identifies a large, long-lived ordered map as a CPU bottleneck would justify a targeted test, with real key distributions and allocator behavior included. More repetitions narrow random timing uncertainty; they do not make different key orders, allocation histories, or operation mixes equivalent.

## Method

The primary comparison uses three blocks of ten sequential A/B pairs per configuration, with a different deterministic case order in each block. Variant order alternates within each block. Each process runs on virtual CPU 2. Builds, calibration, and profiling run separately from the primary measurements.

The core suite retains all ten original workload/mode combinations. The scratch-file suite retains its four cases. The map suite covers all four lifecycle models at 1, 32, 256, 1,024, and 4,096 ordered entries, plus shuffled inputs at 256 and 1,024 entries. This fixed grid targets CarryFS-sized maps rather than repeating the adaptive multi-million-entry stress sweep. There are 42 configurations, 30 measured pairs each, and 2,520 primary process samples.

Core and scratch-file runs have one warmup pair per configuration in each block. Their timer measures the original workload's wall time. Map processes warm up for 100 ms and then measure at least 300 ms, retaining both process CPU and wall time. Each map sample includes construction, the modeled operations, and destruction. Format, reopen, and structural validation remain outside the core timer. Every measured pair checks matching results. The core harness verifies file contents, namespace lookups, and reopened image structure.

The primary ratio divides mean std time by mean bbtree time; above 1 favors bbtree. The report also gives the median paired ratio. Its 95% bootstrap intervals resample pairs 5,000 times with a fixed seed. These intervals assume independent pairs and do not measure uncertainty across every VM state or allocator history. The three block ratios help expose time drift. No samples are removed as outliers.

The CarryFS and bbtree source snapshots match the archived experiment. Only the disposable map harness's platform calls change: Linux process CPU clock ID 2 replaces macOS ID 12, and the macOS QoS request is removed. The lifecycle functions are unchanged. Cargo release builds use the archived dependency cache, with no target-cpu or RUSTFLAGS tuning.

## What the upstream claim measures

The pinned [README](https://github.com/XiangpengHao/bbtree/blob/8adae97a0ed97e5c98fa2790312eb0aec9719846/README.md) advertises a fivefold single-thread speedup. That is a broad headline, not a guarantee for each key type, map size, operation mix, or filesystem.

The associated [single-thread chart](https://github.com/XiangpengHao/bbtree/blob/8adae97a0ed97e5c98fa2790312eb0aec9719846/bbtree/vis/src/single_thread_chart.rs) uses 10 million entries, 50% hit/miss lookups, and separate updates, insertion, and scans. Batched bars use 64 lookups or 16 inserts per API call. In the archived [Ryzen 9900X data](https://github.com/XiangpengHao/bbtree/blob/8adae97a0ed97e5c98fa2790312eb0aec9719846/bbtree/vis/data/9900x-20261017/throughput.csv), scalar integer lookups reach about 2.42×, scalar JSON-string lookups 4.11×, and batched JSON-string lookups 6.56× against std. The headline hides that range.

The [upstream operation runner](https://github.com/XiangpengHao/bbtree/blob/8adae97a0ed97e5c98fa2790312eb0aec9719846/bbtree/bench/src/operations/single_map.rs) builds the map and prepares probes outside the lookup timer. CarryFS's lifecycle models pay those costs repeatedly. The diagnostic lookup probe in this investigation isolates the same distinction using integer keys; it is not an exact reproduction of the upstream workload.

The library's design also has fixed costs. Its [default node configuration](https://github.com/XiangpengHao/bbtree/blob/8adae97a0ed97e5c98fa2790312eb0aec9719846/bbtree/src/node_config.rs) uses leaves of at least 1 KiB, and its default allocator requests zeroed, aligned storage. The diagnostic allocator counts measure that cost directly. `new()` itself allocates nothing; the first insertion creates a tree page.

The default inline-key recognition includes `u64`, `i64`, `u128`, and `i128`, but [does not include `usize`](https://github.com/XiangpengHao/bbtree/blob/8adae97a0ed97e5c98fa2790312eb0aec9719846/bbtree/src/tree/node/type_identity.rs). CarryFS's assignment IDs use `usize`, so that optimization is not active for those keys. This is a source-level limitation, not a measured attribution of the assignment workload's latency.

Replacing three temporary maps leaves the persistent tree, page encoding, data copies, publication, checksums, and device I/O intact. Even a large speedup for a particular map operation cannot produce the same speedup for the whole filesystem unless that operation dominates its runtime.

## Reproduce

Use the installed stable toolchain. No tool installation is performed by these scripts.

```sh
python3 benchmarks/bbtree/linux-stable/run.py --build
python3 benchmarks/bbtree/linux-stable/run.py --output build/bbtree-fs-evaluation/stable/new-results
python3 benchmarks/bbtree/linux-stable/diagnose.py --build
python3 benchmarks/bbtree/linux-stable/diagnose.py --output build/bbtree-fs-evaluation/stable/new-diagnostics
python3 benchmarks/bbtree/linux-stable/diagnose.py --extra-size 4096 --output build/bbtree-fs-evaluation/stable/new-diagnostics
```

Run one command at a time. The scripts refuse to overwrite raw measurements. The default output paths hold this run's checked-in evidence; use fresh output directories to rerun. `run.py --summarize --output PATH` recomputes a run's summary. `report.py` validates and renders the retained results in this directory. Diagnostic profiling uses the already installed `perf` and `strace`.

The compiler control reads the earlier nightly binaries only to compare the two compilers on the same host and CPU. Primary results and diagnostic binaries use stable. Allocator probes set `GLIBC_TUNABLES` only in their child processes. The `no-trim` control raises the automatic trimming threshold to 1 GiB, following [glibc's documented setting](https://sourceware.org/glibc/manual/latest/html_node/Memory-Allocation-Tunables.html). This is a causal probe, not a production allocator recommendation.

Mounted FUSE throughput and bare-metal behavior are outside this experiment. The host is a shared virtual machine; CPU pinning cannot eliminate hypervisor contention or frequency changes.
