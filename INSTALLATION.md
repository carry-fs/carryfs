# Installing CarryFS

Install a release package after setting up the framework for your platform.
For source builds, see
[CONTRIBUTING.md](CONTRIBUTING.md#build-and-install-from-source).
If you'd rather not run the installer, [install by hand](#install-by-hand).

## Platform support

| Tier | Platform | Adapter |
|---|---|---|
| 0 | Linux | [Native kernel driver](#fuse-adapter-or-native-driver) |
| 1 | macOS, Apple Silicon | macFUSE |
| 1 | Linux, x86-64 | FUSE |
| 1 | Windows\*, x86-64 | WinFsp |
| 2 | FreeBSD\*\* 14.5 and 15.1, amd64 and arm64 | fusefs |
| 2 | NetBSD\*\* 10.2 and 11.1, amd64 | perfused / PUFFS |
| 2 | Android\*\*\* 8 and later, arm64 | [CarryFS app](android/README.md) |

- Tier 0 is a native kernel driver, the fastest option.
- Tier 1 adapters support every feature, are tuned for speed, and are tested
  on real hardware.
- Tier 2 adapters have the core features, and their performance is best effort.

\*Windows can't create new [hard links](#hard-links).

\*\*FreeBSD and NetBSD are tested only in virtual machines and can't set the
[hidden flag](#limits-and-data-safety).

\*\*\*Install the app from a release's `carryfs-VERSION-android-arm64.apk`, or
build it from source. The installers don't install it. It shows files only
through the Files app and the file picker, and can't format or repair volumes.
See its [limits](android/README.md#limits).

## Requirements

CarryFS mounts volumes through a user-space filesystem framework. The install
script stops if it can't find one.

### Linux

You need Ubuntu 24.04 or later, or Arch Linux, on x86-64. Linux desktop
integration requires systemd, [FUSE 3](https://github.com/libfuse/libfuse), and
UDisks. The installer requires a running systemd host.

```sh
# Ubuntu
sudo apt install curl fuse3 udisks2 systemd udev procps coreutils

# Arch Linux
sudo pacman -S curl fuse3 udisks2 procps-ng coreutils
```

`/dev/fuse` must exist. If it doesn't, load the kernel module with
`sudo modprobe fuse`.

#### FUSE adapter or native driver

The install script sets up the FUSE adapter. From a Linux checkout, add the
native module with:

```sh
./install.sh --local --native
```

You still need the FUSE and desktop prerequisites above, but no published
release. The installer builds both adapters before it asks for confirmation.
It installs the module at `/lib/modules/$(uname -r)/updates/carryfs.ko`,
records that kernel version, and refreshes the module index. It does not load
the module.

UDisks and `mount -t carryfs` call `mount.carryfs`, which uses FUSE even when
the native module is loaded. To mount with the native driver, load it and
bypass the helper with `-i`:

```sh
sudo modprobe carryfs
sudo mount -i -t carryfs DEVICE DIRECTORY
```

| Distribution | Kernel | Native driver |
|---|---|---|
| Ubuntu 26.04 | 7.0 | Yes |
| Fedora 45 | 7.2 | Yes |
| Arch Linux | 7.2 | Yes |
| Others | | If the kernel is 7.0 or newer and built with Rust support |

The table lists where the module builds. The installer supports only Ubuntu
and Arch, so on other distributions build and load the module by hand, as
[linux/README.md](linux/README.md) describes. The driver needs 4 KiB kernel
pages and rejects `sync` and `dirsync` mounts.

The installer builds unsigned modules. It refuses to install one when
signature enforcement or kernel lockdown demands a trusted signature. Nothing
rebuilds the module after a kernel upgrade, so boot the new kernel and run
`./install.sh --local --native` again.

### FreeBSD

CarryFS supports FreeBSD 15.1 and 14.5 on amd64 and arm64, with a separate
release package for FreeBSD 14 and 15. Load the base system's FUSE module:

```sh
sudo pkg install curl
sudo kldload fusefs
```

To load it at boot, add `fusefs_load="YES"` to `/boot/loader.conf`. You
don't need a libfuse package. Install with the same `install.sh` command as on
Linux and macOS, or build a checkout with `./install.sh --local`. Source
builds need Rust 1.89 or newer, from `pkg install rust`.

The installer places the CLI in `/usr/local/bin/carryfs` and the helpers in
`/usr/local/sbin`. Mount an existing CarryFS partition with:

```sh
sudo mkdir -p /mnt/carryfs
sudo mount_carryfs -o uid=$(id -u),gid=$(id -g) /dev/da0p1 /mnt/carryfs
sudo umount /mnt/carryfs
```

To use mount(8) or fstab, name the helper with `mountprog`:

```sh
sudo mount -t carryfs -o mountprog=/usr/local/sbin/mount_carryfs /dev/da0p1 /mnt/carryfs
```

A corresponding `/etc/fstab` entry is:

```fstab
/dev/da0p1 /mnt/carryfs carryfs rw,noauto,mountprog=/usr/local/sbin/mount_carryfs 0 0
```

To create a volume, see [FORMATTING.md](FORMATTING.md#freebsd-and-netbsd).
To run the adapter in the foreground, use
`/usr/local/libexec/carryfs/carryfs-fuse serve DEVICE DIRECTORY OPTIONS`.

FreeBSD's FUSE can't invalidate cached names the way case-insensitive names
need, so CarryFS turns name caching off there and uses ordinary directory
listings.

CarryFS tools lock both the canonical provider and the volume's UUID. Every name
for a partition, including GEOM labels such as `/dev/gpt/NAME`, therefore shares
one lock. The locks are advisory. A program that writes the raw device without
taking them, such as one run as root, can still overwrite a mounted volume.
Writable GEOM opens also conflict with writable mounts of other filesystems, so
unmount those before CarryFS opens the provider, even for offline inspection.

### NetBSD

CarryFS supports NetBSD 10.2 and 11.1 on amd64, with a separate package for
NetBSD 10 and 11. The adapter serves FUSE requests through PUFFS with the base
system's `libperfuse` and `perfused`. The GENERIC kernel includes PUFFS, and
custom kernels need `file-system PUFFS`. You don't need a libfuse package.

Install with `install.sh`, or build a checkout with `./install.sh --local`.
Source builds need Rust 1.89 or newer, from `pkgin install rust-bin`. The CLI
goes in `/usr/local/bin`. The mount and format helpers go in `/usr/sbin`,
because NetBSD's `mount(8)` only searches the system helper directories.

Use an unmounted raw disk or partition node, such as `/dev/rdk2` for a GPT
wedge. A block node or a symlink to the same partition takes the same
raw-device lease:

```sh
sudo newfs_carryfs -L Shared /dev/rdk2
sudo mkdir -p /mnt/carryfs
sudo mount -t carryfs /dev/rdk2 /mnt/carryfs
sudo umount /mnt/carryfs
```

For an optional fstab entry:

```fstab
/dev/rdk2 /mnt/carryfs carryfs rw,noauto 0 0
```

The disk's driver must support cache flushes. CarryFS reports a flush the driver
rejects or fails as an error. CarryFS tools take advisory leases on the raw
device and on the volume UUID, which other programs that write the raw device
must honor. NetBSD caches names and attributes conservatively and uses ordinary
directory listings. Changes become durable at fsync, as on the other platforms.
The NetBSD adapter doesn't support the hidden flag or rename exchange.

The adapter loads a compatibility shim for libperfuse into its relay process,
and only there. The shim handles access checks and root ownership, forwards
directory fsync after a create or link, stops libperfuse from syncing every file
on close, and keeps the relay running until the kernel finishes an unmount. The
adapter refuses to mount if libperfuse's node layout differs from the one in
NetBSD 10 and 11.

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

The install script installs the latest stable release. Both installers read
the published versions from `https://carryfs.com/releases.txt` and download
the package from GitHub Releases. The Unix installer doesn't need `jq`.

An existing installation stays on its release line, which is its major
version, or `0.MINOR` before 1.0. A new 0.x minor release can change the disk
format, so moving to it takes `--upgrade` on Unix or `-Upgrade` on Windows.
[VERSIONING.md](VERSIONING.md) explains why.

Linux, macOS, FreeBSD, and NetBSD:

```sh
curl -fsSL https://carryfs.com/install.sh | bash
```

Windows, from 64-bit PowerShell:

```powershell
irm https://carryfs.com/install.ps1 | iex
```

The script downloads the package and checks it against the release's
`SHA256SUMS`. It lists what it will install and writes nothing until you
confirm. Then it asks for administrator permission, through `sudo` on Unix
and a UAC prompt on Windows.

### Choose a version

To install a specific version, such as 0.2.0, pass it to the script:

```sh
curl -fsSL https://carryfs.com/install.sh | bash -s -- --version 0.2.0
```

```powershell
& ([scriptblock]::Create((irm https://carryfs.com/install.ps1))) -Version 0.2.0
```

## Use CarryFS

Plug in a CarryFS disk to mount it, and eject the volume before you unplug
it. On Windows, the `CarryFS` service mounts disks and handles safe removal.
To create a CarryFS disk, follow [FORMATTING.md](FORMATTING.md).

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

Eject all CarryFS volumes first. The Unix script stops if one is still
mounted. On Windows the script stops the `CarryFS` service, which ejects
mounted volumes.

Linux, macOS, FreeBSD, and NetBSD:

```sh
curl -fsSL https://carryfs.com/install.sh | bash -s -- --uninstall
```

Windows, from 64-bit PowerShell:

```powershell
& ([scriptblock]::Create((irm https://carryfs.com/install.ps1))) -Uninstall
```

For a source installation, you can also run `./install.sh --uninstall` from
the checkout. Uninstalling also removes userspace files you installed by hand.

The script asks before it removes anything. It leaves FUSE, macFUSE, WinFsp,
and the contents of your disks alone. If installing or uninstalling fails,
the script puts the previous files back. If that fails too, it prints where
it kept the backup.

On Linux, uninstalling also removes every native module the installer
recorded, including modules for older kernels, and refreshes each affected
module index. It leaves other modules and your UDisks settings alone. It
doesn't know about a module you installed by hand, so it leaves that one in
place. If the native module is loaded, unmount its volumes and unload it
before you install or uninstall:

```sh
sudo modprobe -r carryfs
```

## Limits and data safety

CarryFS is in public beta. Keep independent backups, and test with data you
can replace. The 3-2-1 rule still applies: three copies, on two types of
media, with one copy offsite. Checksums prove that stored bytes are intact,
not that a backup is complete or can be restored.

CarryFS writes changes to new blocks, so the last committed state survives an
interruption as long as the device honors flushes. Changes not yet synced can
be lost. Checksums detect damage but don't repair it, and CarryFS keeps no
second copy to heal from. Restore damaged files from a backup. For damaged
metadata, use [offline diagnosis, repair, and recovery](RECOVERY.md).

A volume stores each object's Unix permission bits (`rwxrwxrwx`) and a hidden
attribute. Linux, macOS, FreeBSD, and NetBSD show and enforce the permission
bits for the account that mounted the volume. Windows shows a file without the
owner write bit as read-only, and setting read-only there clears the write bits.
Hidden maps to Finder's hidden flag and to Windows' hidden attribute. FreeBSD's
FUSE drops file flags, so `chflags hidden` has no effect there. Windows also
shows macOS's own bookkeeping files, such as `.DS_Store` and `._name`, as
hidden. For images, `carryfs put` and `carryfs get` keep modes, and
`carryfs attr IMAGE /PATH [mode=OCTAL] [hidden=yes|no]` shows or changes them.

A volume that CarryFS 0.1.0 formatted mounts read-only. To write to it,
upgrade it with `carryfs-fuse upgrade DEVICE` on Linux, macOS, FreeBSD, and
NetBSD, `carryfs-windows upgrade VOLUME` on Windows, or
`carryfs upgrade IMAGE` for an image. The upgrade writes one root record and
no file data. CarryFS 0.1.0 can't mount an upgraded volume, or one a newer
CarryFS formatted, so update CarryFS on every computer that uses the volume
first.

Linux `fallocate`, macOS `F_PREALLOCATE`, and a Windows allocation size,
given at create or set later, reserve space. Reserved space reads as zeros
until you write to it.

CarryFS doesn't store owners, setuid, setgid, or sticky bits, ACLs, extended
attributes, resource forks, or alternate data streams. It isn't a security
boundary either, since anyone holding the drive can mount it as themselves.
Copying a directory tree onto it is therefore not a complete system backup. To
keep that metadata, store a backup archive that preserves it as an ordinary
file.

Filenames are case-insensitive, so `File.txt` and `file.txt` cannot coexist in
the same directory. CarryFS can store names that Windows normally forbids,
such as `CON`, `NUL`, `COM1`, names containing `:` or `?`, and names ending in a
space or period. Windows shows these under compatible aliases. macOS and
Linux show the stored names.

Boot volumes and server workloads are outside CarryFS's scope. It has no
encryption, compression, snapshots, deduplication, or RAID.
The current disk format is revision 2. A future format change may require
copying files elsewhere and reformatting the partition.

### Hard links

Files and symlinks can have up to 65,535 names each, made with `ln` on
Linux, macOS, FreeBSD, and NetBSD. Directories can't have hard links.

On the macFUSE kernel extension backend, Finder can show a different name
for a hard link and trash or delete the wrong name. Use `mv` or `rm` with
the exact path in Terminal when renaming or removing hard links on macOS.
See [macFUSE issue #1239](https://github.com/macfuse/macfuse/issues/1239).

Windows opens, renames, and deletes existing names. It can't create new ones,
because WinFsp rejects hard link creation. WinFsp also reports a link count of
1 for every file, so Windows tools don't show a file's other names.
`carryfs put` and `carryfs get` keep the hard links inside the copied tree;
if the destination cannot link, `get` writes a separate copy.
