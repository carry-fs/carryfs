# Recover and repair a CarryFS volume

Unmount or eject the volume before you diagnose, export, or repair it. If you
may want evidence later, or another try with a different method, keep an image
of the damaged device first. In-place repair changes the original filesystem.
Recovery into a new image and export to a folder leave the source read-only.

Unix installations include `fsck.carryfs`. From a checkout, build the core with
`cargo build --locked -p carryfs-core` and run `target/debug/fsck-carryfs`.
`carryfs fsck` takes the same options. On Windows, `carryfs.exe fsck`
diagnoses and repairs images and recovers them into new images. Export to a
folder needs Linux, macOS, FreeBSD, or NetBSD.

## Diagnose without writing

Run the checker with no options, or with `--plan`, which does the same:

```sh
fsck.carryfs --plan volume.cfs
```

It prints what it found and the changes a repair would make. It writes
nothing, not even a report unless you pass `--report NEW_FILE`. The plan covers
the structure only. If the metadata is readable, run `carryfs scrub volume.cfs`
to verify the file data too.

## Export accessible files

To copy files out before you change the source, pick a new folder on another
filesystem:

```sh
fsck.carryfs --recover recovered --report recovery.txt volume.cfs
```

Read `recovery.txt` even when the files look intact. It lists missing
metadata, incomplete mappings, and data ranges that failed their checksum.
Export leaves unreadable ranges unwritten in partial files. Treat them as
missing data, not as zeros.

Export skips damaged catalog branches and keeps going through the valid ones.
Files that lost their directory entries go to `lost+found`, named by their
object IDs. If the source already has a `lost+found`, export uses
`lost+found.1` or the next free suffix. Export breaks directory cycles and
reconnects the cut-off directory with its surviving children.

The destination and the report must not exist yet. Export refuses existing
paths, including symlinks and other names for the source. Without `--report`,
it writes the report beside the destination, named after it with
`.recovery.txt` appended.

## Recover into a new image

To rebuild the readable files into a new image without changing the damaged
source, give a new image filename:

```sh
fsck.carryfs --repair --output recovered.cfs volume.cfs
```

The tool opens the source read-only and creates a sparse CarryFS image of the
same length. It copies checksum-verified data through the normal filesystem
writer, which builds new allocation and metadata without using the source's
workspace. It refuses existing output paths, symlinks, and other names for the
source. This works on a clean source too.

Every new image has `RECOVERY-REPORT.json` at its root. If a recovered entry
already uses that name or its working filename, the tool picks
`RECOVERY-REPORT.1.json` or the next free suffix, and the recovered file keeps
its name and contents. The tool also saves the same JSON beside the image as
`recovered.cfs.recovery.json`. Both reports must be new files.

The manifest records the source UUID and generation, destination UUID, recovery
plan, per-file paths and object IDs, copied bytes, missing source ranges, and
output verification failures. A file is `partial` when a known range is missing,
`unknown` when metadata completeness cannot be established, and `damaged` when
destination verification finds bad output data. A file can have more than
one of these. A clean scrub doesn't clear a `partial` or `unknown` status.

Before copying starts, the tool makes the embedded report durable with state
`in_progress`. It replaces the report through an atomic rename as it
finishes. An interrupted update keeps the earlier report and can leave a
`.working` file. A finished report has state `complete`, `partial`, or
`verification_failed`. The totals of recovered objects and files leave out the
report files.

The new image keeps the volume label, recovered paths, hard links, symlink
targets, timestamps, permission bits, hidden attributes, and holes in sparse
files. It gets a new UUID and new object IDs. Reserved space that was never
written is not copied.

The text report goes to `recovered.cfs.recovery.txt` unless you pass
`--report NEW_FILE`. The tool syncs it before it creates the image. It lists
missing metadata and unreadable file ranges by recovered path. Read it even
when the new image passes scrub. Missing data stays unwritten and reads as
zeros in partial files, and scrub can't tell.

To read back and verify destination data after copying and flushing, add
`--verify-data`:

```sh
fsck.carryfs --repair --output recovered.cfs --verify-data volume.cfs
```

This reads back the written ranges of recovered regular files and the final
manifest. Gaps from the source still count as missing, not as verified zeros.
The manifest records verification as `not_requested`, `pending`, `passed`, or
`failed`. If destination data fails, the result is `verification_failed`, both
reports stay, and the tool exits with status 8. Source damage still gives a
partial result with status 4, even when every surviving byte verifies.

Recovery into a new image never needs `--accept-data-loss`, because it leaves
the source alone. Before it reports completion, the tool reads the finished
image back and checks its structure. Complete recovery exits with status 1.
Partial recovery keeps the new image and report and exits with status 4. A
write or flush failure exits with status 8 and can leave a partial image. If
the report has no completion entry for the image, the attempt didn't finish.
Retry with new output and report filenames.

## Repair the original volume

After reviewing the plan and exporting what you need, run:

```sh
fsck.carryfs --repair --report repair.txt volume.cfs
```

Repair reconnects recoverable objects and rebuilds the catalog, the allocation
map, and the cleanup tree. Where their metadata survives, it keeps file bytes,
object IDs, the volume UUID, timestamps, modes, symlink targets, and reserved
extents. It frees obsolete metadata and space waiting for cleanup, and
finishes pending deletions that have matching orphan markers. It syncs the
report before it starts. Without `--report`, image repair writes the report
to the source name with `.repair.txt` appended.

If the plan lists unreadable catalog pages or incomplete file mappings, repair
refuses to discard them. After you save a copy of the source and review the
listed losses, you can allow that loss:

```sh
fsck.carryfs --repair --accept-data-loss --report accepted-loss.txt volume.cfs
```

This option can't bring back missing bytes or override an unsafe write plan.
Keep the report with the repaired volume. A structurally valid result can still
hold partial files or miss objects whose inodes were unreadable.

After repair, verify the data and review any remaining errors:

```sh
carryfs check volume.cfs
carryfs scrub volume.cfs
```

If repair stops partway or reports an I/O failure, close the tool and diagnose
the volume again before you retry. Use a new report filename for the retry.

## Work with an offline raw device

Pass the device's exact length in bytes with `--bytes`. On Linux, for example:

```sh
fsck.carryfs --plan --bytes "$(blockdev --getsize64 /dev/sdb1)" /dev/sdb1
fsck.carryfs --repair --report /safe-disk/repair.txt --bytes "$(blockdev --getsize64 /dev/sdb1)" /dev/sdb1
```

Repairing a raw device in place needs `--report` with a path on another
filesystem. With `--repair --output NEW_IMAGE`, the report goes beside the new
image by default. Point the tool at the partition that holds CarryFS, not the
whole disk. Nothing else may mount or write the device meanwhile. The CLI opens
devices exclusively on Linux, macOS, FreeBSD, NetBSD, and Windows, and refuses
writable raw access elsewhere.

## Recovery limits

At least one supported, unambiguous root record must survive. Recovery follows
references from the newest committed root and verifies page checksums,
identities, generations, record schemas, and key ranges. It doesn't scan old
pages or quietly fall back to an older root. If a catalog root or internal page
is unreadable, this tool can't recover what lies only beneath it.

Repair builds new metadata in the verified workspace and publishes it with
CarryFS's normal durable commit, which writes both root records. Until then the
old metadata and file bytes stay untouched. Repair checks the whole proposed
set of trees before its first write and reads it back after publishing.

In-place repair needs enough verified workspace slots for the new trees, plus
4,096 left over for later writes. A damaged workspace, conflicting ownership,
too little space, or an invalid proposed tree blocks repair. Export can still
work in that case, because it rebuilds the metadata in memory and doesn't need
the workspace. Recovery keeps metadata and ownership bitmaps in memory, so its
memory use grows with the amount of allocated metadata and data.

File data has no second copy for repair to use. Checksums find damaged bytes,
and only a backup can restore them. A successful structural repair alone
doesn't prove the volume is fit for normal use.

## Exit status

`fsck.carryfs` exits with:

- 0: clean diagnosis or complete export.
- 1: committed repair, or complete recovery into a new image.
- 4: damage, blocked repair, or incomplete recovery.
- 8: operational failure.
- 16: invalid arguments.

With `--accept-data-loss`, status 1 does not mean every file was recovered.
