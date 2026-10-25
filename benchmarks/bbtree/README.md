# CarryFS and bbtree benchmark evidence

This directory preserves the experiment behind [Are the bbtree really good for FS?](../../articles/are-the-bbtree-really-good-for-fs.md).

The original core comparison found small, noisy differences after replacing the production core maps. The size sweep found substantial gains for large ordered maps and regressions for small maps. The published decision is to retain `std::collections::BTreeMap` in CarryFS.

| Record | Contents |
|---|---|
| [Initial report](runs/report.md) | Core, scratch-file, and initial map comparisons |
| [Scaling report](runs/size-sweep/report.md) | Observed map sizes, all 67 configurations, intervals, and local plateau decisions |
| [Core samples](runs/core-raw.tsv) | 180 measured runs, nine paired repetitions per case |
| [Scratch-file samples](runs/file-raw.tsv) | 72 measured runs |
| [Initial map samples](runs/maps-raw.tsv) | 540 configuration samples with short wall-time windows |
| [Scaling samples](runs/size-sweep/scaling-raw.tsv) | 938 measured process runs, seven pairs per configuration |
| [Calibration](runs/size-sweep/calibration-summary.json) | Occupancies observed in five Volume API workloads |
| [Scaling metadata](runs/size-sweep/metadata.json) | Hardware, compiler, source revisions, metric, and memory peak |
| [Candidate diff](runs/candidate.patch) | Core map substitutions, without changes to the persistent tree |
| [Complete archive](archive/experiment.tar.gz) | Source snapshots, fixtures, upstream source, dependency cache, runners, reports, diagnostics, and test logs |

The files under `runs/` are byte-for-byte exports. Their historical paths and build scripts describe the original run. `preservation-manifest.json` records their SHA-256 hashes. The archive is the complete runnable source snapshot, with SHA-256 recorded in `archive/SHA256SUMS`. Compiled binaries, Cargo target directories, and Git internals are excluded.

Frozen logs retain their final blank lines, and the saved unified diff retains the spaces required on blank context lines. These records are hash-checked rather than reformatted. Whitespace checks for the new article and reproduction wrapper run separately.

The archive contains CarryFS baseline `df656f3d053167f4788d53e33a3ce7a8fdb0b03e` and bbtree 0.1.5 at `8adae97a0ed97e5c98fa2790312eb0aec9719846`, including their licenses. Earlier timer and wall-time diagnostic probes remain available. They are excluded from the final scaling statistics.

To restore and verify the evidence, use Python 3.11.8 or later:

```sh
python3.11 benchmarks/bbtree/reproduce.py verify
```

The command extracts the archive into the persistent, ignored directory `build/bbtree-fs-evaluation`. It verifies the archive, the readable records, all 938 final samples, their checksums and means, and the eight local plateau decisions. Running it again reuses the restored workspace. No original `/tmp` directory is required.

To build and check the restored programs on macOS, use the already installed Rust 1.99.0 toolchain:

```sh
python3.11 benchmarks/bbtree/reproduce.py build
python3.11 benchmarks/bbtree/reproduce.py smoke
```

Builds use the archived dependency cache with `--offline --locked`. The wrapper resolves Cargo and rustc through rustup before setting the workspace's `CARGO_HOME`. It installs no tools and avoids the machine-specific compiler paths in the original archived build scripts.

A full rerun writes into the restored workspace and leaves these committed records untouched:

```sh
python3.11 build/bbtree-fs-evaluation/run.py core --repeats 9
python3.11 build/bbtree-fs-evaluation/run.py file --repeats 9
python3.11 build/bbtree-fs-evaluation/run.py maps --repeats 9
python3.11 build/bbtree-fs-evaluation/size-sweep/sweep.py --millis 1000 --pairs 7
python3.11 build/bbtree-fs-evaluation/size-sweep/report.py
python3.11 build/bbtree-fs-evaluation/size-sweep/verify.py
```

The sweep may take tens of minutes and use about 1.5 GiB of process memory in the largest case. The core and file cases create disposable RAM or regular-file images. They do not mount or format an existing device. Run one measurement at a time.

The scaling harness uses macOS process CPU time and thread QoS. Its statistics can be verified elsewhere, but its binaries require macOS. If hardware, the compiler, or the operating system changes, record that change in the restored metadata before interpreting new results. The recorded occupancies and local plateaus apply to these workloads and tested sizes. Mounted filesystem throughput, other platforms, and call-stack or allocation profiles were not measured by the scaling experiment.
