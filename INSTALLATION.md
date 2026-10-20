# Installing CarryFS

CarryFS has no stable release yet. You can install a prerelease package, or
build from source with the install script's `--local` option.
If you'd rather not run the script, [install by hand](#install-by-hand).

## 1. Install the FUSE framework

CarryFS mounts volumes through a user-space filesystem framework. The install
script stops if it can't find one.

### Linux

You need Ubuntu 24.04 or later, or Arch Linux, on x86-64 with systemd. Install
[FUSE 3](https://github.com/libfuse/libfuse) and UDisks.

```sh
# Ubuntu
sudo apt install fuse3 udisks2 systemd udev procps coreutils

# Arch Linux
sudo pacman -S fuse3 udisks2 procps-ng coreutils
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

## 2. Install a prerelease package

Run the install script with the exact version. Prereleases are named like
`0.1.0-rc.1`, and the
[releases page](https://github.com/carry-fs/carryfs/releases) lists them.
Without `--version`, the script installs the latest stable release and ignores
prereleases.

Linux and macOS:

```sh
curl -fsSL https://carryfs.com/install.sh | bash -s -- --version 0.1.0-rc.1
```

Windows, from PowerShell:

```powershell
& ([scriptblock]::Create((irm https://carryfs.com/install.ps1))) -Version 0.1.0-rc.1
```

The script downloads the package and checks it against the release's
`SHA256SUMS`. It lists what it will install and writes nothing to the system
until you confirm. Then it asks for administrator permission, through `sudo` on
Linux and macOS and a UAC prompt on Windows.

To build from source instead, skip this step and do steps 3 to 5.

## 3. Install Rust

The build needs Rust 1.89 or later. Install it with
[rustup](https://rustup.rs/).

## 4. Clone the repository

```sh
git clone https://github.com/carry-fs/carryfs.git
cd carryfs
```

## 5. Build and install from source

Eject any mounted CarryFS volumes first. The script stops if one is still
mounted.

Linux and macOS:

```sh
./install.sh --local
```

Windows, from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Local
```

The script checks the prerequisites, builds the checkout, and lists what it
will install. It writes nothing to the system until you confirm. Then it asks
for administrator permission, through `sudo` on Linux and macOS and a UAC
prompt on Windows.

Plug in a CarryFS disk to mount it. To make one, see
[FORMATTING.md](FORMATTING.md).

## Install by hand

The install script runs these steps for you. To run them yourself, finish
steps 1, 3, and 4, then run the commands for your platform from the checkout.

### Linux

Build, then install the CLI, the FUSE adapter, the mount and mkfs helper
links, and the udev rule that identifies CarryFS volumes.

```sh
cargo build --release --locked -p carryfs-core -p carryfs-fuse
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
cargo build --release --locked -p carryfs-core
bash fuse/packaging/macos-bundle.sh
sudo mkdir -p /usr/local/bin
sudo install -m 755 target/release/carryfs /usr/local/bin/carryfs
sudo rm -rf /Library/Filesystems/carryfs.fs
sudo cp -R build/macfuse/carryfs.fs /Library/Filesystems/
sudo killall storagekitd
```

`macos-bundle.sh` builds the adapter and the disk helpers, then assembles them
into `build/macfuse/carryfs.fs`. The last command restarts `storagekitd`, which
runs the bundle's format and check helpers for `diskutil` and Disk Utility.

### Windows

Build in a normal PowerShell.

```powershell
cargo build --release --locked -p carryfs-core -p carryfs-windows
```

Then open PowerShell as Administrator in the checkout. Copy the binaries and
register the service.

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

Run the install script with `--uninstall`. It removes a by-hand install too.

Linux and macOS:

```sh
./install.sh --uninstall
```

Eject all CarryFS volumes first. The script stops if one is still mounted.

Windows, from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall
```

On Windows the script stops the `CarryFS` service, which ejects mounted
volumes.

The script asks before it removes anything. It leaves FUSE, macFUSE, WinFsp,
and the contents of your disks alone.
