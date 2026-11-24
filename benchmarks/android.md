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

## Ten-round USB comparison

The same phone, SSD, and adapter ran ten rounds per build with 128 KiB
requests. The run order was exFAT, CarryFS at `d5802bd`, and the two
CarryFS candidates. Each cell shows the median and distance to the observed
minimum and maximum. See the [per-round USB data](android-usb-128k.tsv).

| Workload | Unit | CarryFS baseline | Initial candidate | Revised candidate | exFAT |
| --- | --- | ---: | ---: | ---: | ---: |
| Write 512 MiB and sync | MiB/s | 145.6 (−7.8/+2.8) | 134.3 (−8.2/+4.9) | 130.0 (−4.8/+0.8) | 211.0 (−3.6/+1.3) |
| Read 512 MiB after write | MiB/s | 157.3 (−10.2/+5.4) | 145.2 (−6.6/+1.5) | 140.0 (−1.3/+1.2) | 5,280.1 (−425.3/+229.2) |
| Create and write 1,000 files | files/s | 83.3 (−4.8/+3.6) | 81.0 (−3.7/+3.4) | 79.2 (−1.2/+1.5) | 297.1 (−22.3/+37.2) |
| List 1,000 files | ms | 22.1 (−2.4/+9.8) | 34.4 (−10.6/+8.3) | 32.2 (−9.5/+14.1) | 18.4 (−1.0/+0.5) |
| Remove 1,000 files | ms | 856.8 (−31.0/+101.5) | 935.5 (−49.1/+120.8) | 1,041.9 (−17.3/+5.0) | 2,020.8 (−270.0/+135.2) |

Both candidates regressed on USB and were reverted. The initial candidate
moved reclamation out of every written file's close. The revision restored
reclamation for dirty closes and skipped a redundant barrier after an
explicit fsync. A simulated USB transport counted 3,000 durability barriers
to create 1,000 files, which helps explain the cost of small-file writes.
Android's cache served the large exFAT read, so that row measures
read-after-write through each provider.

### USB transfer experiment

After the SSD reconnected in a later session, a separate build raised
the SCSI read limit from 64 KiB to 128 KiB and the USB write transfer size
from 16 KiB to 32 KiB. The provider used the original code. A fake USB
transport verified the readback and counted half as many read transfers
and 47% fewer outbound transfers for a 32 MiB write. Those counts measure
USB calls, without the device's transfer latency.

| Workload | Unit | Wider transfers |
| --- | --- | ---: |
| Write 512 MiB and sync | MiB/s | 80.1 (−0.9/+12.6) |
| Read 512 MiB after write | MiB/s | 73.0 (−0.9/+9.3) |
| Create and write 1,000 files | files/s | 57.2 (−6.0/+3.6) |
| List 1,000 files | ms | 51.0 (−27.5/+16.4) |
| Remove 1,000 files | ms | 865.4 (−26.5/+16.4) |

The repeat with the original transfer sizes failed on its first sequential
read after a USB-port reset. usbfs returned `ENOMEM` and then protocol
errors. Linux allocates a kernel buffer for every synchronous bulk transfer
and can reject the allocation before submitting it. [Linux USB implementation](https://raw.githubusercontent.com/torvalds/linux/v6.6/drivers/usb/core/devio.c)

The driver now retries a rejected read in 16 KiB pieces and keeps that
limit for the connection. An injected failure verified that every byte
arrives in order and a short packet still ends the read. The default
transfer sizes remain in place.

## Initial provider experiment on an internal image

The table compares the CarryFS provider at `d5802bd` with an experiment that
used bounded document-name lookups and publication-only close and fsync. Both
builds ran the same [instrumented workload](../android/README.md#test) on the
same phone, each starting with a fresh copy of a 1 GiB CarryFS image on internal storage.
The native library used the release profile in both builds. The client sent
128 KiB sequential requests and checked every byte on readback.

Each cell shows the median and the observed spread below and above it.
Each build completed ten rounds. The [raw samples](android-image.tsv) contain
all five measurements from each round.

| Workload | Unit | Baseline | Candidate |
| --- | --- | ---: | ---: |
| Write 512 MiB and sync | MiB/s | 401.2 (−92.1/+153.5) | 433.6 (−88.7/+59.3) |
| Read 512 MiB after write | MiB/s | 421.5 (−45.9/+26.9) | 435.7 (−20.1/+13.9) |
| Create and write 1,000 files | files/s | 274.3 (−20.1/+6.6) | 290.9 (−3.3/+5.6) |
| List 1,000 files | ms | 11.7 (−1.9/+5.3) | 11.2 (−0.9/+3.8) |
| Remove 1,000 files | ms | 441.2 (−173.9/+20.6) | 363.1 (−9.4/+58.4) |

Creation improved by 6.1% at the median, with no overlap between the observed
ranges. Removal took 17.7% less time at the median, with overlap. The
sequential and listing measurements also overlapped. These runs isolate the
provider and CarryFS core on internal storage.
