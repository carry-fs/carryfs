# Native Linux driver

`linux/` builds `carryfs.ko`, a kernel filesystem driver for CarryFS
volumes. It mounts volumes read-write without FUSE. It is written in Rust
against the kernel's Rust support, plus a C shim for the kernel interfaces
that the Rust bindings don't cover yet.

The driver targets Linux 7.0. It is an out-of-tree driver shaped for an
upstream RFC: it uses the in-tree `kernel` crate, allocates fallibly, and
treats every on-disk value as untrusted. [Upstreaming](#upstreaming) lists
what a submission still needs.

The performance numbers in this file come from earlier revisions of the
driver, before it validated whole volumes at mount and wrote root B
synchronously.

## Build and load

You need a kernel built with Rust support, its headers and Rust libraries,
and the exact `rustc` build that compiled it. `scripts/kernel-build.sh`
checks each of these, says which is missing, and builds the module. CI (the
`kernel` workflow) builds it weekly against these distributions' kernels:

| Distribution | Kernel | Packages |
|---|---|---|
| Ubuntu 26.04 | 7.0 | `linux-headers-$(uname -r) linux-lib-rust-$(uname -r) rustc-1.93` |
| Fedora 45 | 7.2 | `kernel-devel rust` |
| Arch | 7.2 | `linux-headers rust` |

A rustup toolchain doesn't work, even at the kernel's rustc version, because
it is a different build of the compiler.

To install the module alongside the CLI and FUSE adapter from a checkout, use
`./install.sh --local --native`. It leaves the module unloaded and keeps FUSE
as the default. `./install.sh --uninstall` removes every native module the
installer recorded. [Installation](../INSTALLATION.md#fuse-adapter-or-native-driver)
covers kernel upgrades and module signing.

To build and load without installing:

```sh
scripts/kernel-build.sh
sudo insmod linux/carryfs.ko
```

`RUSTC` and `KDIR` override the compiler and the kernel build directory.

## Mount

```sh
sudo mount -i -t carryfs -o uid=1000,gid=1000 /dev/sdX1 /mnt/usb
```

An installed `mount.carryfs` always uses FUSE, so `-i` skips it and selects
the native driver. The driver needs 4 KiB kernel pages. It rejects `sync` and
`dirsync` mounts, because it doesn't publish namespace changes synchronously
yet. Like vfat and exfat, it accepts `uid=` and `gid=`, which default to the
mounting user. Files and directories report their stored permissions, or 0644
and 0755 when none is stored. `ro` mounts read-only and writes nothing, not
even a root mirror. A volume that CarryFS 0.1.0 formatted mounts only with
`ro` until you run `carryfs-fuse upgrade` on it.

## Allocation and unnamed files

The driver implements `fallocate` allocation, `FALLOC_FL_KEEP_SIZE`, and
`FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE`. Other allocation modes return
`EOPNOTSUPP`. Reservations use unwritten extents.

`O_TMPFILE` creates a pinned orphan without a directory entry. `linkat` can
give it a name, unless it was opened with `O_EXCL`. The last close reclaims a
file that still has no name.

`link(2)` gives a file or symlink up to 65,535 names. Linking a directory
fails with `EPERM`. `tests/hardlinks.py` checks link counts, renames between
names of one file, and removal under an open descriptor, on this driver or on
a Linux FUSE mount.

## Check

```sh
cargo build --release -p carryfs-core
sudo make -C linux test
```

`tests/check.py` drives the module through namespace changes, listings
while mounted, data writes, truncation, reclamation, and recovery of orphans
left by a crash. After each phase it runs the userspace `carryfs check` and
`carryfs scrub` and compares file contents after a remount. It also overlaps
data writeback with rename, unlink, replacement, and explicit mtime changes,
then checks that the timestamps survive completion and remount. Finally it
holds 160,000 unnamed files open and closes them, which pushes the metadata
workspace toward both of its bounds (fstests generic/531).

`tests/tmpfile_and_punch.py` checks allocation, naming unnamed files,
exclusive temporary files, and hole punching through an active mmap. It also
runs directly against a Linux FUSE mount:

```sh
python3 linux/tests/tmpfile_and_punch.py /path/to/mounted/carryfs
```

`bash tests/vm.sh` runs the native checks in a disposable VM with the
installed kernel image and module. Run it from `linux/`, or run
`bash linux/tests/vm.sh` from the repository root. It needs `virtme-run`,
QEMU, e2fsprogs, and release builds of the CLI and the FUSE adapter. The guest
reads the host filesystem through 9p, including in CI containers. Test images
go on a temporary 6 GiB disk, because the guest's `/var/tmp` is a small tmpfs
and filling it causes errors that look like driver failures.
`KERNEL_IMAGE` selects another matching kernel image, and `CARRYFS_VM_TCG=1`
uses software emulation when KVM is unavailable.

The VM suite needs the fstests log replay tool. With an fstests checkout in
`build/fstests`, build it from the repository root:

```sh
mkdir -p build/check-kernel
cc -D_GNU_SOURCE -O2 build/fstests/src/log-writes/replay-log.c \
  build/fstests/src/log-writes/log-writes.c -o build/check-kernel/replay-log
```

The VM also runs `tests/faults.py`, which uses device-mapper to fail
metadata, root A, and root B writes one at a time. It checks the root
generations after the failed publication, that fsync keeps failing after the
device recovers, and that the volume reopens. A dm-flakey device that fails
only writes also fails a truncate's tail rewrite, and a later writeback must
then keep the stored size (fstests generic/476). These cases don't fail
flushes alone or tear writes.

Run `python3 linux/tests/flush.py` from the repository root to test flush-only
errors. It boots a separate VM with three QEMU blkdebug devices, which fail
the first, second, and third publication flush. In each case fsync must keep
reporting the error, the volume must reopen cleanly, and it must accept a new
durable change. The runtime CI job runs this test after the main VM suite.

`python3 linux/tests/metadata_writes.py` compiles the C metadata-write helper
with controlled I/O completion and cache reclamation. A failed write must stay
visible until its result is checked, even when reclaim would drop its
unreferenced buffer. The test also checks reference release and allocation
failure. Runtime CI runs it before booting the guest.

`tests/replay.py` records an orphan deletion with `dm-log-writes` while
another thread publishes changes. It starts with cold metadata caches, so
cleanup has to do I/O, and stops the other thread once it sees a partial
commit. It then replays the log into a baseline image and runs the userspace
checker on a snapshot at every logged flush. The run fails unless the deletion
spans several publications and one checked generation still has the orphan
with fewer data blocks, so checking only the final cleanup can't pass. Finally
it reopens one such image through writable native and FUSE mounts, writes a
new file, and checks it after a remount. CI pins the replay tool to fstests
commit `22348afe338c0f6d540c0d7ef0db749eaa51b218`.

`tests/roots.py` simulates interrupted root writes by mixing old and new root
sectors. For both root A and root B it checks all seven partial-prefix lengths
and a write of the last sector alone. Each resulting image must pass the
userspace checker and return the expected file bytes through a read-only
native mount.

`tests/fstests/run.sh FSTESTS_DIR [tests]` runs
[fstests](https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git) against
the driver on two loop images, with `mkfs`, `fsck`, and `mount` helpers in
`tests/fstests/bin/`. See [fstests results](#fstests-results).

To run fstests in the same disposable VM, use these commands from the
repository root. The checkout must already have its helpers built.

```sh
bash linux/tests/vm.sh "$PWD/linux/tests/fstests/guest.sh" "$PWD/build/fstests" -g quick
```

`tests/fuzz.py` mounts corrupted copies of a populated image, read-only or
read-write, walks and reads them, and fails on any kernel BUG, oops, or
warning. It changes random fields of metadata pages and recomputes their
checksums, so the corruption gets past the checksum to the record checks. All
460 cases over two seeds passed. The driver mounted 361 and refused the rest.

## Design

### Shared code with the userspace core

The kernel has no `alloc` crate and requires fallible allocation, so it
cannot run the core's transaction engine, which is built on `Vec`,
`BTreeMap`, and infallible allocation. What both sides need without
allocating lives in `ondisk/`, the `carryfs-ondisk` crate: root and page
encodings, every record check, root selection, CRC32C, and filename
profile 2 with its Unicode 16.0 tables. The core uses it as a crate; the
module compiles the same files by path. XFS shares `libxfs` with xfsprogs
the same way.

The shared `decode_page` checks every page the driver writes before it
reaches the device, and the userspace checker validates whole images that the
driver wrote.

### Files

| File | Role |
|---|---|
| `driver.rs` | Module registration, error mapping |
| `volume.rs` | Geometry, root selection, super block, volume lock |
| `tree.rs` | Verified metadata pages, B+tree searches over committed pages and drafts |
| `edit.rs` | Copy-on-write B+tree edits in the open transaction |
| `space.rs` | Allocation groups, data allocation, workspace refill, retirement, reclamation |
| `commit.rs` | Sealing, workspace bank, two-root publication, mount-time mirroring |
| `check.rs` | Structural, ownership, namespace, and checksum-coverage validation before writable mounts |
| `inode.rs` | Inode loading, lookup, attributes, eviction |
| `namei.rs` | Create, mkdir, symlink, unlink, rmdir, rename |
| `dir.rs` | Directory listing and dentry priming |
| `file.rs` | Verified reads, unit-aligned readahead |
| `write.rs` | Buffered writes, unit writeback, truncation, fsync |
| `names.rs` | Comparison keys, `d_hash`, `d_compare` |
| `vfs.rs` | Typed views of super blocks, inodes, dentries, folios, and directory contexts |
| `glue.c`, `glue.h`, `glue.rs` | C helpers for interfaces without Rust bindings |

`glue.c` holds no filesystem logic. It wraps the `fs_context` mount API,
buffer heads, static inline functions, and libfs's unexported
`simple_write_end`: what in-tree Rust VFS abstractions would provide.
Rust reaches plain struct fields and function pointers, such as `i_link`,
`d_name`, and the `dir_context` actor, through the bindings.

`vfs.rs` stands in for those abstractions on the Rust side. Each type is
`#[repr(transparent)]` over `Opaque<T>`, as `kernel::fs::File` is, and
states once the invariants that make its methods safe. The VFS callbacks
turn raw pointers into these references at the boundary, so filesystem
logic in `dir.rs`, `namei.rs`, `file.rs`, and `write.rs` works with
references; what `unsafe` remains there is block I/O and bounce pages.

### Reads

Metadata pages come from the block device's buffer cache. The driver checks
a page's checksum and records once while it stays cached, and marks it with a
private buffer-head bit, as ext4's `buffer_verified` does. Later reads recheck
only the page's identity.

Each 64 KiB unit of data has one checksum. Readahead therefore widens each
window to unit boundaries with `readahead_expand`, as erofs does for
compressed clusters, and verifies a unit before any of its bytes reach the
page cache. It locates up to 32 units under one hold of the volume lock and
submits their reads without waiting. A workqueue then verifies and unlocks
them, as erofs and btrfs verify data in end_io work. Commits wait for reads in
flight, so a commit never reclaims and reuses a block that is being read.

File data sits in page-cache folios of up to one 64 KiB unit. Reads and
writes allocate folios as large as the request allows. The page cache aligns
a folio to its size, so each folio lies within one unit and the per-unit code
needs no change. With 16 times fewer folios, a 512 MiB cached reread at a
fixed clock on a USB SSD went from 9.3 to 10.5 GB/s (ext4: 10.7), and an
unmount that drops such a file went from 65 to 35 ms.

When files of up to 16 blocks that lie next to each other are read one after
another, the driver reads them through the buffer cache too, and reads the
next 256 KiB ahead there. FUSE gets the same effect by reading the block
device. Without this, each small file is a device request of its own. On a
USB SSD, reading 10,000 4 KiB files in directory order went from 10,082
requests and 670 ms of device time to about 245 requests and 200 ms. When a
data write ends, and before the driver records it, the driver waits for reads
in flight and drops the cached copies of the written blocks, so no reader
finds stale data.

### Writes

A writable mount keeps one open transaction under the volume lock. The
first change under a committed page copies it into a draft; later changes
edit the draft. A commit seals the drafts onto workspace slots, writes the
next workspace bank, and publishes with FORMAT.md's sequence of flush, root A,
flush, root B, flush before it reports success. Sync, fsync, unmount, a
five-second timer, and a transaction reaching 3,072 drafts each start a
commit.

Writeback copies each dirty unit whole, fills its clean pages from the page
cache or the verified old unit, writes it to new blocks, and replaces the
unit's extents and checksum. It allocates the blocks under the volume lock,
submits the writes, and returns. Once the writes complete, a workqueue records
the units and ends their folios' writeback, as iomap completes ioends. Commits
wait for writes in flight, so no publication sees blocks that are allocated
but not yet owned. Each file has one writer or size change at a time, so its
writes are recorded in order. A write that finds no space writes back dirty
data and retries once, as btrfs flushes delalloc and ext4 retries allocation
before returning ENOSPC.

Namespace and explicit timestamp changes update the cached inode times under
the same volume lock as their catalog records. A data completion therefore
cannot copy stale cached times over a newer metadata change.

fsync starts writeback of every dirty file under one plug, so their data
merges into large writes, then waits for all of it.

When the last open of a file that was written to closes, the driver queues the
file for a worker. The worker starts writeback for each batch of closed files
under one plug, so a later fsync of their directory finds the data already
written. NFS also writes back on close, but here the close doesn't wait. The
worker is the module's own thread, with the utilization clamp the FUSE adapter
gives its daemon threads, so schedutil runs it at full clock. That matters
because a writer waits for the worker while it holds the volume lock. On a
kworker the job ran at the low clock of a short burst. Moving it raised 1,000
4 KiB creates on a USB SSD under schedutil from about 12,000 to about 21,000
files/s (FUSE: 13,300). At a fixed full clock both reach about 20,000.

## Caveats learned from other filesystems

- **Case-insensitive names.** `d_hash` and `d_compare` use the format's
  comparison keys, as ntfs3 and exfat use their upcase tables. The kernel's
  `fs/unicode` casefolding cannot be used: it produces NFD keys from a
  different Unicode version. ASCII names fold without locks; others share a
  scratch buffer under a spin lock, since `d_compare` runs under RCU.
- **Negative dentries.** A create must not hide behind a cached negative
  dentry of another spelling. Negative dentries remember the directory's
  iversion and are dropped for creation and rename targets, as in exfat.
- **Case-only rename.** `rename("a", "A")` finds the source's own dentry
  through `d_compare`, so the VFS makes it a no-op. exfat and ext4 casefold
  behave the same.
- **Inode locking.** `iget_locked` can wait on another thread's `I_NEW`
  inode. Lookups and listings therefore drop the volume lock before
  filling inodes.
- **Reclaim recursion.** The volume lock and writeback run in a
  `memalloc_nofs_save` scope, as XFS and btrfs do, so memory reclaim cannot
  re-enter this filesystem's writeback while the lock is held.
- **Buffer-cache aliases.** A block freed as metadata may later hold data
  written by bio. `clean_bdev_aliases` drops stale cached buffers first, as
  ext4 does.
- **Listing then stat.** FUSE's readdirplus instantiates every entry during
  a listing. Without that, a native `ls -l` lost to FUSE, so listings prime
  the dcache as NFS and CIFS do. Priming reads the batch's inode records in
  one catalog walk in object order, as XFS bulkstat walks inode clusters,
  instead of a search per entry.
- **Cold catalog.** The FUSE adapter reads the catalog at mount, because a
  writable open checks the whole forest first. On a non-rotational device a
  writable mount of the driver does the same. It reads and verifies the
  catalog before it returns, a level at a time in block order, filling short
  gaps so that pages from the same commits merge into a few large reads.
  That takes about 28 ms for a 1,700-page catalog on a USB SSD, where a
  writable FUSE mount takes 80 ms.

  A read-only mount reads only the catalog's first 256 leaves in key order,
  which hold the root and the oldest directories. It returns in 8-10 ms, against
  11 ms for FUSE, whose read-only mount starts a key-order read-ahead. A first
  listing elsewhere reads its entries' leaves 16 at a time ahead of itself. As
  it scans each group, it also reads the children's record leaves, merged the
  same way, so its CPU work starts before every entry leaf has arrived. A cold
  read-only listing of 10,000 entries then waits 3-4 ms for reads instead of 10.
  A background walk like the one FUSE runs raced the first listings and slowed
  the passes after them, so the driver doesn't run one on such devices. On a
  disk that seeks, a workqueue reads the catalog ahead a level at a time, as
  ext4 prefetches block bitmaps.
- **Clock.** FUSE's daemon threads set a full utilization clamp on
  themselves, so schedutil runs the CPU fast while they list a directory, and
  the caller's next passes inherit that clock. The driver raises the calling
  task's clamp the same way for the length of a listing that reads the
  catalog, and then restores the default. A file's readahead and an
  unmount's teardown do the same, as FUSE does them in its daemon too. A
  clamp the task set itself stays as it is. Kept listings, dcache hits, and
  cached reads leave the clamp alone. Under schedutil on a USB SSD, cold
  listings went from 1.8-3.1 GHz to 3.3-3.6 GHz, and a warm interleaved
  listing of 1,000 entries went from 6.2 ms to 2.7 ms (FUSE: 6.0 ms).
- **Repeated listings.** FUSE keeps readdir output in the page cache. A
  directory listed whole keeps its entries on its inode, up to 2 MiB of
  names, until it changes or the inode is evicted; inode reclaim bounds the
  memory. Changes hold the directory's inode lock exclusively and listings
  hold it shared, so changes drop the copy without reference counting.
- **Rust in the kernel.** A Rust panic is a kernel BUG and overflow checks are
  on, so the driver reads on-disk data with checked accessors only. Modules
  build as edition 2021, so shared code avoids let chains. objtool in Linux 7.0
  does not know the panic paths of `char::encode_utf8` and
  `slice::sort_unstable`, so the shared code encodes UTF-8 by hand and commit
  uses its own heapsort.
- **Mount helpers.** `mount -t carryfs` runs `mount.carryfs` if installed
  and uses FUSE. Both adapters share the type name, so native
  mounts use `mount -i -t carryfs` to bypass the helper.
- **Direct I/O.** The driver accepts `O_DIRECT` opens and serves them from
  the page cache, and writes a direct write back before it returns. True
  direct I/O would rewrite a whole 64 KiB checksum unit for a 512-byte write.

## Differences from the userspace core

Writable mounts validate structure and block ownership before any root repair,
allocation, or orphan cleanup. The check also reconciles directory links,
inode counts, and checksum coverage. Bitmap windows bound memory use on large
volumes. The userspace checker accepts the images that these remaining
differences produce.

- **Units are rewritten whole.** A partial overwrite copies the whole unit
  to new blocks. FORMAT.md allows keeping unchanged blocks but does not
  require it.
- **Large removals use bounded steps.** Large unlinks and truncations remove
  records in bounded steps, each a valid state, instead of detaching cleanup
  jobs. The driver doesn't drain jobs that other writers left; the userspace
  core does.
- **Reclamation batches are larger.** Up to 64 records and 65,536 blocks per
  commit, above FORMAT.md's initial 1,024-block budget.
- **Unstored attributes.** The driver doesn't store access times. Owner
  changes fail with `EPERM`. The driver drops setuid, setgid, and sticky
  bits, and doesn't expose the volume label.
- **Cgroup I/O accounting.** Writeback does not preserve cgroup ownership
  across writers. fstests generic/563 exposes this limit.

## Benchmarks

`benchmarks/run.py run --images --systems carryfs carryfs-kernel` runs the
full suite on both adapters. The table shows medians of three rounds on 1 GiB
loop images in a KVM guest on Linux 7.0. Loop-device timings vary by about 10%
between runs.

| Metric | FUSE | Kernel |
|---|---:|---:|
| sequential_write (MiB/s) | 403 | 1,622 |
| sequential_write_durable (MiB/s) | 293 | 361 |
| small_file_create (files/s) | 5,015 | 30,622 |
| small_file_create_durable (files/s) | 3,367 | 7,763 |
| tree_create (files/s) | 5,418 | 29,190 |
| create_10000_batch128_durable (ms) | 2,916 | 1,710 |
| remove_10000_batch128_durable (ms) | 1,173 | 783 |
| rw-cold sequential_read (MiB/s) | 489 | 791 |
| rw-warm list_10000_names (ms) | 1.78 | 1.48 |
| rw-stale list_10000_names (ms) | 22.6 | 2.2 |
| ro-cold list_10000_interleaved (ms) | 94 | 118 |

Of 55 workload metrics, the driver wins 32, ties 10, and loses 13. Most
losses are cold listings, 5 to 20% slower. Priming each entry's dentry costs
about as much as FUSE's readdirplus, and the driver reads the catalog from
disk where the FUSE daemon read it ahead at mount. The other losses are fsync
phases of workloads whose totals the driver wins, because `fsync_after_write`
writes what FUSE already wrote during `write()`.

## fstests results

On 2026-11-05, the `quick` group from fstests commit
`22348afe338c0f6d540c0d7ef0db749eaa51b218` ran on Linux 7.2.8-9-omarchy in
a disposable VM. Of 647 tests, 134 passed, 509 did not run, and four failed.
The VM used two 2 GiB loop images on a dedicated 6 GiB ext4 disk. All six
targeted regressions passed too: generic/006, 013, 014, 075, 112, and 127.

This run predates the driver's support for stored modes, hard links,
`fallocate`, and `O_TMPFILE`, added between 2026-11-12 and 2026-11-15, so
rerun the group before you rely on these counts. At the time, tests did not
run when they needed hard links, extended attributes, reflink, fallocate,
freezing, quota, or owner and mode changes. Other skips came from the test
environment, such as its missing `fsgqa` user. The failures came from these
limits:

| Tests | Cause |
|---|---|
| generic/003 | Access times are not stored, and a ctime change through `chmod` is refused |
| generic/120, generic/452 | Programs run from the volume; `mkfs.carryfs` formats without the attributes feature, so no execute bit is stored |
| generic/563 | Writeback does not preserve cgroup I/O ownership across writers |

Earlier VM runs filled the `/var/tmp` tmpfs while writing their loop images,
and the resulting backing-store errors made those results worthless. With the
dedicated disk, generic/129, 411, 451, 460, and 676 all passed. generic/131
passed once the external `locktest` helper was built, which needed a
declaration fix for the installed headers. generic/029, 030, 441, 634, and
740 also passed in the final run.

fstests found five driver bugs, all fixed: incomplete kept listings
(generic/006), growth past a partial tail unit (generic/014), reads racing
writeback (generic/013), the missing `splice_write` (generic/075 and 112),
and the missing `migrate_folio`.

## Upstreaming

A submission would be a series that:

1. Adds Rust VFS abstractions (`kernel::fs`: super blocks, inodes, dentries,
   address-space and file operations, `fs_context`) with their helpers in
   `rust/helpers/`, replacing `glue.c`. They are not in Linux 7.0;
   Wedson Almeida Filho's 2023 "Rust abstractions for VFS" RFC is the
   precedent. `vfs.rs` is the start: its types would move into
   `rust/kernel/fs/`, and operation tables would become `#[vtable]`
   traits.
2. Adds `fs/carryfs/` with these sources and `ondisk/` as a shared library
   directory.
3. Adds `Documentation/filesystems/carryfs.rst` from FORMAT.md and this
   file, and a fstests configuration.

Before that, the driver needs a clean run of the full fstests `auto` group,
longer fuzzing campaigns, and, if non-ASCII lookups contend, a per-CPU name
scratch buffer.
