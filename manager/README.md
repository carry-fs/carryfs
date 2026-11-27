# CarryFS Manager

CarryFS Manager checks, recovers, erases, mounts, and upgrades CarryFS disks
and disk images, and installs CarryFS updates. It is laid out like Disk
Utility, and no task opens Terminal. It works with physical disks on macOS
only. The Linux and Windows builds open disk image files.

The sidebar lists physical disks with their partitions, and the disk images
you opened. **Show all disks** adds system volumes and virtual storage as a
tree of disks, containers, and volumes. **Software Update** sits at the
bottom of the sidebar.

The toolbar holds the disk tasks: **First Aid**, **Erase**, **Recover**,
**Mount** or **Unmount**, **Eject**, and **Unlock**.

The selected disk shows its capacity, a bar of used, reserved, and available
space, its First Aid results, its details, and its format features. Tabs for
**Files** and **History** sit beside **Overview**. Opening a volume checks its
structure, and once nothing else has run for 30 seconds, its file data. Each
result tile also has a **Check** button, which shows progress and time left
for the file data.

## Run

Build the manager and its companion CLI together, then open the app:

```sh
cargo build --manifest-path manager/Cargo.toml --locked
manager/target/debug/carryfs-manager
```

On macOS, build a release app bundle that holds the manager and the companion
CLI:

```sh
manager/scripts/bundle-macos.sh
open "manager/target/CarryFS Manager.app"
```

First Aid and recovery run the companion `carryfs`, so keep both binaries
together.

## Administrator permission

Reading a disk directly, upgrading a disk's format, and installing CarryFS
need administrator permission. The first such task, or **Unlock**, shows the
standard macOS password dialog once.

The dialog starts the manager's own executable as a root helper. The helper
connects to a socket that the manager created in a private directory. The
helper checks the manager's user and process IDs, and the manager checks that
the helper runs as root. The helper can do three things, each on a validated
target:

- Open a `/dev/rdiskN` node read-only and pass the descriptor back.
- Run the installed adapter's `upgrade` on a partition.
- Run the published installer for a version.

Everything else runs as you, including the CLI reading a device through that
descriptor as `/dev/fd/0`, so recovered files belong to you. **Lock**, or
quitting the manager, stops the helper, and the next task asks again.

Discovery reads disk metadata only and needs no permission. Mounting,
unmounting, ejecting, and erasing go through Disk Arbitration (`diskutil`)
and need none either.

## Work with a disk

- **First Aid** asks first, then checks the volume's structure
  (`carryfs fsck --plan`). If the structure is sound and the volume opens, it
  also verifies every file's data (`carryfs scrub`). The sheet shows progress
  and the counts, and you can stop it. When it finds damage it offers
  **Recover Files…**. For a disk image whose repair is neither blocked nor
  lossy, it also offers **Repair…**, which runs `fsck --repair` and writes a
  timestamped report beside the image. It doesn't repair disks in place,
  because the core can't flush a raw device on macOS yet, so rebuild them
  instead. Accepting data loss (`--accept-data-loss`) stays a command-line
  decision; see [RECOVERY.md](../RECOVERY.md).
- **Recover** keeps the source read-only and works around damaged metadata.
  It either copies the readable files, with permissions, hidden flags, and
  hard links, to a new folder (`fsck --recover`), or rebuilds them as a new
  verified disk image (`fsck --repair --output --verify-data`). Each writes a
  report beside its destination, and **Show in Finder** reveals it.
- A volume too damaged to open still appears. It says why it can't open, and
  offers First Aid and recovery.
- **Erase** asks for a name, shows the partition it will erase, and runs
  `diskutil eraseVolume CarryFS` with progress in the sheet. It erases only
  unmounted external partitions, never APFS, internal, or system storage.
- **Eject** ejects every volume on the disk and confirms it did. It never
  forces an unmount.
- **Format features** shows whether the volume stores permissions and which
  releases open it. A volume that CarryFS 0.1.0 formatted (format revision 1)
  stays read-only until **Upgrade Format…** writes a revision 2 root in place,
  without rewriting files. The companion CLI upgrades disk images, and the
  installed adapter upgrades disks. CarryFS 0.1.0 can't open revision 2
  volumes.
- **Files** browses folders read-only and exports single files to new paths.
  Entries show their permissions, hidden entries are dimmed, and the selection
  shows allocated bytes and hard-link names. **Recover This Folder…** exports
  the current folder with `carryfs get`.
- **History** keeps the latest 100 task results of this session.

Once you unlock, selecting an unmounted CarryFS partition opens it read-only
right away. A mounted volume offers **Show in Finder**. Unmount it for First
Aid or recovery.

Long tasks run in child processes with bounded output capture. **Stop** ends
their process group, and quitting stops them too. Stopping a recovery can
leave partial files at the destination.

The manager writes to a source only for **Repair**, **Upgrade Format**, and
**Erase**. Before it uses a cached listing, it checks the volume UUID and
generation. Before it acts on a disk, it checks the disk's identifier, size,
and UUID again, so it never acts on a disk that was reconnected or replaced.

## Software Update

**Software Update** shows the installed CarryFS version, read from
`/Library/Filesystems/carryfs.fs/Contents/VERSION`, the latest release from
`https://carryfs.com/releases.txt`, and the manager's own version. **Install**
runs the published installer with `--yes` through the root helper. The
installer verifies the package checksum and refuses to run while CarryFS
volumes are mounted. It never downgrades and never touches disks. Revision 1
volumes accept writes only after **Upgrade Format**, and rolling back the
software doesn't undo an upgrade.

## Disposable demo and headless checks

Create a new demo image. The example refuses a path that exists.

```sh
cargo run --manifest-path manager/Cargo.toml --locked --example demo -- /tmp/carryfs-demo.cfs
CARRYFS_IMAGE=/tmp/carryfs-demo.cfs manager/target/debug/carryfs-manager
```

Run the tests and lint checks:

```sh
cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager
cargo clippy --manifest-path manager/Cargo.toml --locked --all-targets -- -D warnings
```

The ignored live-discovery test reads disk metadata only:

```sh
cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager \
  discovered_devices_can_be_reidentified -- --ignored --nocapture
```

Capture the real renderer's output without opening a window:

```sh
CARRYFS_IMAGE=/tmp/carryfs-demo.cfs manager/target/debug/carryfs-manager \
  --scene overview --scale 1 --snap /tmp/carryfs-manager.png --tree /tmp/carryfs-manager.json
CARRYFS_IMAGE=/tmp/carryfs-demo.cfs manager/target/debug/carryfs-manager \
  --scene overview --script manager/scripts/demo.json --at 3.2 --scale 1 \
  --snap /tmp/carryfs-manager-files.png --tree /tmp/carryfs-manager-files.json
```

The capture scenes are `overview`, `files`, `activity`, `software`,
`first-aid`, and `erase`. Each loads the real system inventory before the
virtual clock starts, and none adds fake physical devices. The demo script
uses the object IDs that the demo generator writes.

The tests drive Rune's real runtime and file dialogs against disposable
CarryFS images. They cover paginated browsing, First Aid, repair of a damaged
image, recovery and rebuilds, format upgrades, exports, source-generation
changes, overwrite refusal, unchanged source bytes, process cancellation,
descriptor passing for the root helper, malformed device metadata, and erase
guards. They never mount, eject, erase, or update a physical device, and
never start the root helper.

The UI follows Rune's `examples/hello` state, view, and AppSpec pattern and
uses Rune Kit controls. The Rune revision and the crate patches it needs are
pinned together. The manager is a separate Cargo workspace with its own
lockfile, so its UI dependencies don't raise the filesystem workspace's
minimum Rust version. It is tested with Rust 1.99.0 on macOS.
