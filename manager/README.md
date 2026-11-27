# CarryFS Manager

A native macOS and Windows app for CarryFS disks and disk images, laid out like Disk Utility. The sidebar lists physical disks with their partitions, and opened disk images; **Show all disks** adds system volumes and virtual storage as an expandable disk, container, and volume hierarchy. The toolbar holds the disk tasks: **First Aid**, **Erase**, **Recover**, **Mount**/**Unmount**, **Eject**, and **Unlock**. The selected disk shows its capacity, a used/reserved/available bar, First Aid results (opening a volume checks its structure, then its file data once nothing else has run for 30 seconds; each tile also has **Check**, with progress and time left for file data), its details, and its format features, with **Files** and **History** beside **Overview**. **Software Update** at the bottom of the sidebar installs and updates CarryFS. No task opens Terminal.

## Run

Build the manager and its companion CLI together, then open the app:

```sh
cargo build --manifest-path manager/Cargo.toml --locked
manager/target/debug/carryfs-manager
```

On macOS, build a complete release app bundle containing the manager and its companion tool:

```sh
manager/scripts/bundle-macos.sh
open "manager/target/CarryFS Manager.app"
```

Keep both binaries in the app. First Aid and recovery run the companion tool.

## Administrator permission

Reading a disk directly, upgrading a disk's format, and installing CarryFS need administrator permission. The first such task, or **Unlock**, shows the standard macOS password dialog once. It starts this app's executable as a root helper that connects to a socket created by Manager in a private directory. The helper checks Manager's user and process IDs, and Manager checks that the helper is running as root. The helper can open a validated `/dev/rdiskN` node read-only and pass the descriptor back, run the installed adapter's `upgrade` on a validated partition, and run the published installer for a validated version. Everything else, including the CLI reading a device through that descriptor as `/dev/fd/0`, runs as you, so recovered files belong to you. **Lock**, or quitting the manager, stops the helper; the next task asks again.

Discovery reads disk metadata only and needs no permission. Mounting, unmounting, ejecting, and erasing go through Disk Arbitration (`diskutil`) without it.

## Work with a disk

- **First Aid** asks first, then checks the volume's structure (`carryfs fsck --plan`) and, when it is sound and the volume opens, verifies every file's data (`carryfs scrub`). The sheet shows progress, can be stopped, and reports the counts. When it finds damage it offers **Recover Files…** and, for a disk image whose repair would neither be blocked nor lose data, **Repair…** (`fsck --repair`, with a timestamped report beside the image). Disks are not repaired in place: the core's raw-device flush on macOS isn't supported yet, so rebuild them instead. Discarding data (`--accept-data-loss`) stays a command-line decision; see `RECOVERY.md`.
- **Recover** keeps the source read-only and works around damaged metadata: copy readable files, with permissions, hidden flags, and hard links, to a new folder (`fsck --recover`), or rebuild them as a new verified disk image (`fsck --repair --output --verify-data`). Each writes a report beside its destination; **Show in Finder** reveals it.
- A volume too damaged to open still appears, explains why, and offers First Aid and recovery.
- **Erase** asks for a name and shows the partition it will erase, then runs `diskutil eraseVolume CarryFS` with progress in the sheet and refreshes the disk list. Only unmounted external partitions can be erased; APFS, internal, and system storage cannot.
- **Eject** confirms that every volume on the disk is ejected. There is no forced unmount.
- **Format features** shows whether the volume stores permissions and which releases open it. A volume CarryFS 0.1.0 formatted (format revision 1) is read-only until **Upgrade Format…** publishes a revision 2 root in place, without rewriting files: a disk image through the companion tool, a disk through the installed adapter. CarryFS 0.1.0 can't open revision 2 volumes.
- **Files** browses folders read-only and exports single files to new paths. Entries show permissions, hidden entries are dimmed, and the selection shows allocated bytes and hard-link names. **Recover This Folder…** exports the current folder with `carryfs get`.
- **History** keeps the latest 100 task results of this session.

Once unlocked, selecting an unmounted CarryFS partition opens it read-only straight away. A mounted volume offers **Show in Finder**; unmount it for First Aid or recovery.

Long tasks run in child processes with bounded output capture; **Stop** ends their process group, and quitting stops them too. Stopping recovery can leave partial files at the destination. Apart from **Repair**, **Upgrade Format**, and **Erase**, the manager never writes to a source. Before using a cached listing it checks the volume UUID and generation, and it rechecks a disk's identifier, size, and UUID before acting on it, so a reconnected or replaced disk is never acted on by mistake.

## Software Update

**Software Update** shows the installed CarryFS version (from `/Library/Filesystems/carryfs.fs/Contents/VERSION`), the latest release from `https://carryfs.com/releases.txt`, and the manager's own version. **Install** runs the published installer with `--yes` through the root helper; the installer verifies the package checksum and refuses while CarryFS volumes are mounted. It never downgrades and never changes disks. Revision 1 volumes take writes after **Upgrade Format**; rolling back software doesn't undo an upgrade.

## Windows

Build the same way, from 64-bit PowerShell; `carryfs.exe` lands next to `carryfs-manager.exe`, and the manager runs it for First Aid and recovery:

```powershell
cargo build --manifest-path manager/Cargo.toml --locked
manager\target\debug\carryfs-manager.exe
```

Disks, partitions, and volumes come from IOCTLs that need no administrator rights. Windows reports a CarryFS partition as RAW, so the `CarryFS` service says which partitions it mounted and where through its control pipe, `\\.\pipe\CarryFS`, which SYSTEM, administrators, and interactive users can open and remote clients can't. **Mount** and **Unmount** of a CarryFS volume ask the service; for other file systems they add or remove the volume's drive letter. **Eject** asks Windows to safely remove the disk, so the service ejects its volumes first, and detaches a disk image. Disk arrivals and drive-letter changes refresh the list.

**Unlock** restarts the manager as administrator through UAC and reselects the disk; it keeps that permission until you quit, so there is no **Lock**. Reading a partition, **Erase**, **Upgrade Format**, and changing drive letters need it. The elevated manager opens a partition unshared, which keeps the service from mounting it meanwhile, and passes the handle to `carryfs.exe` as standard input, as on macOS. **Erase** makes a partition, or a whole disk as one GPT partition, hidden and without a drive letter, locks and dismounts what it held, formats it with the bundled core, and asks the service to mount it. **Upgrade Format** runs the installed `carryfs-windows upgrade`, and **Install** runs the published `install.ps1 -Yes`, which asks for UAC itself when the manager isn't elevated.

Copying files to a folder (**Copy to a folder** and **Recover This Folder…**) isn't available on Windows yet: the companion tool's link-safe extraction is Unix-only. **Rebuild as a disk image** and single-file export work.

The ignored live tests read this PC's disks, and erase a disposable VHD that `CARRYFS_TEST_DISK` names; that one needs administrator rights and a CarryFS service with the control pipe, and refuses any disk that isn't a VHD:

```powershell
cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager discovered_devices_can_be_reidentified -- --ignored --nocapture
$env:CARRYFS_TEST_DISK = 3
cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager a_vhd_erases_mounts_reads_and_ejects -- --ignored --nocapture
```

## Disposable demo and headless checks

Create a new image, refusing existing paths:

```sh
cargo run --manifest-path manager/Cargo.toml --locked --example demo -- /tmp/carryfs-demo.cfs
CARRYFS_IMAGE=/tmp/carryfs-demo.cfs manager/target/debug/carryfs-manager
```

Run focused tests and lint checks:

```sh
cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager
cargo clippy --manifest-path manager/Cargo.toml --locked --all-targets -- -D warnings
```

The ignored live-discovery test reads disk metadata only:

```sh
cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager \
  discovered_devices_can_be_reidentified -- --ignored --nocapture
```

Capture the actual native renderer without a window:

```sh
CARRYFS_IMAGE=/tmp/carryfs-demo.cfs manager/target/debug/carryfs-manager \
  --scene overview --scale 1 --snap /tmp/carryfs-manager.png --tree /tmp/carryfs-manager.json
CARRYFS_IMAGE=/tmp/carryfs-demo.cfs manager/target/debug/carryfs-manager \
  --scene overview --script manager/scripts/demo.json --at 3.2 --scale 1 \
  --snap /tmp/carryfs-manager-files.png --tree /tmp/carryfs-manager-files.json
```

The `overview`, `files`, `activity`, `software`, `first-aid`, and `erase` capture scenes load actual system inventory synchronously before the virtual clock starts. No fake physical devices are introduced. The demo script uses the demo generator's object IDs.

The tests use disposable CarryFS images to drive Rune's real runtime and file dialogs. They cover paginated browsing, First Aid, repair of a damaged image, recovery and rebuilds, format upgrades, exports, source-generation changes, overwrite refusal, unchanged source bytes, process cancellation, descriptor passing for the root helper, malformed device metadata, and erase guards. They do not mount, eject, erase, or update any physical device, and never start the root helper.

Rune follows its `examples/hello` state/view/AppSpec pattern and uses Rune Kit controls. The Rune revision and its required crate patches are pinned together. This package has a separate workspace and lockfile so its UI dependencies do not change the filesystem workspace's minimum Rust version. The verified compilers are Rust 1.99.0 on macOS and Rust 1.98.1 (x86_64-pc-windows-gnu) on Windows 10.
