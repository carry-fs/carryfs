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
        paths='/usr/local/bin/carryfs /usr/libexec/carryfs /usr/sbin/mount.carryfs /usr/sbin/mkfs.carryfs /etc/udev/rules.d/61-carryfs.rules /etc/udisks2/mount_options.conf'
        ;;
    macos-arm64)
        marker=/Library/Filesystems/carryfs.fs/Contents/VERSION
        paths='/usr/local/bin/carryfs /Library/Filesystems/carryfs.fs'
        ;;
    freebsd14-x86_64|freebsd14-arm64|freebsd15-x86_64|freebsd15-arm64)
        marker=/usr/local/libexec/carryfs/VERSION
        paths='/usr/local/bin/carryfs /usr/local/libexec/carryfs /usr/local/sbin/mount_carryfs /usr/local/sbin/newfs_carryfs'
        ;;
    *) die 'Unsupported package platform.' ;;
esac
mkdir /var/run/carryfs-install.lock || die 'Another installation is running, or /var/run/carryfs-install.lock needs cleanup after an interrupted installation.'
backup=
changed=0
committed=0
native_kernel=
native_stage=
cli_stage=
native_directory_created=0
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
            if [ -n "$native_kernel" ]; then depmod -a "$native_kernel" || restored=0; fi
            udevadm control --reload || restored=0
        fi
        if [ "$restored" = 0 ]; then
            printf 'Restoration failed. Keep the backup at %s and restore it before mounting CarryFS.\n' "$backup" >&2
            backup=
        fi
    fi
    [ -z "$native_stage" ] || rm -f "$native_stage"
    [ -z "$cli_stage" ] || rm -f "$cli_stage"
    if [ "$native_directory_created" = 1 ] && [ "$committed" = 0 ] && [ -d "/lib/modules/$native_kernel/updates" ]; then
        rmdir --ignore-fail-on-non-empty "/lib/modules/$native_kernel/updates"
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
if [ "$platform" = linux-x86_64 ]; then
    [ ! -d /sys/module/carryfs ] || die 'Unload the native module with modprobe -r carryfs before installing.'
    if [ -e "$payload/carryfs.ko" ] || [ -e "$payload/KERNEL_RELEASE" ]; then
        [ -f "$payload/carryfs.ko" ] && [ -f "$payload/KERNEL_RELEASE" ] || die 'Incomplete native module package.'
        native_kernel=$(cat "$payload/KERNEL_RELEASE")
        case $native_kernel in ''|.*|*[!a-zA-Z0-9._+~-]*) die 'Invalid native kernel release.' ;; esac
        [ "$native_kernel" = "$(uname -r)" ] || die 'The native module was not built for the running kernel.'
        [ -d "/lib/modules/$native_kernel" ] || die 'The running kernel module directory is missing.'
        command -v depmod >/dev/null 2>&1 || die 'Missing depmod for native installation.'
        vermagic=$(modinfo -F vermagic "$payload/carryfs.ko")
        [ "${vermagic%% *}" = "$native_kernel" ] || die 'The native module vermagic does not match the running kernel.'
        kernel_config=/lib/modules/$native_kernel/build/include/config/auto.conf
        [ -r "$kernel_config" ] || die 'Cannot verify the kernel module-signing configuration.'
        if grep -q '^CONFIG_MODULE_SIG_FORCE=y$' "$kernel_config"; then
            die 'This installer builds unsigned modules, but this kernel requires trusted signatures.'
        fi
        if [ -e /sys/module/module/parameters/sig_enforce ]; then
            [ "$(cat /sys/module/module/parameters/sig_enforce)" = N ] || die 'This installer builds unsigned modules, but this kernel requires trusted signatures.'
        fi
        if [ -e /sys/kernel/security/lockdown ]; then
            grep -Fq '[none]' /sys/kernel/security/lockdown || die 'Kernel lockdown requires a signed native module; this installer builds unsigned modules.'
        fi
        native_module=/lib/modules/$native_kernel/updates/carryfs.ko
        native_manifest=/usr/libexec/carryfs/native-kernels
        native_owned=0
        [ ! -L "$native_manifest" ] || die 'The native module record must not be a symlink.'
        if [ -f "$native_manifest" ]; then
            while IFS= read -r kernel || [ -n "$kernel" ]; do
                case $kernel in ''|.*|*[!a-zA-Z0-9._+~-]*) die 'Invalid kernel release in the native module record.' ;; esac
                if [ "$kernel" = "$native_kernel" ]; then native_owned=1; fi
            done < "$native_manifest"
        fi
        [ ! -L "$native_module" ] || die 'The native module destination must not be a symlink.'
        if [ -e "$native_module" ]; then
            [ "$native_owned" = 1 ] || die 'The native module destination belongs to another installation.'
        fi
        paths="$paths $native_module"
    fi
fi
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
        for link in /usr/sbin/mount.carryfs /usr/sbin/mkfs.carryfs; do
            if [ -e "$link" ] || [ -L "$link" ]; then
                [ -L "$link" ] && [ "$(readlink "$link")" = /usr/libexec/carryfs/carryfs-fuse ] || die "$link already belongs to another installation."
            fi
        done
        mkdir -p /usr/libexec/carryfs /etc/udev/rules.d /etc/udisks2
        # Rename each staged executable so readers never see a partial binary.
        install -m 755 "$payload/carryfs-fuse" /usr/libexec/carryfs/carryfs-fuse.new
        mv -f /usr/libexec/carryfs/carryfs-fuse.new /usr/libexec/carryfs/carryfs-fuse
        for link in /usr/sbin/mount.carryfs /usr/sbin/mkfs.carryfs; do
            if [ ! -L "$link" ]; then ln -s /usr/libexec/carryfs/carryfs-fuse "$link"; fi
        done
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
        if [ -n "$native_kernel" ]; then
            if [ ! -d "/lib/modules/$native_kernel/updates" ]; then native_directory_created=1; fi
            mkdir -p "/lib/modules/$native_kernel/updates"
            native_stage=$(mktemp "/lib/modules/$native_kernel/updates/.carryfs.XXXXXXXX")
            install -m 644 "$payload/carryfs.ko" "$native_stage"
            mv -f "$native_stage" "$native_module"
            native_stage=
            if [ "$native_owned" = 0 ]; then printf '%s\n' "$native_kernel" >> "$native_manifest"; fi
            depmod -a "$native_kernel"
        fi
        udevadm control --reload
        ;;
    freebsd14-x86_64|freebsd14-arm64|freebsd15-x86_64|freebsd15-arm64)
        for link in /usr/local/sbin/mount_carryfs /usr/local/sbin/newfs_carryfs; do
            if [ -e "$link" ] || [ -L "$link" ]; then
                [ -L "$link" ] && [ "$(readlink "$link")" = /usr/local/libexec/carryfs/carryfs-fuse ] || die "$link already belongs to another installation."
            fi
        done
        mkdir -p /usr/local/libexec/carryfs /usr/local/sbin
        install -m 755 "$payload/carryfs-fuse" /usr/local/libexec/carryfs/carryfs-fuse.new
        mv -f /usr/local/libexec/carryfs/carryfs-fuse.new /usr/local/libexec/carryfs/carryfs-fuse
        for link in /usr/local/sbin/mount_carryfs /usr/local/sbin/newfs_carryfs; do
            if [ ! -L "$link" ]; then ln -s /usr/local/libexec/carryfs/carryfs-fuse "$link"; fi
        done
        install -m 644 "$payload/VERSION" "$marker"
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
cli_stage=$(mktemp /usr/local/bin/.carryfs.XXXXXXXX)
install -m 755 "$backup/carryfs" "$cli_stage"
mv -f "$cli_stage" /usr/local/bin/carryfs
cli_stage=
committed=1
printf 'System files installed. Reconnect an ejected CarryFS disk to mount it.\n'
if [ -n "$native_kernel" ]; then
    printf 'Native module installed for %s, but not loaded. Use modprobe carryfs and mount -i -t carryfs for deliberate native mounts.\n' "$native_kernel"
fi
