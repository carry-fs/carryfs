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

`-i` stops `mount` from running an installed `mount.carryfs` helper, which
mounts through FUSE. The driver accepts `uid=` and `gid=`, defaulting to the
mounting user, as vfat and exfat do; files report mode 644, directories 755.
`ro` mounts read-only and never writes, not even root mirroring.

## Check

```sh
cargo build --release -p carryfs-core
sudo make -C linux test
```

`tests/check.py` drives the module through namespace changes, data writes,
truncation, reclamation, and crash-orphan recovery, and judges each phase
with the userspace `carryfs check` and `carryfs scrub`, comparing file
contents after remount.

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
| `glue.c`, `glue.h`, `glue.rs` | C helpers for interfaces without Rust bindings |

`glue.c` holds no filesystem logic. It wraps the `fs_context` mount API,
buffer heads, static inline functions, and libfs's unexported
`simple_write_end`: what in-tree Rust VFS abstractions would provide.

### Reads

Metadata pages come from the block device's buffer cache. A page is
checksummed and its records validated once per cache lifetime; a private
buffer-head bit records that, as ext4's `buffer_verified` does, and later
reads recheck only the page's identity.

Data is checksummed per 64 KiB unit, so readahead widens each window to unit
boundaries with `readahead_expand`, as erofs does for compressed clusters,
and verifies a unit before any of its bytes reach the page cache.

### Writes

A writable mount keeps one open transaction under the volume lock. The
first change under a committed page copies it into a draft; later changes
edit the draft. Commit seals drafts onto workspace slots, writes the
successor workspace bank, and publishes with FORMAT.md's flush, root A,
flush, root B, flush sequence. Sync, fsync, unmount, a five-second timer,
and a transaction of 3,072 drafts commit.

Writeback copies each dirty unit whole, filling clean pages from the page
cache or the verified old unit, writes it to new blocks, and replaces the
unit's extents and checksum. Allocation, the data writes, and the catalog
update share one hold of the volume lock, so no publication sees blocks
allocated but unowned.

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
  the dcache as NFS and CIFS do, once per directory until its inode is
  evicted.
- **Rust in the kernel.** A Rust panic is a kernel BUG and overflow checks
  are on, so on-disk data is read with checked accessors only. Modules build
  as edition 2021, so shared code avoids let chains. objtool in Linux 7.0
  does not know the panic paths of `char::encode_utf8` and
  `slice::sort_unstable`, so the shared code encodes UTF-8 by hand and
  commit uses its own heapsort.
- **Mount helpers.** `mount -t carryfs` runs `mount.carryfs` if installed,
  which is the FUSE adapter. Packaging both needs a different type name or
  helper.

## Differences from the userspace core

All of these produce images the userspace checker accepts.

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

`benchmarks/run.py` runs the full suite with `--systems carryfs carryfs-kernel`.
Medians of three rounds on 1 GiB loop images (KVM guest, Linux 7.0),
measured before the fsync and listing fixes below:

| Metric | FUSE | Kernel |
|---|---:|---:|
| sequential_write (MiB/s) | 391 | 1,323 |
| small_file_create (files/s) | 5,597 | 21,914 |
| tree_create (files/s) | 4,943 | 25,956 |
| create_10000_batch128_durable (ms) | 2,986 | 901 |
| rw-cold sequential_read (MiB/s) | 531 | 730 |
| rw-warm sequential_read (MiB/s) | 1,734 | 9,631 |
| mount_rw-stream (s) | 0.078 | 0.004 |

That run lost `fsync_after_small_files` (2,802 ms against 93 ms) and warm
listings. fsync now writes back every dirty file before committing, and
listings batch entries in one buffer: a spot check then measured 55 ms for
the 1,000 fsyncs and 3.4 ms for a warm 10,000-name listing (FUSE 1.8 ms).
Rerun the suite to confirm before quoting these.

## Upstreaming

A submission would be a series that:

1. Adds Rust VFS abstractions (`kernel::fs`: super blocks, inodes, dentries,
   address-space and file operations, `fs_context`) with their helpers in
   `rust/helpers/`, replacing `glue.c`. They are not in Linux 7.0;
   Wedson Almeida Filho's 2023 "Rust abstractions for VFS" RFC is the
   precedent.
2. Adds `fs/carryfs/` with these sources and `ondisk/` as a shared library
   directory.
3. Adds `Documentation/filesystems/carryfs.rst` from FORMAT.md and this
   file, and a fstests configuration.

Before that, the driver needs fstests runs, fuzzing of the mount path
with corrupted images, a per-CPU name scratch if non-ASCII lookups contend,
and a decision on the mount helper name.
