#!/bin/sh
# Included in Unix release archives as install-system.sh; called after confirmation.
set -eu
die() { printf 'carryfs: %s\n' "$*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die 'System installation requires root.'
[ "$#" = 3 ] || die 'Usage: install-system.sh PACKAGE PLATFORM EXPECTED_VERSION'
payload=$1 platform=$2 expected=$3
case $platform in
    linux-x86_64)
        marker=/usr/libexec/carryfs/VERSION
        paths='/usr/local/bin/carryfs /usr/libexec/carryfs /usr/sbin/mount.carryfs /etc/udev/rules.d/61-carryfs.rules /etc/udisks2/mount_options.conf'
        ;;
    macos-arm64)
        marker=/Library/Filesystems/carryfs.fs/Contents/VERSION
        paths='/usr/local/bin/carryfs /Library/Filesystems/carryfs.fs'
        ;;
    *) die 'Unsupported package platform.' ;;
esac
mkdir /var/run/carryfs-install.lock || die 'Another installation is running, or /var/run/carryfs-install.lock needs cleanup after an interrupted installation.'
backup=
changed=0
committed=0
finish() {
    status=$?
    trap - EXIT
    if [ "$changed" = 1 ] && [ "$committed" = 0 ]; then
        printf 'Installation failed; restoring the previous installation.\n' >&2
        index=0
        restored=1
        for path in $paths; do
            index=$((index+1))
            rm -rf "$path" || restored=0
            if [ -e "$backup/$index" ] || [ -L "$backup/$index" ]; then
                cp -pR "$backup/$index" "$path" || restored=0
            fi
        done
        if [ "$platform" = linux-x86_64 ]; then
            udevadm control --reload || restored=0
        fi
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
current=
[ ! -f "$marker" ] || current=$(cat "$marker")
[ "$current" = "$expected" ] || die 'The installed version changed during download; run the installer again.'
for process in carryfs-fuse mount_carryfs; do
    status=0
    pgrep -x "$process" >/dev/null || status=$?
    case $status in
        0) die 'A CarryFS daemon is running. Eject all CarryFS volumes and retry.' ;;
        1) ;;
        *) die 'Cannot check for running CarryFS daemons.' ;;
    esac
done
umask 022
backup=$(mktemp -d /var/tmp/carryfs-install.XXXXXXXX)
index=0
for path in $paths; do
    index=$((index+1))
    if [ -e "$path" ] || [ -L "$path" ]; then cp -pR "$path" "$backup/$index"; fi
done
changed=1
mkdir -p /usr/local/bin
install -m 755 "$payload/carryfs" "$backup/carryfs"
case $platform in
    linux-x86_64)
        if [ -e /usr/sbin/mount.carryfs ] || [ -L /usr/sbin/mount.carryfs ]; then
            [ -L /usr/sbin/mount.carryfs ] && [ "$(readlink /usr/sbin/mount.carryfs)" = /usr/libexec/carryfs/carryfs-fuse ] || die '/usr/sbin/mount.carryfs already belongs to another installation.'
        fi
        mkdir -p /usr/libexec/carryfs /etc/udev/rules.d /etc/udisks2
        # Rename each staged executable so readers never see a partial binary.
        install -m 755 "$payload/carryfs-fuse" /usr/libexec/carryfs/carryfs-fuse.new
        mv -f /usr/libexec/carryfs/carryfs-fuse.new /usr/libexec/carryfs/carryfs-fuse
        if [ ! -L /usr/sbin/mount.carryfs ]; then ln -s /usr/libexec/carryfs/carryfs-fuse /usr/sbin/mount.carryfs; fi
        install -m 644 "$payload/61-carryfs.rules" /etc/udev/rules.d/61-carryfs.rules
        if [ -f /etc/udisks2/mount_options.conf ]; then
            awk '
                FILENAME == ARGV[1] {
                    if(/^carryfs_defaults=/) defaults=$0
                    if(/^carryfs_allow=/) allow=$0
                    next
                }
                /^\[.*\]/ {
                    if(in_defaults) { print defaults; print allow; in_defaults=0 }
                    if($0 ~ /^\[defaults\][[:space:]]*$/) { in_defaults=1; found=1 }
                }
                in_defaults && /^carryfs_(defaults|allow)=/ { next }
                { print }
                END {
                    if(!defaults || !allow) exit 1
                    if(!found) print "\n[defaults]"
                    if(in_defaults || !found) { print defaults; print allow }
                }' "$payload/mount_options.conf" /etc/udisks2/mount_options.conf > "$backup/mount_options.conf"
            install -m 644 "$backup/mount_options.conf" /etc/udisks2/mount_options.conf
        else
            install -m 644 "$payload/mount_options.conf" /etc/udisks2/mount_options.conf
        fi
        install -m 644 "$payload/VERSION" "$marker"
        udevadm control --reload
        ;;
    macos-arm64)
        mkdir -p /Library/Filesystems
        cp -R "$payload/carryfs.fs" "$backup/carryfs.fs"
        install -m 644 "$payload/VERSION" "$backup/carryfs.fs/Contents/VERSION"
        for file in mount_carryfs carryfs.util newfs_carryfs fsck_carryfs; do
            chmod 755 "$backup/carryfs.fs/Contents/Resources/$file"
        done
        rm -rf /Library/Filesystems/carryfs.fs
        cp -R "$backup/carryfs.fs" /Library/Filesystems/carryfs.fs
        ;;
esac
install -m 755 "$backup/carryfs" /usr/local/bin/carryfs.new
mv -f /usr/local/bin/carryfs.new /usr/local/bin/carryfs
committed=1
printf 'System files installed. Reconnect an ejected CarryFS disk to mount it.\n'
