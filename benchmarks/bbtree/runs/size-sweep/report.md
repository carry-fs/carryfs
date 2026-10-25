CarryFS map scaling with bbtree

The completed sweep covers 67 configurations and 938 measured process runs. 8 of 8 map and key-order combinations met the declared local plateau rule. Ordered data indexes peaked at 2.493x, assignments at 2.657x, and pending writes at 1.230x. Shuffled cleanup updates changed from 1.761x at 4,194,304 entries to 1.774x at 6,291,456. Peak process RSS was 1.47 GiB. Maps with one to four entries favored std in every model.

This experiment measures complete in-memory map lifecycles using CarryFS key and value shapes. It does not measure mounted filesystem throughput.

A separate calibration build traced the maps during five real Volume API workloads over a 256 MiB RAM block device. These occupancies describe those benchmark workloads, rather than a live mount or every possible user workload. Empty observations are retained in the calibration JSON. The percentiles below use nonempty maps only.

| Calibration workload | Map | Nonempty observations | Median entries | 90th percentile | Maximum |
|---|---|---:|---:|---:|---:|
| create | assignments | 12 | 2 | 170 | 1,033 |
| small-write | data_index | 4,000 | 1 | 1 | 1 |
| small-write | assignments | 5 | 3 | 564 | 564 |
| churn | data_index | 4,000 | 1 | 1 | 1 |
| churn | assignments | 117 | 4 | 9 | 564 |
| churn | updates | 64 | 32 | 32 | 32 |
| large-write | data_index | 512 | 256 | 256 | 256 |
| large-write | assignments | 310 | 4 | 5 | 7 |
| large-write | updates | 248 | 32 | 32 | 32 |
| partial-overwrite | data_index | 512 | 16 | 256 | 256 |
| partial-overwrite | assignments | 187 | 4 | 8 | 11 |
| partial-overwrite | updates | 139 | 32 | 32 | 32 |

The size sweep starts at 1, 4, 32, 256, 1,024, and 4,096 entries. Active cases then grow fourfold through 1,048,576 entries. The remaining active case then grows to 2,097,152, 4,194,304, and 6,291,456 entries. Each core map gets at least six size points, and the pending-write map gets at least five through 1,024 entries. A case stops after two successive size increases fail to produce a cumulative 5% gain over its last material best speedup. This is a local plateau under the tested sizes and operation mix. A later cache or allocator transition could change the result.

Each point has seven sequential A/B pairs, with variant order alternating by pair. Every process warms for 100 ms, then measures at least one second of wall time. Timer checks are amortized over batches at small sizes. Raw results retain all samples, operation counts, checksums, CPU time, and wall time.

The primary ratio is average std process-CPU nanoseconds per lifecycle divided by average bbtree process-CPU nanoseconds per lifecycle. A ratio above 1 favors bbtree. Intervals are 95% paired bootstrap intervals over the seven repetitions, using 6,000 resamples and a fixed seed. They describe each tested point, rather than a global confidence guarantee for the adaptive stopping procedure. Median paired ratios, coefficients of variation, CPU utilization, and wall-time ratios are also in the summary JSON.

The host showed scheduling noise during initial diagnostics. The final harness records macOS process CPU time with clock_gettime and requests user-initiated QoS for its own benchmark thread. Both variants receive the same treatment. CPU time excludes time waiting to run but still depends on CPU frequency and core selection. The earlier timer and wall-time diagnostic probes are retained separately and excluded from this final sweep.

The source is the same pinned bbtree 0.1.5 commit 8adae97a0ed97e5c98fa2790312eb0aec9719846 used in the prior comparison. The host is Apple M1 with 16 GB RAM, macOS 15.6, Rust 1.99.0, release optimization. No benchmark, build, or calibration run executes concurrently with these samples.

The simulated operations are unchanged from the prior comparison. Data indexes use duplicate checks, inserts, range scans, removals, and destruction. Assignments use ordered collection, extension, existence checks, iteration, and lookups. Cleanup updates use 17-byte keys and optional 32-byte values, including allocation and replacement. Pending writes use 4 KiB Vec buffers and the actual adapter overlay and run-grouping functions, including buffer allocation and destruction. Fixed 8 KiB overlay windows are one modeled read shape.

Assignment IDs come from BTreeSet. Initial collection and later extension each consume sorted IDs. The ordered simulation models that structure, but uses consecutive halves rather than a recorded ID trace. The shuffled assignment case is stress testing. Data-index and cleanup insertion orders depend on allocation and transaction history, so both orders are retained.

For data indexes and final assignments, sizes above 4,096 exceed the current accepted-plan bound. The calibration's largest data index was only 256 entries. Pending writes above 1,024 entries exceed the normal 4 MiB threshold for the simulated 4 KiB writes. Values beyond those bounds test scaling, rather than an expected current CarryFS workload. The pending-write stress test has a 65,536-entry memory cap. The sweep was extended to 6,291,456 entries because shuffled cleanup updates still improved at the original million-entry limit. A separate memory probe at 1,048,576 entries observed peak process RSS of about 156 MiB for std and 255 MiB for bbtree. The four-million-entry run used about 1.0 GiB peak process RSS. The final six-million-entry cap was selected to keep expected peak process memory within the 1.5 GiB budget on this loaded host. Peak RSS includes the whole benchmark process and allocator caches. A size cap is reported separately if the local plateau was not established.

The following table gives the average CPU speedup at every measured size. A dash means that case had already stopped or reached its memory cap.

| Entries | Data ordered | Data shuffled | Assignments ordered | Assignments shuffled | Updates ordered | Updates shuffled | Pending ordered | Pending shuffled |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.159x | 0.157x | 0.190x | 0.190x | 0.655x | 0.669x | 0.748x | 0.742x |
| 4 | 0.299x | 0.330x | 0.432x | 0.437x | 0.802x | 0.796x | 0.843x | 0.851x |
| 32 | 0.854x | 0.712x | 1.134x | 1.116x | 1.088x | 0.919x | 1.016x | 0.895x |
| 256 | 1.118x | 0.550x | 1.346x | 0.795x | 1.317x | 1.082x | 1.164x | 0.936x |
| 1,024 | 2.493x | 1.095x | 2.278x | 1.603x | 1.350x | 1.121x | 1.230x | 0.954x |
| 4,096 | 2.469x | 1.205x | 2.657x | 1.771x | 1.462x | 1.218x | 1.013x | 0.995x |
| 16,384 | 2.474x | 1.367x | 2.260x | 1.468x | 1.510x | 1.295x | 1.024x | 0.951x |
| 65,536 | - | 1.420x | 2.108x | 1.526x | 1.483x | 1.211x | - | - |
| 262,144 | - | 1.323x | - | - | - | 1.370x | - | - |
| 1,048,576 | - | - | - | - | - | 1.597x | - | - |
| 2,097,152 | - | - | - | - | - | 1.696x | - | - |
| 4,194,304 | - | - | - | - | - | 1.761x | - | - |
| 6,291,456 | - | - | - | - | - | 1.774x | - | - |

This summary identifies the first tested size whose entire interval favors bbtree, the best observed average, and the largest tested size. Intermediate crossover sizes were not tested. Some curves drop below 1 again at larger sizes, so the first tested win applies to that point only. Selecting the largest observed ratio can overestimate the true peak.

| Map | Order | First tested win with interval above 1 | Best observed average | Largest tested size | Stop reason |
|---|---|---:|---|---:|---|
| data_index | ordered | 256 | 2.493x at 1,024 | 16,384 | two size increases without a 5% gain over the last material best |
| data_index | shuffled | 1024 | 1.420x at 65,536 | 262,144 | two size increases without a 5% gain over the last material best |
| assignments | ordered | 32 | 2.657x at 4,096 | 65,536 | two size increases without a 5% gain over the last material best |
| assignments | shuffled | 32 | 1.771x at 4,096 | 65,536 | two size increases without a 5% gain over the last material best |
| updates | ordered | 32 | 1.510x at 16,384 | 65,536 | two size increases without a 5% gain over the last material best |
| updates | shuffled | 256 | 1.774x at 6,291,456 | 6,291,456 | two size increases without a 5% gain over the last material best |
| pending | ordered | 256 | 1.230x at 1,024 | 16,384 | two size increases without a 5% gain over the last material best |
| pending | shuffled | none | 0.995x at 4,096 | 16,384 | two size increases without a 5% gain over the last material best |

Every measured point follows, including uncertainty and average latency. These are microbenchmarks of whole lifecycles, so nanoseconds per lifecycle increase with map size.

| Map | Order | Entries | std CPU, microseconds | bbtree CPU, microseconds | Average speedup | 95% interval | Wall-time speedup |
|---|---|---:|---:|---:|---:|---|---:|
| data_index | ordered | 1 | 0.034 | 0.214 | 0.159x | 0.157 to 0.161x | 0.158x |
| data_index | shuffled | 1 | 0.034 | 0.217 | 0.157x | 0.152 to 0.161x | 0.152x |
| assignments | ordered | 1 | 0.027 | 0.140 | 0.190x | 0.181 to 0.195x | 0.182x |
| assignments | shuffled | 1 | 0.026 | 0.139 | 0.190x | 0.183 to 0.195x | 0.185x |
| updates | ordered | 1 | 0.203 | 0.310 | 0.655x | 0.653 to 0.657x | 0.655x |
| updates | shuffled | 1 | 0.208 | 0.311 | 0.669x | 0.656 to 0.691x | 0.695x |
| pending | ordered | 1 | 0.346 | 0.462 | 0.748x | 0.744 to 0.751x | 0.751x |
| pending | shuffled | 1 | 0.344 | 0.463 | 0.742x | 0.739 to 0.745x | 0.741x |
| data_index | ordered | 4 | 0.082 | 0.275 | 0.299x | 0.292 to 0.304x | 0.289x |
| data_index | shuffled | 4 | 0.099 | 0.301 | 0.330x | 0.307 to 0.357x | 0.337x |
| assignments | ordered | 4 | 0.076 | 0.175 | 0.432x | 0.423 to 0.444x | 0.437x |
| assignments | shuffled | 4 | 0.081 | 0.185 | 0.437x | 0.434 to 0.439x | 0.432x |
| updates | ordered | 4 | 0.518 | 0.646 | 0.802x | 0.784 to 0.813x | 0.799x |
| updates | shuffled | 4 | 0.519 | 0.652 | 0.796x | 0.792 to 0.800x | 0.796x |
| pending | ordered | 4 | 0.759 | 0.901 | 0.843x | 0.838 to 0.848x | 0.844x |
| pending | shuffled | 4 | 0.781 | 0.917 | 0.851x | 0.831 to 0.868x | 0.837x |
| data_index | ordered | 32 | 0.867 | 1.015 | 0.854x | 0.849 to 0.860x | 0.857x |
| data_index | shuffled | 32 | 0.935 | 1.314 | 0.712x | 0.710 to 0.714x | 0.713x |
| assignments | ordered | 32 | 0.631 | 0.557 | 1.134x | 1.129 to 1.140x | 1.136x |
| assignments | shuffled | 32 | 0.789 | 0.707 | 1.116x | 1.113 to 1.119x | 1.114x |
| updates | ordered | 32 | 4.969 | 4.566 | 1.088x | 1.083 to 1.094x | 1.089x |
| updates | shuffled | 32 | 5.247 | 5.711 | 0.919x | 0.912 to 0.929x | 0.919x |
| pending | ordered | 32 | 5.135 | 5.054 | 1.016x | 0.991 to 1.039x | 1.011x |
| pending | shuffled | 32 | 5.027 | 5.617 | 0.895x | 0.891 to 0.899x | 0.895x |
| data_index | ordered | 256 | 10.185 | 9.109 | 1.118x | 1.111 to 1.126x | 1.118x |
| data_index | shuffled | 256 | 10.628 | 19.338 | 0.550x | 0.537 to 0.567x | 0.547x |
| assignments | ordered | 256 | 6.971 | 5.177 | 1.346x | 1.342 to 1.353x | 1.348x |
| assignments | shuffled | 256 | 7.952 | 10.002 | 0.795x | 0.792 to 0.798x | 0.806x |
| updates | ordered | 256 | 49.723 | 37.764 | 1.317x | 1.312 to 1.321x | 1.317x |
| updates | shuffled | 256 | 53.637 | 49.581 | 1.082x | 1.077 to 1.089x | 1.085x |
| pending | ordered | 256 | 44.141 | 37.934 | 1.164x | 1.156 to 1.171x | 1.158x |
| pending | shuffled | 256 | 47.169 | 50.416 | 0.936x | 0.922 to 0.944x | 0.936x |
| data_index | ordered | 1,024 | 91.672 | 36.775 | 2.493x | 2.469 to 2.511x | 2.494x |
| data_index | shuffled | 1,024 | 95.599 | 87.283 | 1.095x | 1.073 to 1.120x | 1.097x |
| assignments | ordered | 1,024 | 54.259 | 23.820 | 2.278x | 2.265 to 2.290x | 2.306x |
| assignments | shuffled | 1,024 | 71.426 | 44.546 | 1.603x | 1.586 to 1.621x | 1.604x |
| updates | ordered | 1,024 | 219.914 | 162.851 | 1.350x | 1.342 to 1.359x | 1.365x |
| updates | shuffled | 1,024 | 244.462 | 218.035 | 1.121x | 1.105 to 1.133x | 1.125x |
| pending | ordered | 1,024 | 183.668 | 149.295 | 1.230x | 1.224 to 1.237x | 1.235x |
| pending | shuffled | 1,024 | 200.258 | 209.903 | 0.954x | 0.938 to 0.965x | 0.949x |
| data_index | ordered | 4,096 | 448.821 | 181.816 | 2.469x | 2.456 to 2.479x | 2.470x |
| data_index | shuffled | 4,096 | 554.761 | 460.435 | 1.205x | 1.198 to 1.212x | 1.205x |
| assignments | ordered | 4,096 | 306.200 | 115.248 | 2.657x | 2.479 to 2.847x | 2.923x |
| assignments | shuffled | 4,096 | 401.076 | 226.494 | 1.771x | 1.746 to 1.791x | 1.791x |
| updates | ordered | 4,096 | 950.443 | 649.946 | 1.462x | 1.453 to 1.471x | 1.467x |
| updates | shuffled | 4,096 | 1132.781 | 929.709 | 1.218x | 1.203 to 1.231x | 1.215x |
| pending | ordered | 4,096 | 2482.754 | 2452.087 | 1.013x | 0.988 to 1.037x | 1.011x |
| pending | shuffled | 4,096 | 2983.563 | 2997.828 | 0.995x | 0.972 to 1.021x | 0.994x |
| data_index | ordered | 16,384 | 1919.041 | 775.649 | 2.474x | 2.466 to 2.480x | 2.471x |
| data_index | shuffled | 16,384 | 2770.106 | 2026.267 | 1.367x | 1.363 to 1.374x | 1.368x |
| assignments | ordered | 16,384 | 1190.862 | 526.870 | 2.260x | 2.257 to 2.263x | 2.260x |
| assignments | shuffled | 16,384 | 1935.181 | 1318.167 | 1.468x | 1.458 to 1.479x | 1.478x |
| updates | ordered | 16,384 | 4093.178 | 2710.971 | 1.510x | 1.504 to 1.517x | 1.511x |
| updates | shuffled | 16,384 | 5111.425 | 3946.087 | 1.295x | 1.293 to 1.297x | 1.301x |
| pending | ordered | 16,384 | 10546.309 | 10301.227 | 1.024x | 1.011 to 1.038x | 1.031x |
| pending | shuffled | 16,384 | 13587.708 | 14292.577 | 0.951x | 0.892 to 0.993x | 0.930x |
| data_index | shuffled | 65,536 | 12640.948 | 8902.933 | 1.420x | 1.412 to 1.427x | 1.417x |
| assignments | ordered | 65,536 | 5137.989 | 2437.769 | 2.108x | 2.099 to 2.116x | 2.108x |
| assignments | shuffled | 65,536 | 8652.494 | 5670.617 | 1.526x | 1.509 to 1.538x | 1.522x |
| updates | ordered | 65,536 | 18024.360 | 12152.037 | 1.483x | 1.476 to 1.492x | 1.481x |
| updates | shuffled | 65,536 | 24309.589 | 20066.054 | 1.211x | 1.201 to 1.225x | 1.211x |
| data_index | shuffled | 262,144 | 58767.990 | 44413.355 | 1.323x | 1.302 to 1.352x | 1.324x |
| updates | shuffled | 262,144 | 166199.949 | 121281.635 | 1.370x | 1.355 to 1.383x | 1.371x |
| updates | shuffled | 1,048,576 | 1032127.714 | 646266.786 | 1.597x | 1.585 to 1.608x | 1.597x |
| updates | shuffled | 2,097,152 | 2413029.429 | 1422589.714 | 1.696x | 1.688 to 1.705x | 1.697x |
| updates | shuffled | 4,194,304 | 5556048.286 | 3155405.857 | 1.761x | 1.744 to 1.783x | 1.762x |
| updates | shuffled | 6,291,456 | 8874309.429 | 5001341.714 | 1.774x | 1.752 to 1.794x | 1.773x |

The original repository remains unchanged. All work is in this temporary experiment directory. The source snapshots and dependencies remain in its parent directory. Rebuild and rerun on this macOS host with:

```sh
python3 build.py
python3 sweep.py --millis 1000 --pairs 7
python3 report.py
python3 verify.py
```

`--resume` reuses complete recorded points and recalculates the same stopping decisions. It refuses partially completed points. The statistics and stopping-rule self-checks run before every sweep, and every benchmark iteration checks its result. Variant checksums must match for each point. Final verification also checks pair counts and durations.

[Calibration data](calibration-summary.json), [complete raw measurements](scaling-raw.tsv), [summary and confidence intervals](scaling-summary.json), [stop decisions](stop-state.json), [benchmark source](src/bin/maps.rs), and [runner](sweep.py) are retained for review.

These results do not establish a filesystem throughput improvement. Mounted FUSE, WinFsp, other platforms, and CPU or memory profiles were not measured in this sweep.
