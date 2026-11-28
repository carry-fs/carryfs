# Versioning

CarryFS has one version. Every crate, every platform package, the Linux
kernel module source, and the UI manager share it, and one `vX.Y.Z` tag
releases all of them. The crates are not published and nobody installs the
core without its adapters, so separate crate versions would describe
combinations that never exist.

## Where the version lives

The root `Cargo.toml` holds it in `[workspace.package] version`, and every
crate inherits it with `version.workspace = true`. The macOS bundle repeats it
in `CFBundleShortVersionString` in
`fuse/packaging/carryfs.fs/Contents/Info.plist`, and the manager, which is its
own Cargo workspace, repeats it in `manager/Cargo.toml`.
`packaging/stage-release.sh` refuses to stage a package when they differ, and
the release workflow refuses a tag that does not match `Cargo.toml`.

`carryfs`, `carryfs-fuse` and `carryfs-windows` print the version and the
format revisions they write and read:

```
$ carryfs --version
carryfs 0.2.0 (format revision 2; reads revisions 1-2)
```

Ask for that line first in a bug report.

## What to bump

Bump for what a user of the previous release would notice.

- Patch (0.2.x) is for fixes. Volumes, commands, and output stay as they were.
- Minor (0.x.0) is for anything that can break someone. That includes a
  volume the previous release cannot open, a removed or renamed command or
  flag, and changed output a script might parse. Before 1.0 the minor version
  marks compatibility.
- Major plays that role from 1.0 on. Release 1.0 once the format revision is
  frozen.

If you are unsure, bump the minor. An extra minor release costs nothing. A
patch release that leaves someone's drive unreadable on their other computer
costs a lot.

## The disk format

The disk is what two releases share. People format a drive on one computer
and plug it into another that runs an older or newer CarryFS. The format
revision in `ondisk/src/format.rs` tracks this, separately from the product
version. `REVISION` is what this build writes, and `LEGACY_REVISION` is the
oldest one it still reads.

- Change the on-disk encoding only together with a new revision, and describe
  the change in `FORMAT.md`.
- A new revision needs a new minor release before 1.0, or a major one after.
- Keep reading the previous revision. Writing to it may require `upgrade`, as
  revision 1 does.
- Regenerate the encoding vectors with `python3 spec/verify.py --write`, and
  keep a fixture for every revision the code still reads.

The kernel module and the userspace tools share the `ondisk` crate. A module
left over from an older install therefore checks the same revision numbers.

## Installers

`install.sh` and `install.ps1` keep an installed CarryFS on its release line.
That line is the major version, or `0.MINOR` before 1.0. Moving to a new line
takes `--upgrade`, or `-Upgrade` on Windows. Without that rule a computer
could update itself into a release whose volumes the user's other computers
cannot read.

## The UI manager

The UI ships in the same release with the same version. It needs no protocol
version as long as it only runs its companion `carryfs`. If it ever talks to a
separately installed process, such as the Windows service, the two can be on
different versions after one of them updates. Give that connection a protocol
number then, and check it when connecting.

If the UI starts shipping on its own schedule, give it a tag prefix of its own
in this repository, such as `ui-v1.4.0`, and its own release index.

## Cutting a release

1. Set the version in `Cargo.toml`, `manager/Cargo.toml`, and the bundle's
   `Info.plist`, then run `cargo build` in the root and in `manager/` so both
   `Cargo.lock` files follow.
2. Update `RELEASE_NOTES.md`. GitHub uses it as the release text. Its
   compatibility section names the format revision, which earlier releases
   can open new volumes, the upgrade steps, and command changes.
3. Push a `vX.Y.Z` tag, or `vX.Y.Z-rc.N` for a prerelease.
   `.github/workflows/release.yml` builds and publishes the packages, then
   republishes `https://carryfs.com/releases.txt`.

The publisher builds `releases.txt` from the published GitHub releases that
have all three platform packages and `SHA256SUMS`. Never add a version to it
by hand. A tagged run updates only the index and leaves the website's
installer scripts alone. If the index update fails after the release
succeeded, rerun the failed publisher job.
