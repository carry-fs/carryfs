# Offline recovery measurements

The recovery benchmark exercises diagnosis, new-image reconstruction, embedded
manifests, and optional destination data verification. It uses the production
release profile, one thread, and only the standard library. It installs nothing.

## Reproduce

Build and save each revision's benchmark and CLI before building the other:

```sh
cargo bench --locked --bench recovery --no-run
cargo build --release --locked -p carryfs-core --bin fsck-carryfs
```

Cargo prints the benchmark executable path. Copy that executable and
`target/release/fsck-carryfs` to separate baseline and candidate locations.
The baseline measured here is `7e466fa` with the same benchmark harness and
`[[bench]] name = "recovery", harness = false` registration copied into it.
No production code or release settings were changed in that baseline.

Generate shared CLI inputs with either benchmark binary. The directory may
exist, but each image filename must be new. These commands create seven
256 MiB images, including the sizes used to investigate scaling:

```sh
for n in 500 1000 2000; do
  taskset -c 2 BASELINE_BENCH --case orphan --files "$n" --rounds 5 --save build/recovery-inputs
done
for case in clean missing data sparse; do
  taskset -c 2 BASELINE_BENCH --case "$case" --files 1000 --mib 16 --rounds 5 --save build/recovery-inputs
done
python3 benchmarks/recovery.py \
  --baseline-bench BASELINE_BENCH --candidate-bench CANDIDATE_BENCH \
  --baseline-cli BASELINE_CLI --candidate-cli CANDIDATE_CLI \
  --inputs build/recovery-inputs --out build/recovery-comparison --cpu 2
taskset -c 2 CANDIDATE_BENCH --case bandwidth --rounds 10
```

Omit `--cpu` where `sched_setaffinity` is unavailable, and omit `taskset` on
platforms without it. The runner requires Python 3.11+, alternates baseline
and candidate five times, warms each case, and refuses an existing output
directory. It removes its completed CLI output images after validation.

Memory measurements exclude input setup and blank destination allocation.
They include diagnosis or recovery separately; recovery includes publishing
the manifests and validating the resulting filesystem. A RAM device counts
source reads, destination writes, and durable flush requests; it does not
simulate storage latency. Every run checks file counts, copied bytes, partial
status, verification results, and absence of source writes or flushes.

CLI measurements include process startup, diagnosis, sparse image creation,
sidecar reports, reconstruction, durability calls, and `--verify-data`. They
use the same pre-generated input on both sides. The runner checks expected
exit codes (1 for complete recovery, 4 for missing source data), parses the
manifests, compares all fields except the new destination UUID, and confirms
the source SHA-256 is unchanged. Compilation, input generation, hashing, JSON
comparison, and output deletion are outside the timed command.

The cases are 1,000 files of 4 KiB each, either clean, disconnected by removed
directory entries, or missing their checksum records; one dense 16 MiB file;
and one sparse file with four backed bytes at the 1 TiB offset. Damage is
checksummed and deterministic. Recovery must reconstruct orphan metadata or
report missing mappings rather than treating the source as empty.

## Results

Measured on Linux x86-64 under KVM, four virtual CPUs backed by an AMD EPYC
7543, 7.7 GiB RAM, Rust 1.99.1 (58cf8f9c3), Cargo 1.99.1. Release settings
were unchanged: fat LTO, one codegen unit, no extra RUSTFLAGS, default features,
one worker pinned to CPU 2. Other users ran builds and VMs during the final
comparison (load average approximately 5 on four CPUs). Alternating both sides
limits drift; the ranges below retain the observed interference. These are
warm-input measurements on this host, not physical-device throughput claims.

The initial 500/1,000/2,000-orphan study found copy medians of
303/1,015/3,547 ms without destination verification (five samples each).
`perf record`/`perf report` identified repeated page decoding, leaf construction,
checksum validation, and workspace checkpoint encoding. This matched the growth
in metadata work; diagnosis was a small fraction of the original total.

The shipped changes use existing mechanisms:

- Batch destination mutations through bounded deferred checkpoints after the
  initial manifest is durable. Final report publication and flushes remain.
- Retain the existing bounded metadata cache for immutable reconstructed source
  pages, and for destination data verification after a fresh structural check.
  Physical source metadata still receives fresh reads; file data still receives
  checksum validation. Final structural checks and manifest read-back remain.
- Resolve orphan-name collisions through the catalog index, including checking
  that the indexed entry actually survived normalization. Generated numeric
  names for different object IDs cannot collide with one another.
- Publish the intermediate `unverified` report only when verification is
  requested, avoiding a duplicate final report in the default path.

Verified recovery, excluding diagnosis, on the RAM device. All entries are
**median [minimum–maximum] milliseconds, five alternating samples per side**:


| Case | Before | After |
|---|---:|---:|
| 1,000 orphans | 1355.70 [1307.28–1625.58] | 45.60 [43.16–64.26] |
| 1,000 clean files | 1671.12 [1557.89–2254.26] | 868.13 [815.41–1345.61] |
| 1,000 missing checksum mappings | 640.60 [538.09–693.19] | 24.74 [18.14–27.55] |
| 16 MiB dense file | 34.66 [33.60–35.99] | 24.78 [21.63–29.07] |
| 1 TiB sparse file, four backed bytes | 3.33 [3.26–4.02] | 3.40 [3.07–3.83] |

End-to-end CLI with `--verify-data`, including diagnosis, sidecars, durability,
and process startup; same units and sample counts:


| Case | Before | After |
|---|---:|---:|
| 1,000 orphans | 8295.05 [6105.87–8384.61] | 115.37 [105.09–134.15] |
| 1,000 clean files | 8566.88 [7286.60–10335.65] | 1090.36 [1029.00–1117.39] |
| 1,000 missing checksum mappings | 4602.67 [4110.61–5363.20] | 93.87 [75.25–114.99] |
| 16 MiB dense file | 829.14 [744.59–889.95] | 111.11 [95.41–139.08] |
| 1 TiB sparse file, four backed bytes | 35.21 [30.95–40.14] | 25.77 [20.81–39.86] |

The 1,000-orphan case improves about 30× in RAM and 72× through the CLI.
The larger CLI improvement also reflects avoided durable flushes. RAM flush
requests drop from 12,048 to 24 with verification (18 without); destination
writes fall from 102,436,864 to 5,451,776 bytes. Copied source data remains
4,096,000 bytes. The sparse case has no measurable RAM speed change. Its CLI
median is lower, but the ranges overlap; do not treat that as an established win.

Raw samples: [RAM, both verification modes](recovery-memory.tsv) and
[CLI](recovery-cli.tsv). Every CLI case matched manifests across revisions
except for the destination UUID, returned the expected exit status, and left
the source hash unchanged. Every RAM run passed its correctness assertions.

For CPU profiling, use a separate run and exclude its timings from comparison:

```sh
perf record -o build/recovery-perf.data -F 499 --call-graph dwarf -- \
  taskset -c 2 CANDIDATE_BENCH --case orphan --files 2000 --rounds 10
perf report -i build/recovery-perf.data --stdio --no-children --call-graph none
```

An untimed `strace -c` run of verified 1,000-orphan CLI recovery counted
115,492 → 4,166 `pread64` calls, 25,009 → 1,331 `pwrite64` calls, and
12,056 → 32 `fsync` calls, including CLI reports. These counts explain the
end-to-end gain independently of wall-clock noise. One separate
`/usr/bin/time` sample measured peak RSS of 21,008 → 7,808 KiB; this is a
process high-water mark, not an allocation count. The metadata cache retains
its existing 4,096-node bound. No dependency, thread, unsafe operation, or
release setting was added.

## Floor and stopping boundary

Ten warmed copies of 64 MiB with `--case bandwidth` measured a median of
20.57 ms [18.82–22.30], approximately 3.26 GB/s of payload copying. Scaling
this measured bandwidth gives deliberately optimistic lower bounds of about
1.26 ms for 4,096,000 bytes and 5.14 ms for 16 MiB. These bounds exclude
metadata, allocation, checksumming, extra verification reads, and durability;
they are not predictions for a complete filesystem operation. Verified dense
recovery at 24.78 ms remains approximately 4.8× that copy-only bound. Verified
1,000-orphan recovery at 45.60 ms remains approximately 36× its bound.

After the changes, CPU samples spread across normal tree scans and writes,
memory copying/allocation, checksum work, and diagnosis. The original repeated
decoding and workspace encoding no longer dominate damaged-source copying.
A separate profile of 20 completed CLI recoveries showed checksum extension,
kernel copying, and ext4 writes among the largest individual samples, each
around 2% of CPU cycles; no new recovery routine dominated that profile.
Further tuning of that path would primarily change the shared tree writer,
checker, or data-copy contract rather than the new repair/recovery logic.
Those changes need separate workloads and validation of their wider effects.
Small serialization changes are not supported by these noisy timings; the
simpler serializer remains.

Clean-source recovery still reads physical metadata afresh. The next obvious
candidate would cache that metadata throughout copying; the clean/orphan RAM
gap suggests roughly 0.8 seconds of potential savings for 1,000 files on this
host. That estimate is an inference, not a measured implementation. It was
not taken because it would change detection of metadata faults appearing
after the initial read. Destination verification instead has fresh structural
checks before and after its exclusively held data pass. Durability barriers
and checksum validation were retained throughout.

Correctness checks passed: 22 repair tests, nine inspection tests, nine
offline CLI tests, all-target core Clippy with warnings denied, formatting,
and whitespace checks. The tests cover interruption, write/flush failure,
destination corruption, corrupt source bytes with cached reconstructed
metadata, orphan-name collisions, missing mappings, and source preservation.
No timing or platform-specific claim is made for macOS, Windows, FreeBSD,
other CPUs, failing physical drives, or inputs beyond the measured sizes.
