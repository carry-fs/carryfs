#!/usr/bin/env bash
# Use with linux/tests/vm.sh to keep fstests and its images inside the guest.
set -euo pipefail
cd "$(dirname "$0")/../../.."
source=$(realpath "$1")
shift
mount -t tmpfs tmpfs /tmp
cp -a "$source" /var/tmp/fstests
python3 -c 'import shutil; shutil.rmtree("/var/tmp/fstests/results", ignore_errors=True)'
modprobe loop
insmod linux/carryfs.ko
status=0
bash linux/tests/fstests/run.sh /var/tmp/fstests "$@" || status=$?
for result in /var/tmp/fstests/results/generic/*.out.bad; do
    [[ -f $result ]] || continue
    head -80 "$result"
    full=${result%.out.bad}.full
    [[ ! -f $full ]] || tail -80 "$full"
done
df -h /var/tmp
dmesg
exit "$status"
