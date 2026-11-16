#!/usr/bin/env python3
"""Check O_TMPFILE and hole punching on a mounted Linux CarryFS adapter."""

import ctypes
import errno
import mmap
import os
from pathlib import Path
import resource
import signal
import sys
import tempfile


def check(mount):
    libc = ctypes.CDLL(None, use_errno=True)
    libc.fallocate.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_longlong, ctypes.c_longlong]
    libc.linkat.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_int]

    def call(result):
        if result < 0:
            code = ctypes.get_errno()
            raise OSError(code, os.strerror(code))

    with tempfile.TemporaryDirectory(dir=mount, prefix="tmpfile-punch-") as directory:
        root = Path(directory)
        fd = os.open(root, os.O_TMPFILE | os.O_RDWR, 0o600)
        try:
            assert not list(root.iterdir())
            assert os.fstat(fd).st_nlink == 0
            unit = 65536
            call(libc.fallocate(fd, 1, 0, 4 * unit))
            assert os.fstat(fd).st_size == 0
            assert os.fstat(fd).st_blocks * 512 >= 4 * unit
            data = bytearray(b"x" * (3 * unit + 19))
            assert os.write(fd, data) == len(data)
            with mmap.mmap(fd, len(data)) as mapped:
                assert mapped[:] == data
                blocks = os.fstat(fd).st_blocks
                call(libc.fallocate(fd, 3, 17, 3 * unit - 10))
                data[17:3 * unit + 7] = bytes(3 * unit - 10)
                assert os.fstat(fd).st_size == len(data)
                assert os.fstat(fd).st_blocks < blocks
                assert mapped[:] == data
                assert os.pread(fd, len(data), 0) == data
            os.fsync(fd)
            call(libc.linkat(-100, os.fsencode(f"/proc/self/fd/{fd}"), -100,
                             os.fsencode(root / "published"), 0x400))
            assert os.fstat(fd).st_nlink == 1
            assert (root / "published").read_bytes() == data
        finally:
            os.close(fd)
        assert (root / "published").read_bytes() == data
        os.unlink(root / "published")

        fd = os.open(root, os.O_TMPFILE | os.O_EXCL | os.O_RDWR, 0o600)
        try:
            result = libc.linkat(-100, os.fsencode(f"/proc/self/fd/{fd}"), -100,
                                 os.fsencode(root / "exclusive"), 0x400)
            assert result == -1 and ctypes.get_errno() == errno.ENOENT
        finally:
            os.close(fd)
        assert not list(root.iterdir())

        # A punch over a hole still updates the times.
        sparse = root / "sparse"
        with open(sparse, "wb") as stream:
            stream.truncate(4 * 65536)
        os.utime(sparse, ns=(10**18, 10**18))
        fd = os.open(sparse, os.O_RDWR)
        try:
            call(libc.fallocate(fd, 3, 65536, 65536))
            stamp = os.fstat(fd)
            assert stamp.st_mtime_ns > 10**18 and stamp.st_ctime_ns > 10**18, stamp
        finally:
            os.close(fd)

        # Growth by fallocate respects RLIMIT_FSIZE.
        limited = os.fork()
        if limited == 0:
            signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
            resource.setrlimit(resource.RLIMIT_FSIZE, (65536, resource.RLIM_INFINITY))
            fd = os.open(root / "limited", os.O_CREAT | os.O_RDWR, 0o600)
            os._exit(0 if libc.fallocate(fd, 0, 0, 2 * 65536) == -1
                     and ctypes.get_errno() == errno.EFBIG else 1)
        assert os.waitpid(limited, 0)[1] == 0, "fallocate past RLIMIT_FSIZE did not fail with EFBIG"
        assert os.stat(root / "limited").st_size == 0


if __name__ == "__main__":
    check(Path(sys.argv[1]))
