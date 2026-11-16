# CarryFS Manager

A native Rune manager for system devices and CarryFS image files. The default sidebar lists external storage, CarryFS volumes, and opened images. Internal/system storage and unrelated virtual disk images are available through **Show system devices**, grouped under expandable disks and APFS containers. The device list supports wheel scrolling and a native scrollbar. Each source has **Overview**, **Files**, and **Activity** tabs. A volume whose metadata is too damaged to open still appears, with diagnosis and recovery. **Software & updates** is separate from volume operations.

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

Keep both binaries in the app. Check, scrub, and recovery use the companion tool.

Use **Refresh** to discover attached devices. Discovery reads macOS disk metadata without opening raw disks or asking for administrator access. Select a device to see its identity, capacity, filesystem, and mount state. **Open read-only** opens an unmounted source for inspection and may request administrator approval through macOS authopen.

Unknown device kind is shown as unknown; omitted metadata is not treated as virtual. Device discovery and system operations currently support macOS. Image browsing remains available on other platforms, but Linux and Windows builds have not been verified. Folder recovery uses the existing Unix CLI extraction implementation.

## Work with a volume

- **Overview** shows capacity, available space, maintenance reserve, format revision, format features, and per-volume integrity results with their counts (objects, metadata pages, data and free blocks; scrubbed files, verified units, unreadable ranges). **Show volume details** reveals the source path, UUID, and generation. **Check structure** validates metadata and ownership. **Scrub file data** also checks backed file contents. **Diagnose damage** runs `carryfs fsck --plan` and lists what an offline repair would change. All three are read-only.
- **Recovery** works around damaged metadata and keeps the source read-only. **Recover files…** exports accessible files with their permissions, hidden flags and hard links to a new folder (`fsck --recover`). **Rebuild into new image…** copies them into a new image and verifies its data (`fsck --repair --output --verify-data`). Each writes a report beside its destination. After a diagnosis that is neither blocked nor losing data, an image offers **Repair in place…**, which runs `fsck --repair` with a timestamped report beside the image and reopens it. Devices are not repaired in place; rebuild them instead. Discarding data (`--accept-data-loss`) stays a command-line decision; see `RECOVERY.md`.
- **Format features** shows whether the volume stores permissions and the hidden flag, whether preallocation is enabled, and whether hard links are used. Any of them stops CarryFS 0.1.0 from mounting the volume. **Upgrade volume…** adds permissions and the hidden flag in place after an inline confirmation: an image is upgraded by the companion tool (`carryfs upgrade`) and reopened; a device opens a Terminal review that asks for its identifier again, checks its UUID, and runs the installed adapter's `upgrade` with administrator approval. Preallocation and hard links are never turned on by the manager; they turn on when software reserves space or links a file on a mounted volume.
- **Files** browses directories and exports regular files to new paths. Each entry shows its permissions; hidden entries are dimmed. The header counts the folder's entries, file bytes, and allocated bytes; the selection shows allocated bytes, the hidden flag, and how many names a hard-linked file has. **Recover this folder…** exports the current folder with `carryfs get`. Double-click a folder, or select it and choose **Open folder**. Symlinks are listed but are not followed by the browser. **Recover this folder…** extracts the current folder using the CLI's descriptor-relative destination handling, including symlinks and reports of unreadable ranges.
- **Activity** retains the latest 100 operation results in this session, with the source and completion time. Incomplete recovery is reported as incomplete, including when the CLI returns partial output.

Checks, scrubs, and recovery run in child processes with bounded output capture. **Cancel operation** stops their process group. Closing the manager also stops an active cancellable process. Cancellation can leave incomplete recovery files; the source stays read-only. Single-file export runs on a worker thread and does not offer cancellation.

Apart from **Upgrade volume…** and **Repair in place…**, the manager never writes to a source. Images are opened only for the duration of each operation. Before using a cached listing, the manager checks the volume UUID and generation. If either changed, reopen the image to refresh it. The selected device's identifier, size, and available UUID are rechecked before device actions. Disconnected devices do not silently redirect operations to a replacement device.

## Mount, eject, and format

External CarryFS volumes offer **Mount** or **Unmount** through diskutil. **Eject device…** explicitly identifies that other volumes on the same disk will also be ejected. There is no force-unmount path. Internal/system sources are protected from these changes. A source without a stable reported identity cannot authorize an action.

Formatting has a separate inline flow. It requires an unmounted external physical partition, its exact identifier, and the installed CarryFS formatter. APFS containers and their stores are excluded. After the identifier is entered, **Review formatting in Terminal…** opens an interactive script that shows disk information, asks for the identifier again, checks its UUID, and invokes `diskutil eraseVolume CarryFS` on that partition. The manager does not report formatting as complete merely because Terminal opened. Refresh devices after the system tool finishes.

Formatting destroys files. It is never used as an implicit format upgrade.

## Versions and updates

**Software & updates** distinguishes the manager version, bundled core version, installed macOS adapter version, latest stable release, and supported on-disk revision. The installed adapter version comes from `/Library/Filesystems/carryfs.fs/Contents/VERSION`; it is not inferred from the manager's build.

**Check for updates** reads the project's stable release index. **Review installation…** opens the existing release installer in Terminal with an exact version. That installer verifies the package checksum, requires mounted adapters to be stopped, and asks for installation and administrator confirmation. The manager does not install anything automatically and does not offer a downgrade as an update. Refresh the installed version after the installer finishes.

Revision 1 volumes gain features in place through root feature bits; no file is rewritten. Neither feature can be turned off, and rolling back software does not undo one. Update CarryFS on every computer that uses a volume before upgrading it. A volume using a feature this manager does not know is refused with a request to update the manager.

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

The `overview`, `files`, `activity`, and `software` capture scenes load actual system inventory synchronously before the virtual clock starts. No fake physical devices are introduced. The demo script uses the demo generator's object IDs.

The tests use disposable CarryFS images to drive Rune's real runtime and file dialogs. They cover paginated browsing, checks, scrubs, recovery, exports, source-generation changes, overwrite refusal, unchanged source bytes, process cancellation, malformed device metadata, and formatting guards. They do not mount, eject, format, or update any physical device.

Rune follows its `examples/hello` state/view/AppSpec pattern and uses Rune Kit controls. The Rune revision and its required winit/wgpu-hal patches are pinned together. This package has a separate workspace and lockfile so its UI dependencies do not change the filesystem workspace's minimum Rust version. The verified compiler is Rust 1.99.0 on macOS.
