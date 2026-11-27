# CarryFS 0.2.0

CarryFS is an open-source filesystem for sharing data drives across macOS,
Linux, Windows, FreeBSD, and NetBSD. This release adds Unix permissions, hard
links, and preallocation, and moves the disk format to revision 2.

CarryFS remains in public beta. Keep independent backups and do not store the
only copy of important data on a CarryFS volume. Disk format revision 2 is not
frozen. A future format change may require copying files elsewhere and
reformatting the partition.

## New in 0.2.0

- Volumes store each object's Unix permission bits and a hidden attribute.
  Linux, macOS, FreeBSD, and NetBSD enforce the permission bits for the account
  that mounted the volume. Windows shows a file without the owner write bit as
  read-only.
- Files and symlinks can have up to 65,535 names through `ln`. Windows opens,
  renames, and deletes them but cannot create new ones.
- Linux `fallocate`, macOS `F_PREALLOCATE`, and Windows allocation sizes
  reserve space, and Linux can punch holes. Linux and FUSE support unnamed
  temporary files (`O_TMPFILE`).
- A rename can swap two existing entries: Linux `RENAME_EXCHANGE` and macOS
  `RENAME_SWAP`, through FUSE and the native driver. macOS Archive Utility can
  now unpack archives onto a CarryFS volume.
- `carryfs put` commits the image once when it finishes instead of every
  64 KiB, and skips holes in sparse source files on Linux, macOS, and FreeBSD.
- Creating and removing small files, metadata lookups, name checks, and FUSE
  reads do less work, and release builds use full link-time optimization. The
  [benchmark notes](https://github.com/carry-fs/carryfs/blob/v0.2.0/benchmarks/README.md#published-results)
  have new results at format revision 2 on all five platforms.
- FreeBSD 14 and 15 Tier 2 packages, using the base system's FUSE. The
  [installation guide](https://github.com/carry-fs/carryfs/blob/v0.2.0/INSTALLATION.md#platform-support)
  explains the support tiers.
- NetBSD 10.2 and 11.1 Tier 2 support on amd64, using perfused and PUFFS.
- `fsck.carryfs` diagnoses damaged volumes, repairs them in place, or
  recovers their files to a new image (see RECOVERY.md).
- `carryfs --version`, `carryfs-fuse --version`, and
  `carryfs-windows --version` print the release and the format revisions it
  reads and writes.
- The installers and CarryFS Manager offer a stable release as an update to its
  release candidates, so 0.2.0 replaces 0.2.0-rc.1.
- Three new kinds of download, which the installers don't install:
  - `carryfs-0.2.0-android-arm64.apk` is an Android app for Android 8 or later
    on arm64. It shows CarryFS volumes on a USB drive in the Files app and the
    file picker. A development key signs it, so uninstall it before you install
    the APK of a later release.
  - `carryfs-manager-0.2.0-PLATFORM` archives hold CarryFS Manager for macOS,
    Linux, and Windows. On macOS it checks, erases, recovers, mounts, and
    upgrades disks and installs CarryFS updates. macOS blocks the unsigned app
    on first launch; allow it in System Settings > Privacy & Security. On Linux
    and Windows it works with disk image files only. Keep `carryfs-manager` and
    `carryfs` in the same folder.
  - `carryfs-0.2.0-linux-module-src.tar.gz` is the source of the native Linux
    kernel driver, Tier 0 on Linux. A module loads only on the kernel it was
    built for, so build it with `scripts/kernel-build.sh` from the archive, or
    install it with `./install.sh --local --native` from a checkout.

## Compatibility

- New volumes use disk format revision 2. CarryFS 0.1.0 cannot mount them.
  Update CarryFS on every computer that shares a drive before formatting new
  volumes or upgrading old ones.
- CarryFS 0.2.0 reads volumes that 0.1.0 formatted, but mounts them only
  read-only. To write to one, run `carryfs-fuse upgrade DEVICE` on Linux,
  macOS, FreeBSD, or NetBSD, `carryfs-windows upgrade VOLUME` on Windows, or
  `carryfs upgrade IMAGE` for an image. The upgrade writes one root record and
  no file data. Afterwards CarryFS 0.1.0 cannot mount the volume. The Windows
  service does not mount a 0.1.0 volume until it is upgraded.
- `carryfs mkfs` no longer accepts `--attributes`. Every new image stores
  permissions.
- On Linux, `mkfs.carryfs` refuses a device that already holds data, such as
  another file system. Add `-f` to erase it and format. CarryFS 0.1.0 erased
  the device without asking, so scripts that format used devices need `-f`.
  macOS `newfs_carryfs` still erases without asking, because Disk Utility's
  Erase asks first.
- The installers keep a 0.1 installation on 0.1 releases. Run the installer
  with `--upgrade`, or `-Upgrade` on Windows, to move to 0.2.

## Fixes

- `carryfs get` keeps hard links to files with mode 000 linked on macOS, and
  reports any link it had to copy instead.
- Windows gives a hard-linked relative symlink the type of its target as seen
  from the folder you opened it through.
- `fsck --repair --output` refuses an existing output or report path before it
  writes anything, so you can retry a refused rebuild with the same name.
- Checking a volume over 1 TiB usually reads its metadata once instead of
  several times.

## Requirements

- Linux: Ubuntu 24.04 or later, or Arch Linux, on x86-64.
  Linux desktop integration requires systemd, FUSE 3, and UDisks.
- macOS: 15.6 or later on Apple Silicon, with macFUSE 5.4 or later.
- Windows: Windows 10 or later on x86-64, with 64-bit PowerShell 5.1 or later
  and WinFsp 2.1 or later.
- FreeBSD: 15.1 or 14.5 on amd64 or arm64, with the base system's `fusefs`
  module loaded.
- NetBSD: 10.2 or 11.1 on amd64, with the base system's `perfused` and PUFFS.
- Android: 8 or later on arm64, with USB host (OTG) support.
- Native Linux driver: a 7.0 or later kernel built with Rust support and 4 KiB
  pages, and the exact `rustc` build that compiled that kernel.

Install the framework for your platform first. Follow the
[installation guide](https://github.com/carry-fs/carryfs/blob/v0.2.0/INSTALLATION.md)
for setup, exact-version installation, and uninstall instructions.

Linux, macOS, FreeBSD, and NetBSD:

```sh
curl -fsSL https://carryfs.com/install.sh | bash
```

Windows, from 64-bit PowerShell:

```powershell
irm https://carryfs.com/install.ps1 | iex
```

## Limits

Checksums detect corruption but do not repair damaged files. CarryFS has no
self-healing. Boot volumes and server workloads are outside its scope.

CarryFS stores Unix permission bits but not owners, setuid, setgid, or sticky
bits, ACLs, extended attributes, resource forks, or alternate data streams.
Anyone holding the drive can mount it as themselves. Copying a directory tree
is not a complete system backup. Encryption, compression, snapshots,
deduplication, and RAID are outside the current scope.

See the [formatting guide](https://github.com/carry-fs/carryfs/blob/v0.2.0/FORMATTING.md)
before preparing a disk. Report problems with your operating system, CarryFS
version, storage device, and the operation that failed.
