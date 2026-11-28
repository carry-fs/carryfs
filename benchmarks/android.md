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

## Small-file deletion on Android

The Android provider now gets the parent directory from the delete operation.
This avoids a second document lookup and lets it notify the parent that
actually contained the removed document.

The baseline was `ceaf137`. Both builds used the release native library on
the same phone and alternated in batches of five rounds, each starting with
a fresh 1 GiB CarryFS image on internal storage. The instrumented workload
ran with `-e smallOnly true` and checked that the listing contained all
1,000 names and sizes and that every removal succeeded. The table shows the
median and observed spread over ten rounds per build. The
[raw samples](android-image.tsv) use the `delete-baseline` and
`delete-single-lookup` labels.

| Workload | Unit | Baseline | Single lookup |
| --- | --- | ---: | ---: |
| Create and write 1,000 files | files/s | 290.53 (−11.69/+8.80) | 287.57 (−7.16/+4.66) |
| List 1,000 files | ms | 13.71 (−3.69/+3.69) | 13.91 (−3.93/+5.83) |
| Remove 1,000 files | ms | 448.64 (−14.65/+15.97) | 428.03 (−9.21/+6.81) |

Median removal time fell by 4.6%. Creation and listing varied within the
observed spread. A separate USB diagnostic run spent 5.9 seconds in 2,940
SCSI cache flushes during the 17-second creation phase. Reordering and
buffering metadata did not cut the number of SCSI writes for creation in
the simulated transport, so the adapter keeps its existing write and
durability behavior. Both builds timed out during USB mount in the
follow-up comparison, so the deletion result is for internal storage.

## November 28 comparison

Measured on a Galaxy S25 Ultra (SM-S938B) running Android 17 build
`S938BXXUDDZJB`. The OCZ Vertex 4 sat in its usual enclosure (USB ID
`13fd:5917`) and linked at SuperSpeed. Both filesystems used the same 8 GiB
partitions as before and ran ten rounds each through the benchmark app,
exFAT first. The first CarryFS run used `adfe776` with the document-lookup
and USB-handoff changes described below. The second also had read-ahead and
ran about an hour later. See the [per-round data](android-2026-11-28.tsv).

| Workload | Unit | CarryFS, first | CarryFS, read-ahead | exFAT, first | exFAT, second |
| --- | --- | ---: | ---: | ---: | ---: |
| Write 512 MiB and sync | MiB/s | 70.1 (−1.3/+4.8) | 151.8 (−4.3/+1.4) | 211.0 (−13.7/+2.0) | 210.8 (−13.8/+2.4) |
| Read 512 MiB, wall time | MiB/s | — | 215.4 (−117.5/+4.7) | — | 824.9 (−75.2/+101.7) |
| Read 512 MiB, time in reads | MiB/s | 75.6 (−3.2/+11.8) | 440.5 (−315.7/+5.2) | 6,270.0 (−1,383.7/+1,199.1) | 7,280.2 (−446.2/+1,656.2) |
| Create and write 1,000 files | files/s | 56.2 (−3.4/+5.1) | 91.2 (−2.2/+2.0) | 327.4 (−35.7/+23.8) | 341.9 (−33.2/+17.1) |
| List 1,000 files | ms | 48.0 (−18.3/+38.8) | 26.6 (−9.3/+11.6) | 18.1 (−2.4/+2.3) | 17.7 (−2.1/+1.0) |
| Remove 1,000 files | ms | 757.9 (−9.1/+23.1) | 691.7 (−6.3/+30.7) | 1,760.3 (−71.6/+345.1) | 1,789.7 (−60.5/+180.9) |

The SSD's state moved CarryFS's results more than any code change did. In
the read-ahead comparison below, the same build wrote at 139–152 MiB/s in
its first batch and at 69–95 MiB/s fifteen rounds later, after about 8 GiB of
writes. Small-file creation fell from 97 to 68–76 files/s over the same
batches. The drive recovers after sitting idle. The first CarryFS run, and
the November 28 runs of `d5802bd` at 69 MiB/s, caught it in the slow state.
The write path is unchanged since November 24, when it reached 145.6 MiB/s.
Compare builds only within alternating batches.

The workload originally timed only `readFully`. With read-ahead the provider
keeps reading while the client checks each chunk, so that figure can exceed
the link. The wall time includes the checks and is the figure to compare.
The checks also bound exFAT's wall time.

### Where CarryFS loses time

A profiling build timed every SCSI command and every AppFuse callback over
the instrumented workload.

**Small files.** Each created file is one publish. A publish sends three
SYNCHRONIZE CACHE commands: after the data and tree pages, after root A and
after root B. One round sent 3,022 for 1,000 files, at 2.0 ms each. That is
6 ms per file, and about 8 WRITE commands add 1.3 ms. The drive alone caps
creation near 135 files/s. Linux's exFAT driver waits for its directory
writes but does not flush the drive's cache for them. Matching it
needs a commit protocol with fewer barriers. That is a core change, not an
adapter change.

**Sequential writes.** Bulk-Only Transport runs one command at a time, and
CarryFS sends each 256 KiB WRITE as 16 KiB synchronous transfers. In three
rounds per size in the same drive state, 16 KiB pieces wrote at 71 MiB/s,
64 KiB pieces at 90–93 and 256 KiB pieces at 93–95. The 16 KiB limit protects
a Lexar stick that dropped off the bus with larger transfers. Raising it for
this SSD needs a per-device rule tested on that stick, so the limit stays.
The kernel's UAS driver, which exFAT uses, queues several commands.

**Sequential reads.** exFAT's read after write comes from Android's page
cache. On November 24 the kernel disk counters recorded under 2 MiB of USB
reads, and 7,280 MiB/s is beyond USB 3.2 Gen 1. No USB driver can match that
without caching the whole file in memory. CarryFS reads through AppFuse,
which passes the provider 64 KiB per callback. Without read-ahead, each
callback waited about 375 µs for its USB READ and spent about 350 µs more in
AppFuse. CarryFS now fetches the next 256 KiB on a worker thread while the
current callback returns.

**Listing.** The second run listed in 26.6 ms against exFAT's 17.7 ms, and
the first run's spread covers both. This was not profiled further.

### Read-ahead

Builds `8a073a5` and read-ahead ran the full instrumented workload in
alternating five-round batches.

| Workload | Unit | Without | Read-ahead |
| --- | --- | ---: | ---: |
| Read 512 MiB, wall time | MiB/s | 78.3 (−38.6/+41.5) | 178.3 (−123.4/+34.1) |
| Read 512 MiB, time in reads | MiB/s | 119.6 (−45.7/+46.5) | 386.0 (−274.4/+66.5) |

Within each drive state, read-ahead read faster: 175–197 MiB/s in four of
five rounds against 114–119 MiB/s while the drive was fast, and 54–212
against 39–42 MiB/s once it slowed.
Writes, creation and removal followed the drive's state, not the build.

### Document lookup

`isChildDocument` now asks the native layer for the parent ID only, and the
session remembers the name of the document it created or found last. The
instrumented small-file workload alternated builds in five-round batches.

| Workload | Unit | USB, `adfe776` | USB, lookup | Image, `adfe776` | Image, lookup |
| --- | --- | ---: | ---: | ---: | ---: |
| Create and write 1,000 files | files/s | 73.2 (−5.4/+6.8) | 74.7 (−4.2/+3.7) | 326.6 (−1.5/+2.9) | 332.0 (−3.4/+30.4) |
| List 1,000 files | ms | 46.0 (−17.2/+24.0) | 49.2 (−17.5/+26.8) | 11.8 (−2.9/+6.0) | 11.2 (−2.0/+9.2) |
| Remove 1,000 files | ms | 399.6 (−9.3/+13.7) | 391.2 (−66.8/+15.6) | 388.6 (−12.3/+13.6) | 370.3 (−3.0/+7.5) |

On the internal image, creation rose 1.6% and removal took 4.7% less time at
the median. On USB the differences stayed inside the spread.

### Handing the drive between Android and CarryFS

This bridge fails Bulk-Only commands after Android's UAS driver has used it.
The first command times out after 20 seconds, and later attempts alternate
between timeouts and `EPROTO`. A Bulk-Only reset and cleared halts do not
recover it, but a USB reset does. A reset makes the kernel rebind its UAS
driver, so CarryFS now resets the device after claiming it and claims it
again before Android can mount anything. Eleven mounts in a row succeeded on
the first command in 4–5 seconds. The last was a replug with CarryFS set to
open automatically, which mounted half a second after the drive appeared.

Releasing the drive had the opposite problem. Without a reset, Android's UAS
driver took 30–60 seconds to find the disk again, or never did until the
drive was replugged. CarryFS now resets the drive when it releases it, and
Android remounted exFAT within 2 seconds after each of nine releases.

Claiming the drive while Android has a volume mounted from it removes that
volume without an unmount. Samsung then shows **Restart your phone**. An app
cannot unmount another volume, so unmount it first, in Settings or with
`adb shell sm unmount public:8,67`, to avoid the warning.
