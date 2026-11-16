# Recover and repair a CarryFS volume

Unmount or eject the volume before diagnosis, export, or repair. Keep an image
of the damaged device if you need to preserve evidence or try another recovery
method later. In-place repair changes the original filesystem. New-image recovery
keeps the source read-only.

Unix installations provide `fsck.carryfs`. From a checkout, build the core with
`cargo build --locked -p carryfs-core` and use `target/debug/fsck-carryfs`.
`carryfs fsck` accepts the same options. On Windows, use `carryfs.exe fsck` for
image diagnosis, repair, and recovery to a new image. Directory export requires
Linux, macOS, or FreeBSD.

## Diagnose without writing

Run the checker with no options, or request the repair plan explicitly:

```sh
fsck.carryfs --plan volume.cfs
```

Read the reported findings and proposed changes. Diagnosis does not modify the
volume or create a report unless you supply `--report NEW_FILE`. A plan describes
structural repairs. Run `carryfs scrub volume.cfs` separately to verify file data
when the metadata is readable.

## Export accessible files

To recover files before modifying the source, choose a new destination on another
filesystem:

```sh
fsck.carryfs --recover recovered --report recovery.txt volume.cfs
```

Inspect `recovery.txt` even when some files look intact. The report lists missing
metadata and incomplete mappings. It also records data ranges that failed checksum
verification. Unreadable ranges remain unwritten in partial files. Those ranges
are missing data, and must not be treated as verified zeros.

Recovery continues through independently valid catalog branches. Files without
usable directory entries appear under `lost+found`, named by their original
object IDs. If the source already has a file named `lost+found`, recovery uses
`lost+found.1` or the next available suffix. Directory cycles are broken and the
disconnected directory is reconnected with its surviving children.

The destination and report must be new. Existing files, including symlinks and
source aliases, are refused. Without `--report`, export writes a report beside
the destination with `.recovery.txt` appended to its name.

## Recover into a new image

To rebuild accessible files without modifying the damaged source, choose a new
image filename:

```sh
fsck.carryfs --repair --output recovered.cfs volume.cfs
```

The tool opens the source read-only and creates a sparse CarryFS image with the
same byte length. It copies checksum-verified data through the normal filesystem
writer, rebuilding allocation and metadata independently of the source's writable
workspace. Existing output files, symlinks, and source aliases are refused.
A clean source can also be rebuilt this way.

Every new image contains `RECOVERY-REPORT.json` at its root. If that name or its
working filename conflicts with a recovered entry, the tool chooses
`RECOVERY-REPORT.1.json` or the next available suffix. Existing source files keep
their names and contents. The exact same JSON is saved beside the image as
`recovered.cfs.recovery.json`. Both sidecars must be new files.

The manifest records the source UUID and generation, destination UUID, recovery
plan, per-file paths and object IDs, copied bytes, missing source ranges, and
output verification failures. A file is `partial` when a known range is missing,
`unknown` when metadata completeness cannot be established, and `damaged` when
destination verification detects bad output data. Damage findings can overlap.
A clean scrub does not change a partial or unknown source status.

Once file copying begins, the embedded report is durable with state
`in_progress`. Complete reports replace it through an atomic rename. Interrupted
updates retain the earlier report and can leave a `.working` file. A completed
report has state `complete`, `partial`, or `verification_failed`. Generated
manifest files are excluded from recovered object and file totals.

The new image retains the volume label, recovered paths, hard links, symlink
targets, timestamps, permission bits, and hidden attributes. It receives a fresh UUID
and assigns object IDs during reconstruction. Sparse file contents are retained.
Unwritten preallocation is not copied.

Without `--report`, the report is `recovered.cfs.recovery.txt`. To choose another
report path, supply `--report NEW_FILE`. The report is synchronized before the
image is created and records missing metadata and unreadable file ranges, including
their recovered paths. Review this report even when the new image passes scrub:
missing data remains unwritten and reads as zeros in partial files.

To read back and verify destination data after copying and flushing, add
`--verify-data`:

```sh
fsck.carryfs --repair --output recovered.cfs --verify-data volume.cfs
```

This checks the backed ranges of recovered regular files and reads back the final
manifest. It does not treat source gaps as verified original zeros. The manifest
records verification as `not_requested`, `pending`, `passed`, or `failed`.
Destination data failures produce `verification_failed`, retain both damage
reports, and exit with status 8. Source damage still produces a partial result
with status 4 even when all surviving output data verifies successfully.

New-image recovery does not require `--accept-data-loss` because the source is
preserved. Complete recovery exits with status 1. Partial recovery retains the
new image and report and exits with status 4. A write or flush failure exits with
status 8 and may leave a partial image. A report without the image recovery
completion entry records an unfinished attempt. Retry with new output and report
filenames. The tool reads back and structurally checks the completed image
through its held device before it reports completion.

## Repair the original volume

After reviewing the plan and exporting what you need, run:

```sh
fsck.carryfs --repair --report repair.txt volume.cfs
```

Repair reconnects recoverable objects and rebuilds the catalog, allocation map,
and cleanup tree. File bytes, object IDs, the volume UUID, timestamps, modes,
symlink targets, and preallocated extents are retained where their metadata
survives. Freed space includes obsolete metadata and pending cleanup allocations.
Repair completes pending deletions that have matching orphan markers.
The report is synchronized before repair starts. Without `--report`, image repair
appends `.repair.txt` to the source name.

If the plan lists inaccessible catalog pages or incomplete file mappings, ordinary
repair refuses to discard them. After saving the source and reviewing the listed
losses, you can explicitly permit that loss:

```sh
fsck.carryfs --repair --accept-data-loss --report accepted-loss.txt volume.cfs
```

This option cannot reconstruct missing bytes or bypass an unsafe write plan.
Keep the report with the repaired volume. A structurally valid result can still
contain partial files or lack objects whose inodes were unreadable.

After repair, verify the data and review any remaining errors:

```sh
carryfs check volume.cfs
carryfs scrub volume.cfs
```

If repair is interrupted or reports an I/O failure, close the tool and run a fresh
diagnosis before another repair. Use a new report filename for the next attempt.

## Work with an offline raw device

Supply the device's exact length in bytes with `--bytes`. On Linux, for example:

```sh
fsck.carryfs --plan --bytes "$(blockdev --getsize64 /dev/sdb1)" /dev/sdb1
fsck.carryfs --repair --report /safe-disk/repair.txt --bytes "$(blockdev --getsize64 /dev/sdb1)" /dev/sdb1
```

In-place raw-device repair requires an explicit report path on another filesystem.
With `--repair --output NEW_IMAGE`, the default report is beside the new image. Use the
partition that contains CarryFS, rather than the whole disk. Device access must
exclude mounts and other writers. The CLI uses exclusive device access on Linux,
macOS, FreeBSD, and Windows, and refuses writable raw access on other platforms.

## Recovery limits

At least one supported, unambiguous root record must survive. Recovery follows
references from the newest committed root and verifies page checksums, identities,
generations, record schemas, and key ranges. It does not scan historical pages or
silently roll back to an older root. If a catalog root or internal page is
unreadable, its unreachable descendants cannot be recovered by this tool.

Repair builds replacement metadata in the verified workspace and publishes it
with CarryFS's durable metadata and root-mirror sequence. The old metadata and
file bytes remain untouched until publication. Repair also checks the complete
proposed forest before its first write and reads it back after publication.

In-place repair requires enough verified workspace slots to hold the replacement
trees and leave 4,096 slots for subsequent writes. A damaged workspace, conflicting
ownership, insufficient space, or an invalid proposed forest blocks repair.
Read-only export can use a validated in-memory reconstruction even when the real
workspace cannot safely fund repair. Export metadata can exceed the physical
workspace's capacity. Metadata and ownership bitmaps are held in
memory, so recovery memory use grows with the amount of allocated metadata and data.

File data has no redundant copy for repair to use. Checksums detect damaged bytes;
restore those bytes from an independent backup. A successful structural repair
alone does not establish that the filesystem is ready for unrestricted use.

Exit status is 0 for a clean diagnosis or complete export, 1 for a committed
repair or complete recovery into a new image, 4 for damage, blocked repair, or
incomplete recovery, 8 for an operational
failure, and 16 for invalid arguments. Status 1 does not mean every file was
recovered when `--accept-data-loss` was supplied.
