#!/usr/bin/env bash
set -euo pipefail
[[ ${GITHUB_ACTIONS:-} == true && ${RUNNER_OS:-} == Linux ]] || {
    echo 'Run only on a disposable GitHub-hosted Linux runner.' >&2
    exit 1
}

mkdir -p smoke-logs
exec > >(tee smoke-logs/ubuntu.log) 2>&1
set -x
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
installer="curl -fsSL https://carryfs.com/install.sh | bash -s -- --version $CARRYFS_VERSION"
expect tests/install-smoke/confirm.exp "$installer" | tee smoke-logs/install.log
grep -F "Installed CarryFS $CARRYFS_VERSION." smoke-logs/install.log

[[ $(cat /usr/libexec/carryfs/VERSION) == "$CARRYFS_VERSION" ]]
test -x /usr/local/bin/carryfs
test -x /usr/libexec/carryfs/carryfs-fuse
for helper in mount mkfs; do
    [[ $(readlink "/usr/sbin/$helper.carryfs") == /usr/libexec/carryfs/carryfs-fuse ]]
done
grep -F '/usr/libexec/carryfs/carryfs-fuse probe' /etc/udev/rules.d/61-carryfs.rules
grep -F 'carryfs_defaults=' /etc/udisks2/mount_options.conf
grep -F 'carryfs_allow=' /etc/udisks2/mount_options.conf

/usr/local/bin/carryfs mkfs "$work/image" 268435456
/usr/local/bin/carryfs check "$work/image"
status=0
/usr/libexec/carryfs/carryfs-fuse > "$work/adapter.log" 2>&1 || status=$?
[[ $status == 1 ]]
grep -F 'usage: carryfs-fuse' "$work/adapter.log"

expect tests/install-smoke/confirm.exp "$installer" | tee smoke-logs/reinstall.log
grep -F "CarryFS $CARRYFS_VERSION is already current" smoke-logs/reinstall.log
expect tests/install-smoke/confirm.exp \
    'curl -fsSL https://carryfs.com/install.sh | bash -s -- --uninstall'
for path in /usr/local/bin/carryfs /usr/libexec/carryfs /usr/sbin/mount.carryfs \
    /usr/sbin/mkfs.carryfs /etc/udev/rules.d/61-carryfs.rules; do
    [[ ! -e $path && ! -L $path ]]
done
if [[ -f /etc/udisks2/mount_options.conf ]]; then
    if grep -E '^carryfs_(defaults|allow)=' /etc/udisks2/mount_options.conf; then
        echo 'CarryFS UDisks options remain after uninstall.' >&2
        exit 1
    fi
fi
printf 'Ubuntu %s: public install, adapter startup, reinstall, and uninstall passed.\n' \
    "$CARRYFS_VERSION" >> "$GITHUB_STEP_SUMMARY"
