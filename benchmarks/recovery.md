# Offline recovery measurements

The recovery benchmark times diagnosis, new-image reconstruction, embedded
manifests and optional destination data verification. It uses the release
profile, one thread and the standard library only.

## Reproduce

Build the benchmark and the CLI for the baseline and the candidate revision,
and copy each pair to its own location:

```sh
cargo bench --locked --bench recovery --no-run
cargo build --release --locked -p carryfs-core --bin fsck-carryfs
```

Cargo prints the benchmark path. The baseline here is `7e466fa` with the same
benchmark harness and `[[bench]] name = "recovery", harness = false` entry
copied in. No production code changed in the baseline.

Generate the shared CLI inputs with either benchmark binary. Each image
filename must be new:

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

Drop `--cpu` where `sched_setaffinity` is missing and `taskset` where it is
missing. The runner needs Python 3.11+, alternates baseline and candidate five
times, warms each case and refuses an existing output directory. It writes the
raw samples to `memory.tsv` and `cli.tsv` in that directory.

The cases are 1,000 files of 4 KiB each (clean, disconnected by removed
directory entries, or missing their checksum records), one dense 16 MiB file,
and one sparse file with four backed bytes at the 1 TiB offset.

The RAM numbers use a RAM device and exclude input setup. They cover diagnosis
or recovery, including manifest publication and the final structural check.
The CLI numbers include process startup, sparse image creation, sidecar
reports, durability calls and `--verify-data`. For the CLI the runner also
checks the exit code (1 for complete recovery, 4 for missing source data),
compares the manifests of both sides except the destination UUID, and confirms
the source SHA-256 did not change.

## What changed

Profiling the 500, 1,000 and 2,000 orphan cases (303, 1,015 and 3,547 ms
without verification) showed repeated page decoding, leaf construction,
checksum validation and workspace checkpoint encoding. The fixes:

- Batch destination mutations into bounded deferred checkpoints once the
  initial manifest is durable. Final report publication and flushes remain.
- Reuse the existing metadata cache for reconstructed source pages and for
  destination verification after a fresh structural check. Physical source
  metadata is still read fresh, and file data is still checksummed.
- Resolve orphan-name collisions through the catalog index.
- Write the intermediate `unverified` report only when verification is
  requested.

## Results

Linux x86-64 under KVM, four vCPUs on an AMD EPYC 7543, 7.7 GiB RAM, Rust
1.99.1, fat LTO, one codegen unit, one worker pinned to CPU 2. Other builds and
VMs ran during the comparison (load average about 5), so the ranges include
that noise. These are warm-input numbers, not physical-device throughput.

Verified recovery on the RAM device, excluding diagnosis. Median
[min-max] milliseconds over five alternating samples per side:

| Case | Before | After |
|---|---:|---:|
| 1,000 orphans | 1355.70 [1307.28-1625.58] | 45.60 [43.16-64.26] |
| 1,000 clean files | 1671.12 [1557.89-2254.26] | 868.13 [815.41-1345.61] |
| 1,000 missing checksum mappings | 640.60 [538.09-693.19] | 24.74 [18.14-27.55] |
| 16 MiB dense file | 34.66 [33.60-35.99] | 24.78 [21.63-29.07] |
| 1 TiB sparse file, four backed bytes | 3.33 [3.26-4.02] | 3.40 [3.07-3.83] |

CLI with `--verify-data`, same units:

| Case | Before | After |
|---|---:|---:|
| 1,000 orphans | 8295.05 [6105.87-8384.61] | 115.37 [105.09-134.15] |
| 1,000 clean files | 8566.88 [7286.60-10335.65] | 1090.36 [1029.00-1117.39] |
| 1,000 missing checksum mappings | 4602.67 [4110.61-5363.20] | 93.87 [75.25-114.99] |
| 16 MiB dense file | 829.14 [744.59-889.95] | 111.11 [95.41-139.08] |
| 1 TiB sparse file, four backed bytes | 35.21 [30.95-40.14] | 25.77 [20.81-39.86] |

The 1,000-orphan case is about 30x faster in RAM and 72x faster through the
CLI. Flush requests drop from 12,048 to 24 (18 without verification), and
destination writes from 102,436,864 to 5,451,776 bytes. The sparse case shows
no RAM change. Its CLI median is lower but the ranges overlap, so it is not an
established win.

An untimed `strace -c` run of verified 1,000-orphan CLI recovery counted
`pread64` calls dropping from 115,492 to 4,166, `pwrite64` from 25,009 to
1,331 and `fsync` from 12,056 to 32. Peak RSS in one `/usr/bin/time` sample went from
21,008 to 7,808 KiB. The metadata cache keeps its 4,096-node bound.

Copying 64 MiB with `--case bandwidth` took a median of 20.57 ms (about
3.26 GB/s). Verified dense recovery is about 5x that copy-only bound and
verified 1,000-orphan recovery about 36x. The remaining profile is spread over
tree scans and writes, memory copies, checksums and diagnosis.

Clean-source recovery still re-reads physical metadata. Caching it for the
whole copy might save about 0.8 s per 1,000 files, which is an inference and
not a measurement. It was not done because it would stop detecting metadata
faults that appear after the first read.

Nothing here covers macOS, Windows, FreeBSD, other CPUs, failing physical
drives or inputs larger than those measured.
