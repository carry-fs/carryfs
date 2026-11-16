#!/bin/sh
set -eu

repository=carry-fs/carryfs
release_index=https://carryfs.com/releases.txt
# The last release with no package for this system, if any; set per platform.
unpackaged_through=
work=

die() { printf 'carryfs: %s\n' "$*" >&2; exit 1; }

valid_version() {
    printf '%s\n' "$1" | LC_ALL=C awk '
        NR == 1 && /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$/ {
            ok=1
            i = index($0, "-")
            n = i ? split(substr($0, i+1), id, ".") : 0
            for (k = 1; k <= n; k++) if (id[k] ~ /^0[0-9]+$/) ok=0
        }
        END { exit !(ok && NR == 1) }'
}

newer() {
    awk -v a="$1" -v b="$2" 'BEGIN {
        split(a,x,"."); split(b,y,".")
        for(i=1;i<=3;i++) if(x[i]+0 != y[i]+0) exit !(x[i]+0 > y[i]+0)
        exit 1
    }'
}

newer_version() {
    LC_ALL=C awk -v a="$1" -v b="$2" '
        function cmp(x, y,    cx, cy, i, px, py, nx, ny, hx, hy, n, dx, dy) {
            hx = index(x, "-"); hy = index(y, "-")
            split(hx ? substr(x, 1, hx-1) : x, cx, ".")
            split(hy ? substr(y, 1, hy-1) : y, cy, ".")
            for (i = 1; i <= 3; i++) if (cx[i]+0 != cy[i]+0) return cx[i]+0 > cy[i]+0 ? 1 : -1
            if (!hx && !hy) return 0
            if (!hx) return 1
            if (!hy) return -1
            nx = split(substr(x, hx+1), px, ".")
            ny = split(substr(y, hy+1), py, ".")
            n = nx < ny ? nx : ny
            for (i = 1; i <= n; i++) {
                dx = px[i] ~ /^[0-9]+$/; dy = py[i] ~ /^[0-9]+$/
                if (dx && dy) { if (px[i]+0 != py[i]+0) return px[i]+0 > py[i]+0 ? 1 : -1 }
                else if (dx) return -1
                else if (dy) return 1
                else if (px[i] != py[i]) return (px[i] "") > (py[i] "") ? 1 : -1
            }
            return nx == ny ? 0 : (nx > ny ? 1 : -1)
        }
        BEGIN { exit !(cmp(a, b) > 0) }'
}

download() {
    curl --fail --silent --show-error --location --proto '=https' \
        --proto-redir '=https' --connect-timeout 15 --max-time 300 \
        --retry 2 \
        -H 'User-Agent: carryfs-installer' --output "$2" "$1" ||
        die "Download failed: $1. Check the connection and release availability."
}

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
    tools='curl tar awk mktemp'
    [ "$local_build" = 0 ] || tools='cargo awk mktemp'
    for tool in $tools; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            printf 'Missing %s. curl: https://curl.se/download.html; cargo: https://rustup.rs/\n' "$tool" >&2
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
            if [ "$native" = 1 ]; then
                for tool in bash make modinfo depmod; do
                    command -v "$tool" >/dev/null 2>&1 || die "Missing $tool for native installation."
                done
            fi
            ;;
        Darwin)
            [ "$native" = 0 ] || die '--native is supported on Linux only.'
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
        FreeBSD)
            [ "$native" = 0 ] || die '--native is supported on Linux only.'
            release=$(uname -r)
            major=${release%%.*}
            case $major in 14|15) ;; *) die 'FreeBSD packages support releases 14.5 and 15.1 or later within those major versions.' ;; esac
            minor=${release#*.}
            minor=${minor%%-*}
            case $minor in ''|*[!0-9]*) die 'Cannot identify the FreeBSD release.' ;; esac
            if { [ "$major" = 14 ] && [ "$minor" -lt 5 ]; } || { [ "$major" = 15 ] && [ "$minor" -lt 1 ]; }; then
                die 'FreeBSD packages require 14.5 or 15.1 or later within those major versions.'
            fi
            case $(uname -m) in
                amd64) architecture=x86_64 ;;
                arm64|aarch64) architecture=arm64 ;;
                *) die 'FreeBSD packages support amd64 and arm64 only.' ;;
            esac
            platform=freebsd$major-$architecture
            # FreeBSD packages start after 0.1.0; release.yml cannot publish without them.
            unpackaged_through=0.1.0
            marker=/usr/local/libexec/carryfs/VERSION
            adapter=/usr/local/libexec/carryfs/carryfs-fuse
            for tool in sha256 pgrep mount_fusefs; do
                command -v "$tool" >/dev/null 2>&1 || die "Missing $tool."
            done
            [ -c /dev/fuse ] || die 'Load the FreeBSD FUSE module first: kldload fusefs'
            ;;
        *) die 'Use install.sh on Linux, macOS, or FreeBSD. On Windows, use install.ps1 from PowerShell.' ;;
    esac
    [ "$missing" = 0 ] || die 'Install the missing prerequisites and run the installer again.'
    if [ "$(id -u)" != 0 ]; then
        command -v sudo >/dev/null 2>&1 || die 'System installation needs sudo or a root shell.'
    fi
}

select_release() {
    if [ -n "$requested" ]; then
        if [ -n "$unpackaged_through" ] && ! newer_version "$requested" "$unpackaged_through"; then
            die "CarryFS $requested has no package for this system. Choose a later release, or build this checkout with ./install.sh --local."
        fi
        tag=v$requested
    else
        download "$release_index" "$work/versions"
        major=
        if [ -n "$installed" ] && [ "$upgrade" = 0 ]; then major=${installed%%.*}; fi
        tag=$(awk -v major="$major" -v unpackaged="$unpackaged_through" '
            BEGIN { split(unpackaged, u, ".") }
            { sub(/\r$/, "") }
            $0 == "" { next }
            $0 !~ /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ { exit 1 }
            { split($0,n,".") }
            major != "" && n[1] != major { next }
            unpackaged != "" && (n[1]+0<u[1]+0 || (n[1]+0==u[1]+0 && (n[2]+0<u[2]+0 || (n[2]+0==u[2]+0 && n[3]+0<=u[3]+0)))) { next }
            !best || n[1]+0>a || (n[1]+0==a && n[2]+0>b) || (n[1]+0==a && n[2]+0==b && n[3]+0>c) {
                best=$0; a=n[1]+0; b=n[2]+0; c=n[3]+0
            }
            END { if(best) print "v" best }' "$work/versions") || die 'Invalid release index from carryfs.com.'
        if [ -z "$tag" ] && [ -n "$unpackaged_through" ]; then
            die 'No published release has a package for this system yet. Build this checkout with ./install.sh --local.'
        fi
        [ -n "$tag" ] || die 'No published stable release matches this installation. Use --version for an explicit published tag.'
    fi
    selected=${tag#v}
    if [ -n "$installed" ]; then
        if [ "$selected" = "$installed" ] || { [ -z "$requested" ] && ! newer_version "$selected" "$installed"; }; then
            printf 'CarryFS %s is already current for this selection.\n' "$installed"
            exit 0
        fi
    fi
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
    elif [ "${platform#freebsd}" != "$platform" ]; then
        actual=$(sha256 -q "$work/$asset")
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
        freebsd14-*|freebsd15-*)
            [ -f "$work/payload/carryfs-fuse" ] || die 'The release package is missing carryfs-fuse.'
            ;;
        macos-arm64)
            [ -f "$work/payload/carryfs.fs/Contents/Info.plist" ] || die 'The release package is missing the filesystem bundle.'
            for file in mount_carryfs carryfs.util newfs_carryfs fsck_carryfs; do
                [ -f "$work/payload/carryfs.fs/Contents/Resources/$file" ] || die "The bundle is missing $file."
            done ;;
    esac
}

build_local() {
    [ -f "$checkout/Cargo.toml" ] && [ -f "$checkout/packaging/install-unix.sh" ] ||
        die '--local needs a CarryFS checkout: run ./install.sh --local in the cloned repository.'
    selected=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$checkout/Cargo.toml" | head -n 1)
    valid_version "$selected" || die 'Cannot read the version from Cargo.toml.'
    mkdir "$work/payload"
    (cd "$checkout" &&
        target=${CARGO_TARGET_DIR:-target}/release &&
        if [ "$platform" = linux-x86_64 ]; then
            cargo build --release --locked -p carryfs-core -p carryfs-fuse &&
            cp "$target/carryfs" "$target/carryfs-fuse" fuse/packaging/61-carryfs.rules \
                fuse/packaging/mount_options.conf "$work/payload/" &&
            if [ "$native" = 1 ]; then
                kernel_release=$(uname -r) &&
                bash scripts/kernel-build.sh "/lib/modules/$kernel_release/build" &&
                cp linux/carryfs.ko "$work/payload/" &&
                printf '%s\n' "$kernel_release" > "$work/payload/KERNEL_RELEASE"
            fi
        elif [ "${platform#freebsd}" != "$platform" ]; then
            cargo build --release --locked -p carryfs-core -p carryfs-fuse &&
            cp "$target/carryfs" "$target/carryfs-fuse" "$work/payload/"
        else
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

uninstall_linux() {
    sudo=
    [ "$(id -u)" = 0 ] || sudo=sudo
    $sudo sh -s <<'UNINSTALL'
set -eu
die() { printf 'carryfs: %s\n' "$*" >&2; exit 1; }
validate_kernel() {
    case $1 in ''|.*|*[!a-zA-Z0-9._+~-]*) die 'Invalid kernel release in the native module record.' ;; esac
}
[ ! -d /sys/module/carryfs ] || die 'Unload the native module with modprobe -r carryfs before uninstalling.'
mkdir /var/run/carryfs-install.lock || die 'Another installation is running, or the install lock needs cleanup.'
backup=
changed=0
committed=0
kernels=
paths='/usr/local/bin/carryfs /usr/libexec/carryfs /usr/sbin/mount.carryfs /usr/sbin/mkfs.carryfs /etc/udev/rules.d/61-carryfs.rules /etc/udisks2/mount_options.conf'
finish() {
    status=$?
    trap - EXIT
    if [ "$changed" = 1 ] && [ "$committed" = 0 ]; then
        printf 'Uninstall failed; restoring the previous installation.\n' >&2
        index=0
        restored=1
        for path in $paths; do
            index=$((index+1))
            rm -rf "$path" || restored=0
            if [ -e "$backup/$index" ] || [ -L "$backup/$index" ]; then
                mkdir -p "$(dirname "$path")" || restored=0
                cp -pR "$backup/$index" "$path" || restored=0
            fi
        done
        for kernel in $kernels; do
            if [ -d "/lib/modules/$kernel" ]; then depmod -a "$kernel" || restored=0; fi
        done
        udevadm control --reload || restored=0
        if [ "$restored" = 0 ]; then
            printf 'Restoration failed. Keep the backup at %s and restore it before mounting CarryFS.\n' "$backup" >&2
            backup=
        fi
    fi
    [ -z "$backup" ] || rm -rf "$backup"
    rmdir /var/run/carryfs-install.lock
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
for process in carryfs-fuse mount_carryfs; do
    status=0
    pgrep -x "$process" >/dev/null || status=$?
    case $status in
        1) ;;
        0) die 'A CarryFS daemon is running. Eject all CarryFS volumes and retry.' ;;
        *) die 'Cannot check for running CarryFS daemons.' ;;
    esac
done
[ ! -L /usr/libexec/carryfs/native-kernels ] || die 'The native module record must not be a symlink.'
if [ -f /usr/libexec/carryfs/native-kernels ]; then
    command -v depmod >/dev/null 2>&1 || die 'Missing depmod for native uninstallation.'
    while IFS= read -r kernel || [ -n "$kernel" ]; do
        validate_kernel "$kernel"
        kernels="$kernels $kernel"
        paths="$paths /lib/modules/$kernel/updates/carryfs.ko"
    done < /usr/libexec/carryfs/native-kernels
fi
backup=$(mktemp -d /var/tmp/carryfs-install.XXXXXXXX)
index=0
for path in $paths; do
    index=$((index+1))
    if [ -e "$path" ] || [ -L "$path" ]; then cp -pR "$path" "$backup/$index"; fi
done
options=/etc/udisks2/mount_options.conf
if [ -f "$options" ]; then
    awk '
        /^\[.*\]/ { in_defaults = ($0 ~ /^\[defaults\][[:space:]]*$/) }
        in_defaults && /^carryfs_(defaults|allow)=/ { next }
        { print }' "$options" > "$backup/mount_options.conf"
fi
changed=1
rm -f /usr/local/bin/carryfs /etc/udev/rules.d/61-carryfs.rules
rm -rf /usr/libexec/carryfs
for link in /usr/sbin/mount.carryfs /usr/sbin/mkfs.carryfs; do
    if [ -L "$link" ] && [ "$(readlink "$link")" = /usr/libexec/carryfs/carryfs-fuse ]; then
        rm -f "$link"
    fi
done
if [ -f "$options" ]; then
    if awk '!/^[[:space:]]*(#.*)?$/ && !/^\[defaults\][[:space:]]*$/ { set=1 } END { exit !set }' "$backup/mount_options.conf"; then
        install -m 644 "$backup/mount_options.conf" "$options"
    else
        rm -f "$options"
    fi
fi
for kernel in $kernels; do
    rm -f "/lib/modules/$kernel/updates/carryfs.ko"
    if [ -d "/lib/modules/$kernel" ]; then depmod -a "$kernel"; fi
    if [ -d "/lib/modules/$kernel/updates" ]; then
        rmdir --ignore-fail-on-non-empty "/lib/modules/$kernel/updates"
    fi
done
udevadm control --reload
committed=1
UNINSTALL
}

main() {
    requested='' upgrade=0 uninstall=0 local_build=0 native=0 installed=''
    while [ "$#" -gt 0 ]; do
        case $1 in
            --version)
                [ "$#" -ge 2 ] || die '--version needs a version.'
                requested=${2#v}
                valid_version "$requested" || die 'Version must be X.Y.Z or vX.Y.Z, optionally with a prerelease such as -rc.1.'
                shift 2 ;;
            --upgrade) upgrade=1; shift ;;
            --uninstall) uninstall=1; shift ;;
            --local) local_build=1; shift ;;
            --native) native=1; shift ;;
            --help|-h)
                printf 'Usage: install.sh [--version X.Y.Z[-rc.N] | --upgrade | --local [--native] | --uninstall]\nDefault: latest stable release within the installed major; latest stable on first install.\n--version installs that exact release, prereleases included.\n--local builds the checkout holding this script and installs or reinstalls it.\n--native adds the experimental Linux module for the running kernel; requires --local.\n--uninstall also removes all native modules recorded by this installer.\n'
                return ;;
            *) die "Unknown argument: $1" ;;
        esac
    done
    [ "$local_build" = 0 ] || { [ -z "$requested" ] && [ "$upgrade" = 0 ] && [ "$uninstall" = 0 ]; } ||
        die '--local cannot be combined with --version, --upgrade, or --uninstall.'
    [ "$native" = 0 ] || [ "$local_build" = 1 ] || die '--native requires --local.'
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
                question="Remove CarryFS from /usr/local/bin/carryfs and $adapter, with its helper links, udev rule, UDisks options, and recorded native modules?"
                kept='Disk contents' ;;
            FreeBSD)
                adapter=/usr/local/libexec/carryfs
                question="Remove CarryFS and its mount and format helpers from /usr/local?"
                kept='Disk contents and the fusefs module' ;;
            *) die '--uninstall is supported on Linux, macOS, and FreeBSD only.' ;;
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
        elif [ "$system" = FreeBSD ]; then
            sudo=
            [ "$(id -u)" = 0 ] || sudo=sudo
            $sudo rm -f /usr/local/bin/carryfs /usr/local/sbin/mount_carryfs /usr/local/sbin/newfs_carryfs
            $sudo rm -rf /usr/local/libexec/carryfs
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
    [ -z "$requested" ] || valid_version "$requested" || die 'Version must be X.Y.Z or vX.Y.Z, optionally with a prerelease such as -rc.1.'
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
    if [ -n "$installed" ] && newer_version "$installed" "$selected"; then printf 'This will downgrade CarryFS.\n'; fi
    printf 'Destination: /usr/local/bin/carryfs and %s. Eject all CarryFS volumes before continuing.\n' "${marker%/VERSION}"
    if [ "$native" = 1 ]; then
        printf 'Native module: /lib/modules/%s/updates/carryfs.ko. FUSE remains the default; the module will not be loaded.\n' "$(cat "$work/payload/KERNEL_RELEASE")"
    fi
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

main "$@"
