"""Run benchmarks/workloads/bench.py on Windows without editing it.

benchmarks/workloads/bench.py assumes POSIX: os.open defaults to text mode on Windows, and
sync_path opens files and directories read-only before fsync, which Windows
refuses. This wrapper opens in binary mode and flushes through
CreateFileW + FlushFileBuffers, the same primitive fsync uses on Windows.
    python bench_win.py MOUNTPOINT LABEL --phase write|read [...]
"""
import ctypes
import os
import sys
from ctypes import wintypes
from pathlib import Path

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from benchmarks.workloads import bench  # noqa: E402

_open = os.open
os.open = lambda path, flags, mode=0o777, **kw: _open(path, flags | os.O_BINARY, mode, **kw)

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
kernel32.CreateFileW.restype = wintypes.HANDLE
kernel32.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
                                 wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
GENERIC_READ, GENERIC_WRITE = 0x80000000, 0x40000000
SHARE_ALL, OPEN_EXISTING, BACKUP_SEMANTICS = 7, 3, 0x02000000
INVALID = wintypes.HANDLE(-1).value


def sync_path(path):
    handle = kernel32.CreateFileW(str(path), GENERIC_READ | GENERIC_WRITE, SHARE_ALL, None,
                                  OPEN_EXISTING, BACKUP_SEMANTICS, None)
    if handle == INVALID:
        raise ctypes.WinError(ctypes.get_last_error(), f"open for flush: {path}")
    try:
        if not kernel32.FlushFileBuffers(handle):
            raise ctypes.WinError(ctypes.get_last_error(), f"FlushFileBuffers: {path}")
    finally:
        kernel32.CloseHandle(handle)


bench.sync_path = sync_path
sys.exit(bench.main())
