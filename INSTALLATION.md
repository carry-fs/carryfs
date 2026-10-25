# Installing CarryFS

Install a release package after setting up the framework for your platform.
For source builds, see
[CONTRIBUTING.md](CONTRIBUTING.md#build-and-install-from-source).
If you'd rather not run the installer, [install by hand](#install-by-hand).

## Requirements

CarryFS mounts volumes through a user-space filesystem framework. The install
script stops if it can't find one.

### Linux

You need Ubuntu 24.04 or later, or Arch Linux, on x86-64. Linux desktop
integration requires systemd, [FUSE 3](https://github.com/libfuse/libfuse), and
UDisks. The installer requires a running systemd host.

```sh
# Ubuntu
sudo apt install curl jq fuse3 udisks2 systemd udev procps coreutils

# Arch Linux
sudo pacman -S curl jq fuse3 udisks2 procps-ng coreutils
```

`/dev/fuse` must exist. If it doesn't, load the kernel module with
`sudo modprobe fuse`.

### macOS

You need macOS 15.6 or later on Apple Silicon.

Install [macFUSE](https://macfuse.github.io/) 5.4 or later. macOS blocks its
kernel extension until you allow it, so follow the
[macFUSE getting started guide](https://github.com/macfuse/macfuse/wiki/Getting-Started)
through the restart.

The install script needs the extension loaded. This prints a line when it is:

```sh
kmutil showloaded --list-only | grep io.macfuse.filesystems.macfuse
```

### Windows

You need Windows 10 or later on x86-64, with a 64-bit PowerShell 5.1 or newer.

Install [WinFsp](https://winfsp.dev/rel/) 2.1 or later.

## Install a release package

Run the install script to install the latest stable release.

Linux and macOS:

```sh
curl -fsSL https://carryfs.com/install.sh | bash
```

Windows, from PowerShell:

```powershell
irm https://carryfs.com/install.ps1 | iex
```

The script downloads the package and checks it against the release's
`SHA256SUMS`. It lists what it will install and writes nothing to the system
until you confirm. Then it asks for administrator permission, through `sudo` on
Linux and macOS and a UAC prompt on Windows.

### Choose a version

To select `0.1.0` explicitly, run:

```sh
curl -fsSL https://carryfs.com/install.sh | bash -s -- --version 0.1.0
```

```powershell
& ([scriptblock]::Create((irm https://carryfs.com/install.ps1))) -Version 0.1.0
```

## Use CarryFS

Plug in a CarryFS disk to mount it. To create one, follow [FORMATTING.md](FORMATTING.md).
Eject the volume before unplugging it. On Windows, the `CarryFS` service handles
mounting and safe removal.

## Install by hand

Set up the [requirements](#requirements), clone the repository, and
[build the binaries](CONTRIBUTING.md#build-without-installing) for your platform.
Run the following commands from the checkout. Eject all CarryFS volumes before
installing or replacing the adapter.

### Linux

Install the CLI, the FUSE adapter, the mount and mkfs helper links, and the
udev rule that identifies CarryFS volumes.

```sh
sudo install -D -m 755 target/release/carryfs /usr/local/bin/carryfs
sudo install -D -m 755 target/release/carryfs-fuse /usr/libexec/carryfs/carryfs-fuse
sudo ln -sf /usr/libexec/carryfs/carryfs-fuse /usr/sbin/mount.carryfs
sudo ln -sf /usr/libexec/carryfs/carryfs-fuse /usr/sbin/mkfs.carryfs
sudo install -D -m 644 fuse/packaging/61-carryfs.rules /etc/udev/rules.d/61-carryfs.rules
```

UDisks reads its CarryFS mount options from `/etc/udisks2/mount_options.conf`.
If you don't have that file, copy the one from the checkout.

```sh
sudo install -D -m 644 fuse/packaging/mount_options.conf /etc/udisks2/mount_options.conf
```

If you do have it, add these two lines under its `[defaults]` group, and
create the group if the file has none. Keep `$UID` and `$GID` as written.
UDisks fills them in.

```ini
carryfs_defaults=uid=$UID,gid=$GID
carryfs_allow=uid=$UID,gid=$GID,strict,metadata-cache,readdirplus=yes
```

Reload udev so it picks up the rule.

```sh
sudo udevadm control --reload
```

### macOS

```sh
sudo mkdir -p /usr/local/bin
sudo install -m 755 target/release/carryfs /usr/local/bin/carryfs
sudo rm -rf /Library/Filesystems/carryfs.fs
sudo cp -R build/macfuse/carryfs.fs /Library/Filesystems/
sudo killall storagekitd
```

The source build creates `build/macfuse/carryfs.fs` with the adapter and disk
helpers. The last command restarts `storagekitd`, which runs the bundle's
format and check helpers for `diskutil` and Disk Utility.

### Windows

Open PowerShell as Administrator in the checkout. Copy the built binaries
and register the service.

```powershell
$target = Join-Path $env:ProgramFiles 'CarryFS'
New-Item -ItemType Directory $target
Copy-Item target\release\carryfs.exe, target\release\carryfs-windows.exe $target
$binary = '"{0}" service' -f (Join-Path $target 'carryfs-windows.exe')
New-Service -Name CarryFS -BinaryPathName $binary -DisplayName 'CarryFS automount' -StartupType Automatic
Start-Service CarryFS
```

The `CarryFS` service mounts CarryFS disks when you plug them in and ejects
them on safe removal.

These commands are for a first install. To replace an existing one, run
`Stop-Service CarryFS`, copy the two binaries again, and run
`Start-Service CarryFS`.

## Uninstall

Run the public installer with its uninstall option. It removes a by-hand
install too; no repository checkout is needed.

Linux and macOS:

```sh
curl -fsSL https://carryfs.com/install.sh | bash -s -- --uninstall
```

Eject all CarryFS volumes first. The script stops if one is still mounted.

Windows, from PowerShell:

```powershell
& ([scriptblock]::Create((irm https://carryfs.com/install.ps1))) -Uninstall
```

On Windows the script stops the `CarryFS` service, which ejects mounted
volumes.

The script asks before it removes anything. It leaves FUSE, macFUSE, WinFsp,
and the contents of your disks alone.

## Limits and data safety

CarryFS is in public beta. Keep independent backups and use replaceable data
when testing. The 3-2-1 rule still applies: three copies, on two types of media,
with one copy offsite. Checksums verify stored bytes but do not prove that a
backup is complete or restorable.

Copy-on-write preserves the previous committed state during updates, provided
the device honors flushes. Changes that have not been synchronized can be lost
after an interruption. Checksums detect damage but do not repair files;
CarryFS has no self-healing. Restore damaged files from an independent backup.

CarryFS does not preserve platform-specific permissions, ACLs, extended
attributes, resource forks, or alternate data streams. Copying a directory
tree is not a complete system backup. To preserve that metadata, store a
metadata-preserving backup archive as an ordinary file.

Filenames are case-insensitive, so `File.txt` and `file.txt` cannot coexist in
the same directory. CarryFS can store names that Windows normally forbids,
such as `CON`, `NUL`, `COM1`, names containing `:` or `?`, and names ending in a
space or period. Windows presents these through compatible aliases; macOS and
Linux show the stored names.

Boot volumes and server workloads are outside CarryFS's scope. Encryption,
compression, snapshots, deduplication, RAID, and hard links are not supported.
Disk format revision 1 is not frozen; a future format change may require
copying files elsewhere and reformatting the partition.
