#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
trap 'dmesg | tail -100' ERR
uname -a
getconf PAGESIZE
rg '^CONFIG_(RUST|PAGE_SIZE_4KB|UCLAMP_TASK|CGROUP_WRITEBACK)=' \
    "/usr/lib/modules/$(uname -r)/build/include/config/auto.conf"
modprobe loop
insmod linux/carryfs.ko
python3 linux/tests/check.py --entries 1000 --fsx-operations 500 --seed 17
python3 linux/tests/adapters.py
python3 linux/tests/permissions.py
python3 linux/tests/faults.py
python3 linux/tests/roots.py
python3 linux/tests/replay.py build/check-kernel/replay-log
python3 linux/tests/fuzz.py --cases 30 --seed 17
if dmesg | rg 'BUG:|Oops:|WARNING:|panicked|Call Trace:'; then
    echo "kernel diagnostics found during native checks" >&2
    exit 1
fi
