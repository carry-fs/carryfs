# Native Linux driver

`linux/` builds `carryfs.ko`, a kernel filesystem driver for CarryFS
volumes. It mounts volumes read-write without FUSE and is written in Rust
for the kernel's Rust support, apart from a C shim for kernel interfaces the
Rust bindings do not cover.

The driver targets Linux 7.0. It is an out-of-tree prototype shaped for an
upstream RFC: it uses the in-tree `kernel` crate, allocates fallibly, and
treats every on-disk value as untrusted. [Upstreaming](#upstreaming) lists
what a submission still needs.

## Build and load

You need the headers and Rust libraries of the running kernel, and the
`rustc` that built them. On Ubuntu 26.04 with kernel 7.0:

```sh
sudo apt install linux-headers-$(uname -r) linux-lib-rust-$(uname -r) rustc-1.93
make -C linux build
sudo insmod linux/carryfs.ko
```

`RUSTC` and `KDIR` override the compiler and the kernel build directory.

## Mount

```sh
sudo mount -i -t carryfs -o uid=1000,gid=1000 /dev/sdX1 /mnt/usb
```

An installed `mount.carryfs`, the FUSE adapter's helper, hands the mount to
this driver when the module is loaded or loadable, dropping the FUSE-only
options; `CARRYFS_FUSE=1` keeps FUSE. `-i` skips the helper. The driver accepts `uid=` and `gid=`, defaulting to the
mounting user, as vfat and exfat do; files report mode 644, directories 755.
`ro` mounts read-only and never writes, not even root mirroring.

## Check

```sh
cargo build --release -p carryfs-core
sudo make -C linux test
```

`tests/check.py` drives the module through namespace changes, listings
while mounted, data writes, truncation, reclamation, and crash-orphan
recovery, and judges each phase with the userspace `carryfs check` and
`carryfs scrub`, comparing file contents after remount.

`tests/fstests/run.sh FSTESTS_DIR [tests]` runs
[fstests](https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git) against
the driver on two loop images, with `mkfs`, `fsck`, and `mount` helpers in
`tests/fstests/bin/`. See [fstests results](#fstests-results).

`tests/fuzz.py` mounts corrupted copies of a populated image, read-only or
read-write, walks and reads them, and fails on any kernel BUG, oops, or
warning. It changes random fields of metadata pages and recomputes their
checksums, so the corruption reaches the record checks. 460 cases over two
seeds passed: 361 mounted, the rest were refused.

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

Every page the driver writes is checked by the shared `decode_page` before
it reaches the device, and the userspace checker validates whole images
written by the driver.

### Files

| File | Role |
|---|---|
| `driver.rs` | Module registration, error mapping |
| `volume.rs` | Geometry, root selection, super block, volume lock |
| `tree.rs` | Verified metadata pages, B+tree searches over committed pages and drafts |
| `edit.rs` | Copy-on-write B+tree edits in the open transaction |
| `space.rs` | Allocation groups, data allocation, workspace refill, retirement, reclamation |
| `commit.rs` | Sealing, workspace bank, two-root publication, mount-time mirroring |
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
Plain struct fields and function pointers, such as `i_link`, `d_name`, and
the `dir_context` actor, are reached from Rust through the bindings.

`vfs.rs` stands in for those abstractions on the Rust side. Each type is
`#[repr(transparent)]` over `Opaque<T>`, as `kernel::fs::File` is, and
states once the invariants that make its methods safe. The VFS callbacks
turn raw pointers into these references at the boundary, so filesystem
logic in `dir.rs`, `namei.rs`, `file.rs`, and `write.rs` works with
references; what `unsafe` remains there is block I/O and bounce pages.

### Reads

Metadata pages come from the block device's buffer cache. A page is
checksummed and its records validated once per cache lifetime; a private
buffer-head bit records that, as ext4's `buffer_verified` does, and later
reads recheck only the page's identity.

Data is checksummed per 64 KiB unit, so readahead widens each window to unit
boundaries with `readahead_expand`, as erofs does for compressed clusters,
and verifies a unit before any of its bytes reach the page cache. Readahead
locates up to 32 units under one hold of the volume lock, submits their
reads without waiting, and verifies and unlocks them from a workqueue, as
erofs and btrfs verify data in end_io work. Commits wait for reads in
flight, so no block being read is reclaimed and reused under them.

File data sits in page-cache folios of up to one 64 KiB unit, which reads
and writes allocate as large as the request allows. The page cache aligns a
folio to its size, so each lies within one unit, and everything done per
unit stays as it was. A 512 MiB cached reread at a fixed clock on a USB
SSD went from 9.3 to 10.5 GB/s (ext4 10.7), and unmounts that drop such a
file from 65 to 35 ms, as the page cache handles 16 times fewer folios.

Files of up to 16 blocks, read one after another where they lie together,
are read through the buffer cache too, with the next 256 KiB chunks read
ahead there, as FUSE gets from reading the block device. Otherwise each
small file is a device request of its own: on a USB SSD, reading 10,000
4 KiB files in directory order went from 10,082 requests and 670 ms of
device time to about 245 requests and 200 ms. A data write forgets the cached
copies of its blocks once it ends and before it is recorded, waiting for
reads in flight, so no reader finds stale data.

### Writes

A writable mount keeps one open transaction under the volume lock. The
first change under a committed page copies it into a draft; later changes
edit the draft. Commit seals drafts onto workspace slots, writes the
successor workspace bank, and publishes with FORMAT.md's flush, root A,
flush sequence; root B follows (see [Differences](#differences-from-the-userspace-core)). Sync, fsync, unmount, a five-second timer,
and a transaction of 3,072 drafts commit.

Writeback copies each dirty unit whole, filling clean pages from the page
cache or the verified old unit, writes it to new blocks, and replaces the
unit's extents and checksum. It holds the volume lock to allocate the
units' blocks, submits their writes, and returns; a workqueue records the
units and ends their folios' writeback once the writes complete, as iomap
completes ioends. Commits wait for writes in flight, so no publication
sees blocks allocated but unowned. Each file has one writer or size change
at a time, so its writes record in order. A write that finds no space
writes back dirty data and retries once, as btrfs flushes delalloc and
ext4 retries allocation before ENOSPC.

fsync starts writeback of every dirty file under one plug, so their data
merges into large writes, then waits for all of it. The last close of a
file written through it queues the file for a worker that starts the
writeback of each batch of closed files under one plug, so a later fsync
of their directory finds the data written, as NFS writes back on close,
without making the close wait. The worker is a thread of the module's own
with the utilization clamp the FUSE adapter gives its daemon threads, so
schedutil runs it at full clock: a writer waits for it while it holds the
volume lock. On a kworker it ran at the low clock of a short burst, and
1,000 4 KiB creates on a USB SSD under schedutil went from about 12,000
to about 21,000 files/s (FUSE 13,300); at a fixed full clock both are
about 20,000.

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
- **Cold catalog.** The FUSE adapter reads the catalog at mount: a writable
  open checks the whole forest first. On a non-rotational device a
  writable mount of the driver does the same, reading and verifying the
  catalog before it returns, a level at a time in block order with short
  gaps filled, so that pages written by the same commits merge into a few
  large reads: about 28 ms for a 1,700-page catalog on a USB SSD, where a
  writable FUSE mount takes 80 ms. A read-only mount reads the catalog's
  first 256 leaves in key order, the root and the oldest directories, and
  returns in 8-10 ms, against 11 ms for FUSE, whose read-only mount
  starts a key-order read-ahead; a first listing elsewhere reads the
  leaves of its entries 16 at a time ahead of itself and, as it scans each
  group, its children's record leaves, each merged, so that its CPU work
  starts before every entry leaf has arrived: a cold read-only listing of
  10,000 entries then waits 3-4 ms for reads rather than 10. A
  background walk, as FUSE runs, raced first listings and slowed the
  passes after them. On a disk that seeks, a workqueue reads the catalog
  ahead a level at a time, as ext4 prefetches block bitmaps.
- **Clock.** FUSE's daemon threads set a full utilization clamp on
  themselves, so schedutil runs the CPU fast while they list a directory,
  and the caller's next passes inherit that clock. A listing that reads
  the catalog raises the calling task's clamp the same way for the call
  and returns it to the default after, and so do a file's readahead and
  an unmount's teardown, which FUSE also does in its daemon; a clamp the
  task set itself is left
  alone, and kept listings, dcache hits and cached reads do not touch it.
  Under schedutil on a USB SSD, cold listings went from 1.8-3.1 GHz to
  3.3-3.6 GHz, and a warm interleaved listing of 1,000 entries from
  6.2 ms to 2.7 ms (FUSE 6.0 ms).
- **Repeated listings.** FUSE keeps readdir output in the page cache. A
  directory listed whole keeps its entries on its inode, up to 2 MiB of
  names, until it changes or the inode is evicted; inode reclaim bounds the
  memory. Changes hold the directory's inode lock exclusively and listings
  hold it shared, so changes drop the copy without reference counting.
- **Rust in the kernel.** A Rust panic is a kernel BUG and overflow checks
  are on, so on-disk data is read with checked accessors only. Modules build
  as edition 2021, so shared code avoids let chains. objtool in Linux 7.0
  does not know the panic paths of `char::encode_utf8` and
  `slice::sort_unstable`, so the shared code encodes UTF-8 by hand and
  commit uses its own heapsort.
- **Mount helpers.** `mount -t carryfs` runs `mount.carryfs` if installed.
  Both adapters share the type name, as ntfs-3g and ntfs3 do not, so the
  helper decides: it mounts through the driver when `carryfs` is in
  `/proc/filesystems` after `modprobe`, and through FUSE otherwise.
- **Direct I/O.** `O_DIRECT` opens are accepted and served by the page
  cache, and a direct write is written back before it returns. True direct
  I/O would rewrite a whole 64 KiB checksum unit for a 512-byte write.

## Differences from the userspace core

All of these produce images the userspace checker accepts.

- **Root B follows later.** FORMAT.md acknowledges a publication after
  flush, root A, flush, root B, flush. The driver acknowledges after root
  A's flush, which its crash table already makes durable ("G is durable in
  A; select G and finish mirroring"), and writes root B with the next
  publication's pages, whose first flush covers it before root A is
  overwritten; sync and unmount write it at once. That saves one cache
  flush per fsync. `tests/check.py` checks an image copied right after
  fsync.
- **Writable mounts check less.** FORMAT.md asks a writable reopen to
  check all ownership before mirroring. The driver verifies every page it
  reads and validates the workspace list, but leaves whole-volume checks to
  `carryfs check`, as kernel filesystems leave them to fsck.
- **Units are rewritten whole.** A partial overwrite copies the whole unit
  to new blocks. FORMAT.md allows keeping unchanged blocks but does not
  require it.
- **Large removals use bounded steps.** Large unlinks and truncations remove
  records in bounded steps, each a valid state, instead of detaching
  cleanup jobs. Jobs other writers left are not drained; the userspace core
  drains them.
- **Reclamation batches are larger.** Up to 64 records and 65,536 blocks per
  commit, above FORMAT.md's initial 1,024-block budget.
- **Unstored attributes.** Access times are not stored. Changes to mode or
  owner fail with `EPERM`. The volume label is not exposed.
- **Hard links.** The format has none; `link(2)` fails with `EPERM`.

## Benchmarks

`benchmarks/run.py run --images --systems carryfs carryfs-kernel` runs the
full suite on both adapters. Medians of three rounds on 1 GiB loop images
in a KVM guest on Linux 7.0; loop-device timings vary by about 10% between
runs.

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

Of 55 workload metrics the driver wins 32, ties 10, and loses 13. The
losses are mostly cold listings, 5 to 20% slower: priming each entry's
dentry costs about what FUSE's readdirplus does, and the driver reads the
catalog from disk where the FUSE daemon reads it ahead at mount. The others
are fsync phases whose totals the driver wins: `fsync_after_write` writes
what FUSE wrote during `write()`.

## fstests results

The `quick` group from fstests at commit 22348afe: of 647 tests, 98 pass,
539 do not run, and 10 fail; with `O_DIRECT` since accepted, generic/091,
130, 263, and 704 also pass. Tests do not run when they need what the
format lacks (hard links, extended attributes, reflink, fallocate,
freezing, quota, owners and modes) or what the host lacks (a `fsgqa`
user).

Every failure comes from the format, the host, or a deliberate choice:

| Tests | Cause |
|---|---|
| generic/003 | Access times are not stored, and a ctime change through `chmod` is refused |
| generic/029, generic/030 | The host's `od` prints hexadecimal in upper case |
| generic/120, generic/452 | Programs run from the volume; the format stores no execute bit |
| generic/441 | After a failed write, FORMAT.md keeps the volume faulted until remount, so fsync keeps failing after the device recovers |
| generic/563 | Needs a device smaller than the format's 256 MiB minimum |
| generic/634 | The host's `touch` cannot parse a date past 2038 |
| generic/740 | The userspace `mkfs` helper overwrites other filesystems without `-f` |

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

Before that, the driver needs the full `auto` group of fstests, longer
fuzzing campaigns, and a per-CPU name scratch if non-ASCII lookups
contend.
