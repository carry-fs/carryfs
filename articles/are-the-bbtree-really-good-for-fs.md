# Are the bbtree really good for FS?

CarryFS is keeping `std::collections::BTreeMap` after this experiment. Large ordered maps ran roughly 2.5 times faster with `bbtree`, but the temporary maps in our filesystem workloads were often much smaller. A one-entry data-index simulation took about six times as much CPU time with `bbtree`.

That spread was worth investigating. The [bbtree README](https://github.com/XiangpengHao/bbtree/blob/8adae97a0ed97e5c98fa2790312eb0aec9719846/README.md) advertises a fivefold single-thread gain over Rust's standard map. I wanted to know how much of that CarryFS could use. The answer required two measurements: the effect on core filesystem operations, and the effect of map size on the container itself.

CarryFS stores its persistent metadata in its own copy-on-write tree. The standard maps we considered replacing hold temporary state: queued data-page indexes, page assignments during publication, and cleanup updates. The FUSE adapter also keeps buffered writes in an ordered map. A directory with many files can still produce a transaction with only a few entries in one of these maps.

This distinction changes the experiment. Filesystem capacity and total file count do not tell us how large the candidate maps become. We need their occupancies and the operations performed during their lifetimes.

I first replaced the three core map types in an isolated source copy, leaving `BTreeSet`, `HashMap`, and the persistent tree logic unchanged. The replacement was `bbtree::unsync::BBTreeMap`, which fits these locally owned mutable maps. The buffered-write map was simulated separately using the adapter's overlay and write-grouping functions.

The tests used an Apple M1 with 16 GB RAM, macOS 15.6, and Rust 1.99.0 in release mode. CarryFS was pinned to `df656f3d053167f4788d53e33a3ce7a8fdb0b03e`. The bbtree source was version 0.1.5 at `8adae97a0ed97e5c98fa2790312eb0aec9719846`. These are results for that hardware, those revisions, and these operation mixes.

The first comparison exercised the real `Volume` API over a 256 MiB memory-backed block device. Reads and writes copied bytes, and durable flush was a no-op. Format and post-run structural checks were outside the timer. With deferred publication, the median paired wall-time ratios were:

| Core workload | bbtree speedup |
|---|---:|
| Create 20,000 files | 0.999× |
| Create, write, and read small files | 0.995× |
| Create, write, read, and delete small files | 0.989× |
| Large writes | 1.030× |
| Partial overwrites | 1.022× |

Above 1× means faster than the standard map. The small-file cases used 4,000 files. Large writes repeatedly wrote 1 MiB chunks into a rolling 32 MiB file, with a sync every 16 writes. Partial overwrites added an unaligned 4 KiB write after each chunk. The [initial report](../benchmarks/bbtree/runs/report.md) records the counts and eager-publication results.

Nine paired repetitions were enough to see that this was no broad filesystem acceleration. They were less convincing about a small write gain. One standard-map large-write run took about 180 ms while the others were around 139 ms. The ratio of average durations suggested a 5.6% gain; the median paired ratio suggested 3.0%. That difference came from the outlier's weight in the average. Scratch-file image tests with real I/O also failed to establish a consistent gain. No mounted filesystem was measured.

Before extending the map benchmarks, I added a separate calibration build. It recorded occupancies during five core workloads and emitted histograms after each run. The instrumentation stayed out of the timed map sweep. These observations describe those workloads, rather than every possible CarryFS workload.

| Observed operation | Map | Median nonempty entries | Maximum |
|---|---|---:|---:|
| Write small files | Data index | 1 | 1 |
| Write 1 MiB chunks | Data index | 256 | 256 |
| Partially overwrite files | Data index | 16 | 256 |
| Create empty files | Assignments | 2 | 1,033 |
| Create, write, read, and delete files | Assignments | 4 | 564 |
| Write 1 MiB chunks | Assignments | 4 | 7 |
| Cleanup during large writes | Cleanup updates | 32 | 32 |

The assignment map sometimes became large enough to make the comparison interesting. Its ordinary size was still a few entries. Cleanup maps in these runs were usually empty or held 32 entries. The data index changed with the transfer size, so a large-write result could not stand in for small writes.

The next sweep started at 1, 4, 32, 256, 1,024, and 4,096 entries, then increased active cases further. Each configuration had seven alternating A/B pairs. Every process warmed for at least 100 ms and measured for at least one second of wall time. Large lifecycles could take longer than a second on their own. The complete sweep covered 67 configurations and 938 measured process runs.

Longer duration alone did not remove the noise. Early diagnostics showed substantial scheduling variance on the loaded host. The final harness requested user-initiated QoS for its own thread and recorded both process CPU time and wall time. It also amortized timer checks across batches at small sizes. The ratios below divide average standard-map CPU time per lifecycle by average bbtree CPU time per lifecycle. They use a different metric from the earlier core table.

The lifecycle includes construction, the modeled map operations, and destruction. Cleanup includes key and value allocation. The buffered-write case includes allocation and destruction of 4 KiB buffers, fixed 8 KiB read overlays, and write grouping. It does not model every adapter operation or a mounted FUSE workload. The complete operation mixes and 95% paired bootstrap intervals are in the [scaling report](../benchmarks/bbtree/runs/size-sweep/report.md).

For ordered input, the averages were:

| Entries | Data index | Assignments | Cleanup updates | Buffered writes |
|---:|---:|---:|---:|---:|
| 1 | 0.159× | 0.190× | 0.655× | 0.748× |
| 4 | 0.299× | 0.432× | 0.802× | 0.843× |
| 32 | 0.854× | 1.134× | 1.088× | 1.016× |
| 256 | 1.118× | 1.346× | 1.317× | 1.164× |
| 1,024 | 2.493× | 2.278× | 1.350× | 1.230× |
| 4,096 | 2.469× | 2.657× | 1.462× | 1.013× |

This gives a useful crossover range for each modeled lifecycle. It does not give a universal minimum size for choosing bbtree. Shuffled input changed the result substantially. At 256 entries, the shuffled data-index case reached 0.550×, and shuffled assignments reached 0.795×. At 1,024 entries, both favored bbtree again. The curves were not monotonic.

Key order also has a concrete connection to the code. Assignment IDs come from `BTreeSet`. Initial collection and later extension each consume sorted IDs. The ordered simulation follows that structure using consecutive halves of the IDs, rather than an exact recorded trace. The shuffled assignment case is stress testing. Data-index and cleanup insertion order can vary with allocation and transaction history.

I continued each case until two size increases failed to produce another cumulative 5% gain over its last material best speedup. Every case got at least seven size points. This is a local plateau rule with a practical tolerance; it cannot prove that no larger size will cross another cache or allocator boundary.

| Ordered lifecycle | Best observed average | Size at that result | Largest tested size |
|---|---:|---:|---:|
| Data index | 2.493× | 1,024 | 16,384 |
| Assignments | 2.657× | 4,096 | 65,536 |
| Cleanup updates | 1.510× | 16,384 | 65,536 |
| Buffered writes | 1.230× | 1,024 | 16,384 |

The shuffled cleanup case took longest to flatten. Its average gain rose from 1.597× at 1,048,576 entries to 1.696× at 2,097,152, then 1.761× at 4,194,304 and 1.774× at 6,291,456. All eight map and key-order combinations eventually met the declared rule. Selecting the largest observed ratio can overstate the true peak, so the report retains the intervals and every sample.

The largest case reached about 1.47 GiB of peak process RSS. That includes the process and allocator caches, rather than a measurement of live map storage alone. These large cases are scaling tests. Current accepted plans cap the data index and final assignment count at 4,096. For the simulated 4 KiB writes, the adapter's normal 4 MiB buffering threshold corresponds to 1,024 entries. The calibration never produced a cleanup map remotely close to six million entries.

The small-map regressions and large-map gains can both be real. Rust's [BTreeMap documentation](https://doc.rust-lang.org/std/collections/struct.BTreeMap.html#background) describes its cache-efficiency tradeoff and good performance for cheap comparisons in small nodes. Our benchmark measures the combined cost of allocation, searches, mutation, and destruction. It does not separate those costs, so assigning bbtree's one-entry regression to a particular internal mechanism would require profiling.

A container-level gain also has a limited share of runtime to improve. As a hypothetical example, if map work occupies 5% of an operation, making that work 2.5 times faster reduces total time by 3%. We did not measure the map CPU share in a mounted workload. The small core-level changes therefore remain a separate result from the large-map speedups.

For CarryFS, I would retain the standard maps. The measured occupancy distributions often put them below bbtree's strongest range, and the core comparison did not establish a compelling overall improvement. A future profile showing a large ordered map consuming meaningful CPU time would justify testing that particular replacement. Increasing transaction sizes solely to make a container benchmark look better would change memory and publication behavior without establishing a filesystem benefit.

The [saved experiment](../benchmarks/bbtree/README.md) includes raw samples, diagnostics, the candidate diff, source snapshots, test logs, and offline reproduction commands. Output checks and reopened-image structural checks passed in the core runs, along with 37 focused tree and allocation tests. The scaling pairs produced matching checksums, and their counts, means, and stopping decisions passed verification. Mounted FUSE, WinFsp, other CPU architectures, and call-stack or allocation profiles remain outside this experiment.
