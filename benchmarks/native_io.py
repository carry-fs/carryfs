"""Native flush primitives for the shared workload; no optional dependencies."""

import ctypes
import functools
import os
from pathlib import Path
import sys
import time


@functools.cache
def windows_api():
    from ctypes import wintypes
    api = ctypes.WinDLL("kernel32", use_last_error=True)
    signatures = {
        "CreateFileW": ([wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
                         wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE], wintypes.HANDLE),
        "FlushFileBuffers": ([wintypes.HANDLE], wintypes.BOOL),
        "CloseHandle": ([wintypes.HANDLE], wintypes.BOOL),
        "CreateMutexW": ([wintypes.LPVOID, wintypes.BOOL, wintypes.LPCWSTR], wintypes.HANDLE),
        "DeviceIoControl": ([wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID, wintypes.DWORD, wintypes.LPVOID,
                             wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID], wintypes.BOOL),
        "GetVolumeInformationW": ([wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.DWORD,
                                    ctypes.POINTER(wintypes.DWORD), ctypes.POINTER(wintypes.DWORD),
                                    ctypes.POINTER(wintypes.DWORD), wintypes.LPWSTR, wintypes.DWORD], wintypes.BOOL),
    }
    for name, (args, result) in signatures.items():
        getattr(api, name).argtypes = args
        getattr(api, name).restype = result
    return api


def open_fd(path, flags, mode=0o777, **kwargs):
    return os.open(path, flags | getattr(os, "O_BINARY", 0), mode, **kwargs)


def flush_fd(descriptor):
    if sys.platform == "darwin":
        import fcntl
        fcntl.fcntl(descriptor, fcntl.F_FULLFSYNC)
    else:
        os.fsync(descriptor)


def volume_info(point):
    from ctypes import wintypes
    api = windows_api()
    flags = wintypes.DWORD()
    filesystem = ctypes.create_unicode_buffer(256)
    if not api.GetVolumeInformationW(str(point), None, 0, None, None, ctypes.byref(flags), filesystem, len(filesystem)):
        raise ctypes.WinError(ctypes.get_last_error(), f"volume information: {point}")
    return dict(fstype=filesystem.value.lower(), readonly=bool(flags.value & 0x80000))


def dismount_volume(volume, patience=10):
    """Lock and dismount a Windows volume, given its volume GUID path.

    Removing an access path leaves the volume mounted and its files cached, so
    a later "cold" phase would read from RAM. A dismount discards that cache;
    the next access mounts the filesystem again. The lock fails while any
    handle is open, so it is retried for `patience` seconds.
    """
    from ctypes import wintypes
    api = windows_api()
    target = str(volume).rstrip("\\")
    handle = api.CreateFileW(target, 0xC0000000, 3, None, 3, 0, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error(), f"open for dismount: {target}")
    try:
        returned = wintypes.DWORD()
        deadline = time.monotonic() + patience
        # FSCTL_LOCK_VOLUME, then FSCTL_DISMOUNT_VOLUME.
        for code, name in ((0x00090018, "lock"), (0x00090020, "dismount")):
            while not api.DeviceIoControl(handle, code, None, 0, None, 0, ctypes.byref(returned), None):
                error = ctypes.get_last_error()
                if name != "lock" or time.monotonic() >= deadline:
                    raise ctypes.WinError(error, f"{name} volume: {target}")
                time.sleep(.1)
    finally:
        if not api.CloseHandle(handle):
            raise ctypes.WinError(ctypes.get_last_error(), f"close dismount handle: {target}")


def sync_path(path):
    if sys.platform == "win32":
        api = windows_api()
        # Directory barriers flush the whole volume, including native NTFS/exFAT.
        target = "\\\\.\\" + Path(path).drive if Path(path).is_dir() else str(path)
        handle = api.CreateFileW(target, 0xC0000000, 7, None, 3, 0, None)
        if handle == ctypes.c_void_p(-1).value:
            raise ctypes.WinError(ctypes.get_last_error(), f"open for flush: {target}")
        try:
            if not api.FlushFileBuffers(handle):
                raise ctypes.WinError(ctypes.get_last_error(), f"FlushFileBuffers: {target}")
        finally:
            if not api.CloseHandle(handle):
                raise ctypes.WinError(ctypes.get_last_error(), f"close flush handle: {target}")
    else:
        descriptor = os.open(path, os.O_RDONLY)
        try:
            if Path(path).is_dir():
                os.fsync(descriptor)
            else:
                flush_fd(descriptor)
        finally:
            os.close(descriptor)
