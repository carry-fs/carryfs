# CarryFS manager progress

## Current state

- Work complete for the latest reported bugs. Native app is running with a disposable demo volume.
- Worktree: `/Users/painhardcore/Data/40_CODE/active/manager`, branch `manager`.
- Original checkout: `/Users/painhardcore/Data/40_CODE/active/carry-fs`, master. Unrelated local changes preserved.
- Publication target: `origin/manager`. User authorized committing and pushing all manager work. No merge to master requested.

## Requested product

Native Rune filesystem manager, following Rune examples/hello state/view/AppSpec. User chose device sidebar with Overview / Files / Activity, plus separate Software & updates. Scope includes image and system-device discovery, read-only browse/export/recovery, check/scrub, guarded mount/eject, explicitly confirmed formatting, installed software version/update flow, and separate on-disk upgrade messaging.

## Latest fixes

- **Missing companion tool:** `manager/scripts/bundle-macos.sh` now builds and packages BOTH `carryfs-manager` and `carryfs` into `manager/target/CarryFS Manager.app/Contents/MacOS`. Both are actual executable files. Staging and rename replace old development symlinks safely. CLI lookup canonicalizes the manager path so development symlinks also work; regression covers bundled, symlinked, and missing-tool cases.
- **Scrolling:** the actual cause was `el()` constructing a Row. Its content stretched across the viewport height, hiding overflow from Rune's direct-child measurement. Vertical device, image, file, and main-content viewports now use `col()`, explicit widths, and Rune Kit native scrollbars. Regression tests exercise wheel input on 40 synthetic devices and a real long directory.
- **Too many system entries:** default list still hides internal/system storage and unrelated virtual installers. Show system devices now exposes a collapsed hierarchy rather than a flat inventory. APFS containers link to physical-store partitions; expanding a disk reveals its partitions, containers, then volumes. This Mac shows 2 root disks instead of 22 flat rows. Collapse hides all descendants.
- **Accessibility:** workspace tabs expose selected state; source tree/listbox items expose selection; Software announces Current view.
- Eject passes the revalidated selected identifier to diskutil, letting diskutil resolve its disk rather than relying on the display hierarchy parent.

## Verified

- `cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager`: **20 passed**, 1 metadata-only live test ignored in this normal run.
- `cargo test --manifest-path manager/Cargo.toml --locked --bin carryfs-manager discovered_devices_can_be_reidentified -- --ignored --nocapture`: **passed**, reidentified 22 disks/volumes using metadata only.
- `cargo clippy --manifest-path manager/Cargo.toml --locked --all-targets -- -D warnings`: **passed**.
- `cargo fmt --manifest-path manager/Cargo.toml --check`, `git diff --check`, `bash -n manager/scripts/bundle-macos.sh`: **passed**.
- Complete bundle built successfully. Both native executables exist and are executable.
- **Actual native CUA verification on the rebuilt bundle:** Check structure passed, 10 objects / 3 metadata pages / 5 data blocks. Scrub passed, 5 files / 5 verified units / 0 unreadable ranges. Both results remain visible independently.
- Native Show system devices displayed 2 collapsed roots. Expanded disk0 / disk0s1 / disk1, then wheeled over the device viewport: the first root left the viewport and the lower Disk Image row became visible. Scrolling is verified in the actual window.
- Updated actual-renderer captures in `.impeccable/review/`: overview, overview-small, files, software-small, device-small, devices-expanded, integrity-checked, with matching JSON/logs. Desktop sizes 1120x760 and 940x640. Integrity screenshot shows BOTH Passed results.
- Final Impeccable reviewer: **ship**, no remaining material defects. Accessibility, grouping, scrollbar, settled selection and integrity results approved.
- Tests also cover read-only image bytes, paginated listing, stale-generation rejection, overwrite refusal, export/recovery, child cancellation, source-specific retained integrity, requested-source Activity attribution, unknown device kind and formatting guards.

## Run

From the manager worktree root:

```sh
manager/scripts/bundle-macos.sh
open "manager/target/CarryFS Manager.app"
```

For the existing disposable demo:

```sh
CARRYFS_IMAGE=/private/tmp/carryfs-manager-demo.cfs "manager/target/CarryFS Manager.app/Contents/MacOS/carryfs-manager"
```

Native process launched in exec session `58431`; may still be running. Prior session24554 stopped. CUA part completed with summary "Verified checks and device scrolling". A future summarized CUA session must call `await cua.rewriteDocumentation()` before continuing.

## Implementation and boundaries

- Standalone manager workspace and Cargo.lock. Root Cargo.toml excludes manager; .gitignore excludes manager/target and .impeccable/review. Rune revision `656af1d5a4d67af0c849dd25ec54e84a6736880b` with matching winit/wgpu-hal patches. Verified compiler Rust1.99.0.
- `main.rs`: UI/state/workers, guarded action confirmations, per-volume integrity keyed by UUID + generation, Activity.
- `model.rs`: opens source read-only for each operation, validates UUID/generation, browses/export without overwrite.
- `platform.rs`: diskutil plist discovery/reidentification, existing authopen for explicit raw reads, non-force device actions, stable-identity guards, exact-target formatting confirmation, installed adapter version and interactive installer.
- `operation.rs`: bundled CLI lookup, bounded child output, process-group cancellation, existing CLI safe recovery sink.
- Installed adapter version marker `/Library/Filesystems/carryfs.fs/Contents/VERSION`; release index `https://carryfs.com/releases.txt`. Software install is an explicit interactive Terminal flow, no silent installation/downgrade.
- No in-place disk-format migration exists in the core. UI says unavailable; revision1 is not frozen. Future incompatible upgrades require a separate verified copy.
- **Not exercised:** physical-device reads requiring authorization, mount/unmount/eject/format, or software installation. No physical media or installed software changed. Linux/Windows builds unverified.
- Capture scenes synchronously discover metadata for deterministic headless rendering; this debug-only capture setup logs main-thread IO warnings. Interactive startup uses workers.
- Regular-file image leases are exclusive even read-only. Avoid simultaneous headless captures opening the same image. The integrity capture was rerun sequentially with --realtime --at3 after one parallel attempt collided. Final artifacts are valid.

## Design and documentation

Impeccable4.3.1 was applied. Context already run once; don't rerun in this session. User approved updating the skill only in a later turn; no skill update performed. Native Operate mode; seed4948d08c; code-first, no generated comp. User screenshots/native utility conventions are authority. PRODUCT.md and manager/DESIGN.md preserve the design direction. The shipped documenter completed the eight-section implementation-backed DESIGN.md and .impeccable/design.json sidecar, including Rune color/type/spacing tokens and control previews. JSON and document structure validated. No remaining implementation or documentation work for the reported bugs.

Reference screenshots: `/Users/painhardcore/Data/30_MEDIA/screenshots/Screenshot 2026-11-12 at 00.53.07.png` and `...00.53.16.png`.
Disk Utility reference: https://support.apple.com/en-ca/guide/disk-utility/dskud6b39edb/mac describes Show Only Volumes versus the full device/container/volume hierarchy.

Logs: `/private/tmp/carry-manager-{tests,clippy,build,live,native}.log`.
