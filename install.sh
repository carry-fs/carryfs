#!/bin/sh
# Install a published CarryFS package, or with --local the checkout holding this
# script. All system writes happen after confirmation.
set -eu

repository=carry-fs/carryfs
api=https://api.github.com/repos/$repository
work=

die() { printf 'carryfs: %s\n' "$*" >&2; exit 1; }

valid_version() {
    printf '%s\n' "$1" | LC_ALL=C awk '
        NR == 1 && /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ { ok=1 }
        END { exit !(ok && NR == 1) }'
}

newer() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        split(a,x,"."); split(b,y,".")
        for(i=1;i<=3;i++) if(x[i]+0 != y[i]+0) exit !(x[i]+0 > y[i]+0)
        exit 1
    }'
}

download() {
    curl --fail --silent --show-error --location --proto '=https' \
        --proto-redir '=https' --connect-timeout 15 --max-time 300 \
        --retry 2 -H 'Accept: application/vnd.github+json' \
        -H 'User-Agent: carryfs-installer' --output "$2" "$1" ||
        die "Download failed: $1. Check the connection, release availability, and GitHub API rate limit."
}

# Arch and its derivatives have no release numbers to compare. Ubuntu needs
# 24.04, the runtime baseline of the release binaries.
check_distribution() {
    [ -r "$1" ] || die 'Cannot identify the Linux distribution.'
    distribution=$(sed -n 's/^ID=//p' "$1" | tr -d '"')
    like=$(sed -n 's/^ID_LIKE=//p' "$1" | tr -d '"')
    case " $distribution $like " in *' arch '*) return ;; esac
    [ "$distribution" = ubuntu ] || die 'Linux packages currently support Ubuntu and Arch Linux with systemd only.'
    os_version=$(sed -n 's/^VERSION_ID=//p' "$1" | tr -d '"')
    awk -v version="$os_version" 'BEGIN { split(version,n,"."); exit !(n[1]+0>24 || (n[1]+0==24 && n[2]+0>=4)) }' ||
        die 'Linux packages require Ubuntu 24.04 or later.'
}

prerequisites() {
    missing=0
    tools='curl jq tar awk mktemp'
    [ "$local_build" = 0 ] || tools='cargo awk mktemp'
    for tool in $tools; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            printf 'Missing %s. curl: https://curl.se/download.html; jq: https://jqlang.org/download/; cargo: https://rustup.rs/\n' "$tool" >&2
            missing=1
        fi
    done
    case $(uname -s) in
        Linux)
            platform=linux-x86_64
            marker=/usr/libexec/carryfs/VERSION
            adapter=/usr/libexec/carryfs/carryfs-fuse
            [ "$(uname -m)" = x86_64 ] || die 'Linux packages currently support x86-64 only.'
            check_distribution /etc/os-release
            if [ ! -c /dev/fuse ]; then
                printf 'Missing /dev/fuse. Enable the FUSE kernel module; see https://github.com/libfuse/libfuse\n' >&2
                missing=1
            fi
            for tool in fusermount3 systemctl systemd-run udevadm udisksctl sha256sum pgrep; do
                if ! command -v "$tool" >/dev/null 2>&1; then
                    printf 'Missing %s. Ubuntu: sudo apt install fuse3 udisks2 systemd udev procps coreutils; Arch: sudo pacman -S fuse3 udisks2 procps-ng coreutils\n' "$tool" >&2
                    missing=1
                fi
            done
            [ -d /run/systemd/system ] || die 'CarryFS needs a running systemd host; containers and WSL without systemd are unsupported.'
            ;;
        Darwin)
            platform=macos-arm64
            marker=/Library/Filesystems/carryfs.fs/Contents/VERSION
            adapter=/Library/Filesystems/carryfs.fs/Contents/Resources/mount_carryfs
            [ "$(sysctl -n hw.optional.arm64)" = 1 ] || die 'macOS packages currently support Apple Silicon only.'
            os_version=$(sw_vers -productVersion)
            newer 15.6.0 "$os_version" && die 'CarryFS requires macOS 15.6 or later.'
            macfuse=/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse
            if [ ! -x "$macfuse" ]; then
                printf 'Install macFUSE 5.4 or later: https://macfuse.github.io/\n' >&2
                missing=1
            else
                fuse_version=$(pkgutil --pkg-info io.macfuse.installer.components.core | sed -n 's/^version: //p') || die 'Cannot read the macFUSE package receipt.'
                [ -n "$fuse_version" ] || die 'Cannot read the macFUSE version; reinstall from https://macfuse.github.io/.'
                newer 5.4.0 "$fuse_version" && die 'Update macFUSE to 5.4 or later: https://macfuse.github.io/.'
                loaded=$(kmutil showloaded --list-only 2>&1) || die "Cannot check the macFUSE kernel extension: $loaded"
                printf '%s\n' "$loaded" | awk '/io\.macfuse\.filesystems\.macfuse/ { found=1 } END { exit !found }' ||
                    die 'The macFUSE kernel extension is not loaded. Complete its setup and restart if requested: https://github.com/macfuse/macfuse/wiki/Getting-Started'
            fi
            for tool in shasum pgrep; do
                command -v "$tool" >/dev/null 2>&1 || die "Missing $tool."
            done
            ;;
        *) die 'Use install.sh on Linux or macOS. On Windows, use install.ps1 from PowerShell.' ;;
    esac
    [ "$missing" = 0 ] || die 'Install the missing prerequisites and run the installer again.'
    if [ "$(id -u)" != 0 ]; then
        command -v sudo >/dev/null 2>&1 || die 'System installation needs sudo or a root shell.'
    fi
}

select_release() {
    if [ -n "$requested" ]; then
        tag=v$requested
    else
        : > "$work/tags"
        page=1
        while :; do
            download "$api/releases?per_page=100&page=$page" "$work/releases.json"
            jq -e 'type == "array"' "$work/releases.json" >/dev/null || die 'Invalid release list from GitHub.'
            jq -r '.[] | select(.draft == false and .prerelease == false) | .tag_name |
                select(test("^v(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$"))' \
                "$work/releases.json" >> "$work/tags"
            [ "$(jq length "$work/releases.json")" -eq 100 ] || break
            page=$((page + 1))
        done
        major=
        if [ -n "$installed" ] && [ "$upgrade" = 0 ]; then major=${installed%%.*}; fi
        tag=$(awk -v major="$major" '
            { v=substr($0,2); split(v,n,".") }
            major != "" && n[1] != major { next }
            !best || n[1]+0>a || (n[1]+0==a && n[2]+0>b) || (n[1]+0==a && n[2]+0==b && n[3]+0>c) {
                best=$0; a=n[1]+0; b=n[2]+0; c=n[3]+0
            }
            END { print best }' "$work/tags")
        [ -n "$tag" ] || die 'No published stable release matches this installation. Use --version for an explicit published tag.'
    fi
    selected=${tag#v}
    if [ -n "$installed" ]; then
        if [ "$selected" = "$installed" ] || { [ -z "$requested" ] && ! newer "$selected" "$installed"; }; then
            printf 'CarryFS %s is already current for this selection.\n' "$installed"
            exit 0
        fi
    fi
    download "$api/releases/tags/$tag" "$work/release.json"
    jq -e --arg tag "$tag" '.tag_name == $tag and .draft == false' "$work/release.json" >/dev/null || die 'Invalid release metadata from GitHub.'
}

verify_package() {
    asset=carryfs-$selected-$platform.tar.gz
    base=https://github.com/$repository/releases/download/$tag
    download "$base/$asset" "$work/$asset"
    download "$base/SHA256SUMS" "$work/SHA256SUMS"
    expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { hash=tolower($1); count++ }
        END { if(count != 1) exit 1; print hash }' "$work/SHA256SUMS") || die "Missing or duplicate checksum for $asset."
    [ "${#expected}" = 64 ] || die 'Invalid SHA-256 checksum.'
    case $expected in *[!0-9a-f]*) die 'Invalid SHA-256 checksum.' ;; esac
    if [ "$platform" = linux-x86_64 ]; then
        actual=$(sha256sum "$work/$asset")
    else
        actual=$(shasum -a 256 "$work/$asset")
    fi
    [ "${actual%% *}" = "$expected" ] || die "Checksum mismatch for $asset; nothing was installed."
    tar -tzf "$work/$asset" > "$work/entries" || die 'Cannot list the release archive.'
    awk '
        /^\// || /(^|\/)\.\.(\/|$)/ || /[^a-zA-Z0-9_.\/-]/ { exit 1 }
        END { if(NR == 0) exit 1 }' "$work/entries" || die 'Unsafe path in the release archive.'
    tar -tvzf "$work/$asset" > "$work/types" || die 'Cannot inspect the release archive.'
    awk 'substr($0,1,1) != "-" && substr($0,1,1) != "d" { exit 1 }' "$work/types" || die 'The release archive must contain only regular files and directories.'
    mkdir "$work/payload"
    tar -xzf "$work/$asset" -C "$work/payload"
    [ "$(cat "$work/payload/VERSION")" = "$selected" ] || die 'The package version does not match its release tag.'
    [ -f "$work/payload/carryfs" ] && [ -f "$work/payload/install-system.sh" ] || die 'Incomplete release package.'
    case $platform in
        linux-x86_64)
            for file in carryfs-fuse 61-carryfs.rules mount_options.conf; do
                [ -f "$work/payload/$file" ] || die "The release package is missing $file."
            done ;;
        macos-arm64)
            [ -f "$work/payload/carryfs.fs/Contents/Info.plist" ] || die 'The release package is missing the filesystem bundle.'
            for file in mount_carryfs carryfs.util newfs_carryfs fsck_carryfs; do
                [ -f "$work/payload/carryfs.fs/Contents/Resources/$file" ] || die "The bundle is missing $file."
            done ;;
    esac
}

# Build the checkout and stage it in the layout of a release package.
build_local() {
    [ -f "$checkout/Cargo.toml" ] && [ -f "$checkout/packaging/install-unix.sh" ] ||
        die '--local needs a CarryFS checkout: run ./install.sh --local in the cloned repository.'
    selected=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$checkout/Cargo.toml" | head -n 1)
    valid_version "$selected" || die 'Cannot read the version from Cargo.toml.'
    mkdir "$work/payload"
    # Chained with &&: a failed build must not stage binaries left by an older one.
    (cd "$checkout" &&
        target=${CARGO_TARGET_DIR:-target}/release &&
        if [ "$platform" = linux-x86_64 ]; then
            cargo build --release --locked -p carryfs-core -p carryfs-fuse &&
            cp "$target/carryfs" "$target/carryfs-fuse" fuse/packaging/61-carryfs.rules \
                fuse/packaging/mount_options.conf "$work/payload/"
        else
            # The bundle layout of fuse/packaging/macos-bundle.sh.
            resources=$work/payload/carryfs.fs/Contents/Resources &&
            cargo build --release --locked -p carryfs-core -p carryfs-fuse -p carryfs-macos &&
            mkdir -p "$resources" &&
            cp "$target/carryfs" "$work/payload/" &&
            cp fuse/packaging/carryfs.fs/Contents/Info.plist "$work/payload/carryfs.fs/Contents/" &&
            cp "$target/carryfs-util" "$resources/carryfs.util" &&
            cp "$target/newfs_carryfs" "$target/fsck_carryfs" "$resources/" &&
            cp "$target/carryfs-fuse" "$resources/mount_carryfs"
        fi &&
        cp packaging/install-unix.sh "$work/payload/install-system.sh") ||
        die 'Local build failed; nothing was installed.'
    printf '%s\n' "$selected" > "$work/payload/VERSION"
}

# Remove what packaging/install-unix.sh installs on Linux. Only the CarryFS keys
# leave the UDisks options; the file goes when nothing else is set in it.
uninstall_linux() {
    sudo=
    [ "$(id -u)" = 0 ] || sudo=sudo
    options=/etc/udisks2/mount_options.conf
    work=$(mktemp -d "${TMPDIR:-/tmp}/carryfs-install.XXXXXXXX")
    trap 'rm -rf "$work"' EXIT
    if [ -f "$options" ]; then
        awk '
            /^\[.*\]/ { in_defaults = ($0 ~ /^\[defaults\][[:space:]]*$/) }
            in_defaults && /^carryfs_(defaults|allow)=/ { next }
            { print }' "$options" > "$work/mount_options.conf"
    fi
    $sudo rm -f /usr/local/bin/carryfs /etc/udev/rules.d/61-carryfs.rules
    $sudo rm -rf /usr/libexec/carryfs
    if [ -L /usr/sbin/mount.carryfs ] && [ "$(readlink /usr/sbin/mount.carryfs)" = /usr/libexec/carryfs/carryfs-fuse ]; then
        $sudo rm -f /usr/sbin/mount.carryfs
    fi
    if [ -f "$options" ]; then
        if awk '!/^[[:space:]]*(#.*)?$/ && !/^\[defaults\][[:space:]]*$/ { set=1 } END { exit !set }' "$work/mount_options.conf"; then
            $sudo install -m 644 "$work/mount_options.conf" "$options"
        else
            $sudo rm -f "$options"
        fi
    fi
    $sudo udevadm control --reload
}

main() {
    requested= upgrade=0 uninstall=0 local_build=0 installed=
    while [ "$#" -gt 0 ]; do
        case $1 in
            --version)
                [ "$#" -ge 2 ] || die '--version needs a version.'
                requested=${2#v}
                valid_version "$requested" || die 'Version must be X.Y.Z or vX.Y.Z.'
                shift 2 ;;
            --upgrade) upgrade=1; shift ;;
            --uninstall) uninstall=1; shift ;;
            --local) local_build=1; shift ;;
            --help|-h)
                printf 'Usage: install.sh [--version X.Y.Z | --upgrade | --local | --uninstall]\nDefault: latest stable release within the installed major; latest stable on first install.\n--local builds the checkout holding this script and installs or reinstalls it.\n'
                return ;;
            *) die "Unknown argument: $1" ;;
        esac
    done
    [ "$local_build" = 0 ] || { [ -z "$requested" ] && [ "$upgrade" = 0 ] && [ "$uninstall" = 0 ]; } ||
        die '--local cannot be combined with --version, --upgrade, or --uninstall.'
    if [ "$uninstall" = 1 ]; then
        [ -z "$requested" ] && [ "$upgrade" = 0 ] || die '--uninstall cannot be combined with --version or --upgrade.'
        system=$(uname -s)
        case $system in
            Darwin)
                adapter=/Library/Filesystems/carryfs.fs
                question="Remove CarryFS from /usr/local/bin/carryfs and $adapter?"
                kept='macFUSE and disk contents' ;;
            Linux)
                adapter=/usr/libexec/carryfs
                question="Remove CarryFS from /usr/local/bin/carryfs and $adapter, with its mount helper link, udev rule, and UDisks options?"
                kept='Disk contents' ;;
            *) die '--uninstall is supported on Linux and macOS only.' ;;
        esac
        command -v pgrep >/dev/null 2>&1 || die 'Missing pgrep.'
        for process in carryfs-fuse mount_carryfs; do
            status=0
            pgrep -x "$process" >/dev/null || status=$?
            case $status in
                0) die 'A CarryFS daemon is running. Eject all CarryFS volumes and retry.' ;;
                1) ;;
                *) die 'Cannot check for running CarryFS daemons.' ;;
            esac
        done
        [ -e "$adapter" ] || [ -e /usr/local/bin/carryfs ] || {
            printf 'CarryFS is not installed.\n'
            return
        }
        [ -r /dev/tty ] || die 'Confirmation needs a terminal; run the script from an interactive shell.'
        printf '%s Eject all CarryFS volumes first. %s will be left alone. [y/N] ' "$question" "$kept" > /dev/tty
        IFS= read -r answer < /dev/tty || die 'Could not read confirmation.'
        case $answer in y|Y|yes|YES) ;; *) printf 'Uninstall cancelled.\n'; return ;; esac
        if [ "$system" = Linux ]; then
            uninstall_linux
        elif [ "$(id -u)" = 0 ]; then
            rm -f /usr/local/bin/carryfs
            rm -rf /Library/Filesystems/carryfs.fs
            launchctl kickstart -k system/com.apple.diskarbitrationd
        else
            sudo rm -f /usr/local/bin/carryfs
            sudo rm -rf /Library/Filesystems/carryfs.fs
            sudo launchctl kickstart -k system/com.apple.diskarbitrationd
        fi
        printf 'Uninstalled CarryFS. %s were left alone.\n' "$kept"
        return
    fi
    [ -z "$requested" ] || valid_version "$requested" || die 'Version must be X.Y.Z or vX.Y.Z.'
    [ -z "$requested" ] || [ "$upgrade" = 0 ] || die '--version and --upgrade cannot be combined.'
    prerequisites
    if [ -f "$marker" ]; then
        installed=$(cat "$marker")
        valid_version "$installed" || die "Invalid installed version in $marker."
    elif [ -e "$adapter" ] || [ -e /usr/local/bin/carryfs ]; then
        [ -n "$requested" ] || [ "$upgrade" = 1 ] || [ "$local_build" = 1 ] ||
            die 'This installation has no version record. Select --version or --upgrade explicitly.'
    fi
    work=$(mktemp -d "${TMPDIR:-/tmp}/carryfs-install.XXXXXXXX")
    trap 'rm -rf "$work"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM HUP
    if [ "$local_build" = 1 ]; then
        checkout=$(cd "$(dirname "$0")" && pwd)
        build_local
        origin="Built from $checkout."
    else
        select_release
        verify_package
        origin='The package is verified.'
    fi
    printf 'Install CarryFS %s for %s (current: %s).\n' "$selected" "$platform" "${installed:-unrecorded}"
    if [ -n "$installed" ] && newer "$installed" "$selected"; then printf 'This will downgrade CarryFS.\n'; fi
    printf 'Destination: /usr/local/bin/carryfs and %s. Eject all CarryFS volumes before continuing.\n' "${marker%/VERSION}"
    printf '%s System installation requires administrator permission.\n' "$origin"
    [ -r /dev/tty ] || die 'Confirmation needs a terminal; download the script and run it from an interactive shell.'
    printf 'Install? [y/N] ' > /dev/tty
    IFS= read -r answer < /dev/tty || die 'Could not read confirmation.'
    case $answer in y|Y|yes|YES) ;; *) printf 'Installation cancelled.\n'; return ;; esac
    if [ "$(id -u)" = 0 ]; then
        sh "$work/payload/install-system.sh" "$work/payload" "$platform" "$installed"
    else
        sudo sh "$work/payload/install-system.sh" "$work/payload" "$platform" "$installed"
    fi
    printf 'Installed CarryFS %s.\n' "$selected"
}

# Keep invocation last so a truncated download cannot start installation.
main "$@"
