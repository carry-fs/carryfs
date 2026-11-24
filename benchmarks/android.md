# Android USB benchmark

The benchmark opens both filesystems through Android's Storage Access
Framework. It runs in an independent app and measures each provider and its
storage driver together. See the [run instructions](../android/README.md#compare-usb-filesystems)
and [per-round data](android.tsv).

## Samsung Galaxy S25 Ultra

Measured on November 24, 2026, on a Galaxy S25 Ultra running Android 17.
Both runs used existing 8 GiB partitions on the same OCZ Vertex 4 SSD and
USB connection. CarryFS used its userspace Bulk-Only Transport driver.
Android mounted exFAT over UAS with `dirsync`. The benchmark reused the
partitions and removed its own files after each round.

Each cell shows the median and the range. The runs were grouped by
filesystem, with CarryFS 0.2.0 measured first using the APK already on the
phone. The exact source commit of that APK is unknown. These are preliminary
results for this setup. A follow-up with build `d5802bd` hit a USB timeout
before it could mount the CarryFS partition.

| Android workload | CarryFS, 3 rounds | exFAT, 5 rounds |
| --- | ---: | ---: |
| Write 512 MiB and sync (MiB/s) | 79.42 (68.12–84.29) | 211.52 (207.80–212.43) |
| Read 512 MiB immediately after write (MiB/s) | 99.86 (95.32–101.15) | 5,582.68 (5,412.53–6,119.69) |
| Create and write 1,000 files (files/s) | 57.70 (57.50–59.05) | 329.37 (300.03–333.53) |
| List the names and sizes of 1,000 files (ms) | 59.90 (52.85–66.78) | 15.97 (15.53–17.79) |
| Remove 1,000 files (ms) | 1,461.44 (1,449.75–1,486.10) | 1,790.09 (1,784.31–2,035.64) |

The read includes the effect of each provider's cache. Kernel disk
counters recorded under 2 MiB of reads over USB during the exFAT session,
while the benchmark read and checked 2,560 MiB. Small-file operations use
ordinary document calls without a separate sync step. The desktop suite
uses different cache controls and sync points.
