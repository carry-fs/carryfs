# Linux stable measurements

Ratios divide std time by bbtree time; above 1 favors bbtree. Each configuration has 30 alternating A/B pairs in three separately ordered blocks. Core and file use wall time per workload; maps use process CPU time per lifecycle. No samples are discarded.

## core

| Workload | Entries/count | Mode/order | std mean µs | bb mean µs | Mean ratio | Median paired | 95% interval | std CV | bb CV | Block mean ratios |
|---|---:|---|---:|---:|---:|---:|---|---:|---:|---|
| churn | 4000 | false | 1084569.396 | 1051765.524 | 1.031× | 1.042× | 1.016–1.047 | 4.8% | 4.4% | 1.004, 1.022, 1.068 |
| churn | 4000 | true | 169796.681 | 169250.002 | 1.003× | 1.002× | 0.978–1.028 | 5.3% | 6.0% | 1.019, 0.999, 0.993 |
| create | 20000 | false | 1975582.200 | 1929619.704 | 1.024× | 1.040× | 1.004–1.043 | 4.4% | 4.0% | 1.034, 1.018, 1.019 |
| create | 20000 | true | 200447.764 | 198876.539 | 1.008× | 0.999× | 0.989–1.028 | 4.1% | 5.4% | 1.017, 0.992, 1.015 |
| large-write | 64 | false | 61481.259 | 61843.824 | 0.994× | 1.017× | 0.970–1.016 | 6.4% | 6.8% | 1.006, 0.993, 0.983 |
| large-write | 512 | true | 301791.730 | 296938.657 | 1.016× | 1.012× | 0.998–1.036 | 4.8% | 6.2% | 1.011, 1.018, 1.020 |
| partial-overwrite | 32 | false | 34511.306 | 34448.619 | 1.002× | 1.008× | 0.964–1.041 | 8.7% | 8.8% | 1.011, 1.001, 0.994 |
| partial-overwrite | 256 | true | 259074.566 | 259976.358 | 0.997× | 0.999× | 0.980–1.012 | 5.3% | 4.8% | 0.976, 0.996, 1.020 |
| small-write | 4000 | false | 704555.418 | 697748.289 | 1.010× | 1.027× | 0.987–1.032 | 3.8% | 5.3% | 1.011, 1.020, 0.999 |
| small-write | 4000 | true | 121189.798 | 120723.392 | 1.004× | 0.989× | 0.981–1.029 | 6.2% | 6.2% | 1.010, 0.981, 1.021 |

## file

| Workload | Entries/count | Mode/order | std mean µs | bb mean µs | Mean ratio | Median paired | 95% interval | std CV | bb CV | Block mean ratios |
|---|---:|---|---:|---:|---:|---:|---|---:|---:|---|
| churn | 100 | true | 8985.542 | 9484.888 | 0.947× | 0.952× | 0.900–0.998 | 12.5% | 18.9% | 0.917, 1.002, 0.932 |
| create | 300 | true | 5737.432 | 6313.836 | 0.909× | 1.019× | 0.732–1.119 | 31.6% | 61.1% | 1.208, 0.968, 0.649 |
| large-write | 32 | true | 93417.585 | 91787.893 | 1.018× | 1.020× | 0.997–1.040 | 6.8% | 4.6% | 1.031, 1.022, 1.000 |
| small-write | 100 | true | 6496.359 | 6688.231 | 0.971× | 0.958× | 0.911–1.039 | 14.7% | 15.7% | 0.944, 1.011, 0.961 |

## maps

| Workload | Entries/count | Mode/order | std mean µs | bb mean µs | Mean ratio | Median paired | 95% interval | std CV | bb CV | Block mean ratios |
|---|---:|---|---:|---:|---:|---:|---|---:|---:|---|
| assignments | 1 | ordered | 0.037 | 0.163 | 0.226× | 0.229× | 0.219–0.233 | 9.0% | 9.5% | 0.224, 0.226, 0.227 |
| assignments | 32 | ordered | 0.868 | 0.978 | 0.887× | 0.894× | 0.859–0.914 | 7.2% | 10.1% | 0.879, 0.920, 0.867 |
| assignments | 256 | ordered | 10.704 | 9.221 | 1.161× | 1.149× | 1.139–1.185 | 6.2% | 6.8% | 1.135, 1.191, 1.158 |
| assignments | 1024 | ordered | 55.718 | 42.166 | 1.321× | 1.274× | 1.272–1.375 | 10.4% | 7.2% | 1.252, 1.386, 1.330 |
| assignments | 4096 | ordered | 336.798 | 187.433 | 1.797× | 1.804× | 1.755–1.839 | 4.4% | 7.5% | 1.773, 1.819, 1.799 |
| assignments | 256 | shuffled | 12.847 | 15.135 | 0.849× | 0.831× | 0.826–0.875 | 7.5% | 4.3% | 0.864, 0.874, 0.810 |
| assignments | 1024 | shuffled | 80.114 | 72.015 | 1.112× | 1.087× | 1.083–1.146 | 9.3% | 4.8% | 1.107, 1.144, 1.086 |
| data_index | 1 | ordered | 0.048 | 0.282 | 0.170× | 0.168× | 0.165–0.175 | 7.5% | 8.0% | 0.163, 0.179, 0.168 |
| data_index | 32 | ordered | 1.345 | 2.053 | 0.655× | 0.647× | 0.623–0.688 | 10.6% | 10.0% | 0.650, 0.673, 0.644 |
| data_index | 256 | ordered | 16.318 | 19.080 | 0.855× | 0.870× | 0.826–0.886 | 8.6% | 8.0% | 0.842, 0.877, 0.848 |
| data_index | 1024 | ordered | 117.119 | 78.390 | 1.494× | 1.530× | 1.446–1.538 | 5.5% | 9.7% | 1.572, 1.462, 1.453 |
| data_index | 4096 | ordered | 544.815 | 363.483 | 1.499× | 1.514× | 1.455–1.545 | 6.7% | 4.7% | 1.519, 1.486, 1.492 |
| data_index | 256 | shuffled | 16.733 | 26.918 | 0.622× | 0.615× | 0.605–0.639 | 9.3% | 6.5% | 0.648, 0.615, 0.600 |
| data_index | 1024 | shuffled | 124.914 | 120.041 | 1.041× | 1.043× | 1.016–1.065 | 5.6% | 5.1% | 1.060, 1.016, 1.045 |
| pending | 1 | ordered | 0.351 | 0.562 | 0.625× | 0.618× | 0.603–0.650 | 8.5% | 6.0% | 0.603, 0.645, 0.630 |
| pending | 32 | ordered | 5.343 | 6.012 | 0.889× | 0.891× | 0.856–0.922 | 9.9% | 10.2% | 0.873, 0.871, 0.926 |
| pending | 256 | ordered | 56.611 | 49.800 | 1.137× | 1.152× | 1.112–1.159 | 5.9% | 5.3% | 1.142, 1.162, 1.105 |
| pending | 1024 | ordered | 260.901 | 2048.656 | 0.127× | 0.128× | 0.124–0.131 | 7.2% | 8.1% | 0.123, 0.129, 0.130 |
| pending | 4096 | ordered | 9947.919 | 3430.120 | 2.900× | 2.916× | 2.792–2.998 | 5.9% | 8.7% | 2.794, 2.874, 3.040 |
| pending | 256 | shuffled | 62.967 | 61.685 | 1.021× | 1.037× | 1.000–1.042 | 5.1% | 5.0% | 1.058, 1.014, 0.991 |
| pending | 1024 | shuffled | 305.122 | 347.292 | 0.879× | 0.859× | 0.857–0.903 | 6.4% | 3.6% | 0.876, 0.910, 0.850 |
| updates | 1 | ordered | 0.152 | 0.342 | 0.444× | 0.451× | 0.429–0.458 | 8.1% | 9.3% | 0.433, 0.445, 0.454 |
| updates | 32 | ordered | 5.913 | 5.605 | 1.055× | 1.068× | 1.024–1.087 | 8.6% | 8.9% | 1.057, 1.069, 1.040 |
| updates | 256 | ordered | 74.723 | 57.398 | 1.302× | 1.306× | 1.264–1.340 | 6.7% | 8.8% | 1.327, 1.286, 1.292 |
| updates | 1024 | ordered | 342.456 | 257.389 | 1.330× | 1.347× | 1.289–1.374 | 6.3% | 9.0% | 1.427, 1.283, 1.291 |
| updates | 4096 | ordered | 1502.742 | 1001.388 | 1.501× | 1.482× | 1.456–1.554 | 8.3% | 6.1% | 1.550, 1.515, 1.437 |
| updates | 256 | shuffled | 78.096 | 71.601 | 1.091× | 1.094× | 1.055–1.123 | 5.6% | 7.4% | 1.091, 1.106, 1.076 |
| updates | 1024 | shuffled | 360.473 | 317.768 | 1.134× | 1.149× | 1.104–1.167 | 5.4% | 8.1% | 1.134, 1.158, 1.111 |

Intervals use 5,000 paired bootstrap resamples with a fixed seed. They assume independent pairs and describe sampling variation within this experiment, not all VM states or allocator histories. The three block means expose time drift. CV is sample standard deviation divided by the mean. CPU timing excludes descheduling, but host contention can still affect CPU work.

## Controlled probes

Policy is allocator setting for isolated and multi-case runs, compiler for compiler controls, and whether bbtree batches 64 lookups for lookup probes. The std batched-loop control still makes scalar gets. Lookup setup and destruction are outside the timer; map lifecycle probes include both.

| Probe | Policy | Workload | Entries | Order | Pairs | Mean ratio | 95% interval | std CV | bb CV |
|---|---|---|---:|---|---:|---:|---|---:|---:|
| isolated | default | pending | 1024 | ordered | 10 | 0.131× | 0.125–0.138 | 8.1% | 4.8% |
| multi-case | default | pending | 1024 | ordered | 5 | 0.967× | 0.889–1.030 | 12.7% | 6.6% |
| multi-case | default | pending | 1024 | shuffled | 5 | 7.193× | 6.465–7.864 | 4.3% | 11.7% |
| isolated | no-trim | pending | 1024 | ordered | 10 | 1.440× | 1.382–1.505 | 7.1% | 5.8% |
| multi-case | no-trim | pending | 1024 | ordered | 5 | 1.254× | 1.192–1.355 | 6.4% | 2.9% |
| multi-case | no-trim | pending | 1024 | shuffled | 5 | 0.863× | 0.842–0.881 | 2.7% | 4.9% |
| compiler-control | stable | data_index | 1 | ordered | 10 | 0.171× | 0.159–0.184 | 8.0% | 12.0% |
| compiler-control | nightly | data_index | 1 | ordered | 10 | 0.204× | 0.196–0.211 | 10.4% | 9.8% |
| compiler-control | stable | data_index | 1024 | ordered | 10 | 1.492× | 1.438–1.542 | 4.2% | 5.7% |
| compiler-control | nightly | data_index | 1024 | ordered | 10 | 1.432× | 1.354–1.503 | 3.4% | 7.7% |
| compiler-control | stable | pending | 1024 | ordered | 10 | 0.133× | 0.126–0.142 | 9.2% | 4.6% |
| compiler-control | nightly | pending | 1024 | ordered | 10 | 0.128× | 0.126–0.130 | 3.2% | 4.3% |
| steady-lookup | false | lookup | 1024 | 50% hits | 10 | 0.471× | 0.451–0.492 | 6.2% | 8.2% |
| steady-lookup | true | lookup | 1024 | 50% hits | 10 | 0.527× | 0.509–0.543 | 6.5% | 3.8% |
| steady-lookup | false | lookup | 1048576 | 50% hits | 10 | 1.252× | 1.167–1.345 | 10.5% | 6.6% |
| steady-lookup | true | lookup | 1048576 | 50% hits | 10 | 2.085× | 1.874–2.304 | 17.0% | 6.8% |
| isolated | default | pending | 4096 | ordered | 10 | 3.072× | 2.926–3.219 | 3.4% | 7.2% |
| isolated | no-trim | pending | 4096 | ordered | 10 | 1.049× | 0.945–1.171 | 10.4% | 19.2% |

## Allocation counts

One complete lifecycle with counting enabled; input keys are allocated before counting. Requested bytes sum allocation and reallocation requests, so they are not peak live memory or RSS. Aligned means requested alignment at least 512 bytes. These instrumented binaries do not produce the primary timings.

| Workload | Entries | Variant | Allocation/reallocation calls | Requested bytes | Zeroed calls | Aligned calls |
|---|---:|---|---:|---:|---:|---:|
| data_index | 1 | std | 1 | 192 | 0 | 0 |
| data_index | 1024 | std | 171 | 35232 | 0 | 0 |
| assignments | 1 | std | 1 | 192 | 0 | 0 |
| assignments | 1024 | std | 135 | 43552 | 0 | 0 |
| updates | 1 | std | 8 | 632 | 1 | 0 |
| updates | 1024 | std | 5035 | 164032 | 5 | 0 |
| pending | 1 | std | 5 | 12840 | 2 | 0 |
| pending | 1024 | std | 1229 | 4316848 | 6 | 0 |
| data_index | 1 | bb | 1 | 1089 | 1 | 1 |
| data_index | 1024 | bb | 25 | 25665 | 25 | 25 |
| assignments | 1 | bb | 1 | 1089 | 1 | 1 |
| assignments | 1024 | bb | 22 | 36929 | 20 | 20 |
| updates | 1 | bb | 8 | 1177 | 2 | 1 |
| updates | 1024 | bb | 4982 | 187457 | 123 | 118 |
| pending | 1 | bb | 5 | 13561 | 3 | 1 |
| pending | 1024 | bb | 1108 | 4302785 | 56 | 50 |

## Buffered-write phases

These instrumented wall-time probes split insertion and buffer allocation, read overlays, map consumption and grouping, and final checksum plus buffer destruction. Extra timers and the counting allocator change overhead; use the uninstrumented primary samples for speedup claims.

| Policy | Entries | Variant | Build µs | Overlay µs | Group µs | Checksum/drop µs |
|---|---:|---|---:|---:|---:|---:|
| default | 256 | std | 44.145 | 3.972 | 3.931 | 5.073 |
| default | 256 | bb | 36.933 | 4.912 | 3.040 | 5.291 |
| default | 1024 | std | 202.563 | 18.020 | 17.528 | 20.866 |
| default | 1024 | bb | 1730.291 | 31.483 | 29.649 | 269.690 |
| default | 4096 | std | 8421.995 | 299.096 | 241.133 | 1341.388 |
| default | 4096 | bb | 2846.240 | 187.784 | 142.990 | 349.143 |
| no-trim | 256 | std | 47.048 | 4.189 | 4.213 | 5.325 |
| no-trim | 256 | bb | 38.218 | 5.066 | 3.029 | 5.300 |
| no-trim | 1024 | std | 208.734 | 18.913 | 18.449 | 21.937 |
| no-trim | 1024 | bb | 124.658 | 19.881 | 11.848 | 21.140 |
| no-trim | 4096 | std | 1256.791 | 141.748 | 90.963 | 109.765 |
| no-trim | 4096 | bb | 1180.893 | 143.951 | 60.193 | 100.433 |

## User and system CPU time

These separate process measurements include setup, warmup, and a 500 ms measurement window at 1,024 ordered pending writes. They locate CPU work rather than estimate a speedup.

| Policy | Variant | User seconds | System seconds | System fraction |
|---|---|---:|---:|---:|
| default | std | 0.5907 | 0.0060 | 1.0% |
| default | bb | 0.1069 | 0.4968 | 82.3% |
| no-trim | std | 0.5996 | 0.0050 | 0.8% |
| no-trim | bb | 0.6001 | 0.0050 | 0.8% |
