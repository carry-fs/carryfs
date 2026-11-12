# CarryFS Manager

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

Native desktop application built with Rust and Rune. The current implementation is verified on macOS. Linux and Windows are intended desktop targets and require their own device and installation adapters. This is not an iOS or Android application.

## Users

People managing CarryFS storage attached to their computer, including the filesystem's developer. They need to identify the correct volume, understand its state, and perform maintenance without losing files.

## Product purpose

Manage physical storage and image files from one native application. Make device identity, mount state, filesystem compatibility, and operation results visible before users act.

## Operating context

CarryFS runs through macFUSE on macOS, FUSE on Linux, and WinFsp on Windows. The repository also contains an experimental Linux native driver. Physical disks can contain multiple partitions; selecting a disk and selecting a filesystem volume are different actions.

## Capabilities and constraints

The approved next scope includes system device discovery, image files, structural checks, data scrubs, recovery/export, and mount/eject. Formatting belongs in a separate flow with explicit confirmation of the destructive target.

Software updates and on-disk format upgrades are separate requirements. The manager must show installed CarryFS software and volume format information separately. The manager's bundled core version is not proof of the installed adapter version.

Current core APIs support read-only inspection and extraction. There is no in-place repair or format migration API. Format revision 1 is not frozen. Unsupported revisions and features are rejected. A format migration must not be presented as available until an implemented and verified migration exists.

Existing installers use version markers, verify release package checksums, and require mounted CarryFS volumes to be ejected or adapters stopped before replacing software. The manager must preserve these constraints.

The initial manager already opens images read-only, browses folders, exports files to new destinations, and checks structure. Preserve those capabilities.

## Evidence on hand

The current native Rune implementation is in manager/src. manager/README.md documents its behavior and verification commands.

The user supplied layout references at /Users/painhardcore/Data/30_MEDIA/screenshots/Screenshot 2026-11-12 at 00.53.07.png and /Users/painhardcore/Data/30_MEDIA/screenshots/Screenshot 2026-11-12 at 00.53.16.png. These show a device sidebar, a selected-volume overview, integrity operations, recovery, eject, and diagnostics.

The repository's FORMAT.md, INSTALLATION.md, and packaging scripts define actual format compatibility and installation behavior. Displayed device identity, capacity, version, and scan results must come from real observations. Demonstration data must be labeled.

## Product principles

- Identify the exact disk and volume before any operation.
- Keep the source intact during inspection and recovery.
- Report verified, incomplete, unsupported, busy, and failed states distinctly.
- Treat software replacement and data-format migration as separate operations.
- Prefer native controls and concise operation results over decorative UI.

## Open decisions

The structure is being shaped before implementation. The initial supported operating systems for physical-device operations, update rollback guarantees, migration implementation, and scan progress/cancellation remain to be specified against available platform and core capabilities.
