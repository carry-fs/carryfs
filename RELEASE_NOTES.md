# CarryFS 0.1.0

CarryFS is an open-source filesystem for sharing data drives across macOS,
Linux, and Windows. This is the first release available through the default
installation commands.

CarryFS remains in public beta. Keep independent backups and do not store the
only copy of important data on a CarryFS volume. Disk format revision 1 is not
frozen; a future format change may require copying files elsewhere and
reformatting the partition.

## Included

- A shared Rust engine with copy-on-write storage and checksums for file data
  and metadata.
- Linux FUSE and macFUSE adapters, a Windows WinFsp adapter and automount
  service, and tools for formatting and checking volumes.
- A command-line tool for creating images, copying files into and out of
  them, checking filesystem structure, and scrubbing stored file data.
- Release packages and installers that verify downloads against SHA-256
  checksums and ask for confirmation before installing.

## Requirements

- Linux: Ubuntu 24.04 or later, or Arch Linux, on x86-64.
  Linux desktop integration requires systemd, FUSE 3, and UDisks.
- macOS: 15.6 or later on Apple Silicon, with macFUSE 5.4 or later.
- Windows: Windows 10 or later on x86-64, with 64-bit PowerShell 5.1 or later
  and WinFsp 2.1 or later.

Install the framework for your platform first. Follow the
[installation guide](https://github.com/carry-fs/carryfs/blob/v0.1.0/INSTALLATION.md)
for setup, exact-version installation, and uninstall instructions.

Linux and macOS:

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

CarryFS does not preserve platform-specific permissions, ACLs, extended
attributes, resource forks, or alternate data streams. Copying a directory
tree is not a complete system backup. Encryption, compression, snapshots,
deduplication, RAID, and hard links are outside the current scope.

See the [formatting guide](https://github.com/carry-fs/carryfs/blob/v0.1.0/FORMATTING.md)
before preparing a disk. Report problems with your operating system, CarryFS
version, storage device, and the operation that failed.
