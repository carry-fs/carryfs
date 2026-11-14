#!/usr/bin/env python3
"""Check O_TMPFILE and hole punching on a mounted Linux CarryFS adapter."""

import ctypes
import errno
import mmap
import os
from pathlib import Path
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


if __name__ == "__main__":
    check(Path(sys.argv[1]))
