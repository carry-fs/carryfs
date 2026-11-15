#!/usr/bin/env python3
"""Check hard links on a mounted Linux CarryFS adapter, kernel or FUSE.

`--keep` leaves `hardlinks-kept/a` and `hardlinks-kept/sub/b` linked;
`--verify-kept` checks them after a remount.
"""

import argparse
import errno
import os
from pathlib import Path
import tempfile


def expect_errno(code, operation):
    try:
        operation()
    except OSError as error:
        assert error.errno == code, (error, code)
    else:
        raise AssertionError(f"expected errno {code}")


def check(mount):
    with tempfile.TemporaryDirectory(dir=mount, prefix="hardlinks-") as directory:
        root = Path(directory)
        a, sub = root / "a", root / "sub"
        sub.mkdir()
        b = sub / "b"
        a.write_bytes(b"shared")
        os.link(a, b)
        assert os.stat(a).st_ino == os.stat(b).st_ino
        assert os.stat(a).st_nlink == os.stat(b).st_nlink == 2
        with open(b, "ab") as stream:
            stream.write(b"!")
        assert a.read_bytes() == b"shared!"

        # Renaming one name onto another name of the same file changes nothing.
        os.rename(a, b)
        assert a.exists() and b.exists() and os.stat(a).st_nlink == 2

        # Directories never link; symlinks link themselves, not their target.
        expect_errno(errno.EPERM, lambda: os.link(sub, root / "dir-link"))
        os.symlink("a", root / "s")
        os.link(root / "s", sub / "s2", follow_symlinks=False)
        assert os.readlink(sub / "s2") == "a"
        assert os.lstat(root / "s").st_ino == os.lstat(sub / "s2").st_ino
        assert os.lstat(root / "s").st_nlink == 2 and os.stat(a).st_nlink == 2
        os.unlink(sub / "s2")
        assert os.lstat(root / "s").st_nlink == 1
        # Names collide case-insensitively.
        expect_errno(errno.EEXIST, lambda: os.link(a, root / "A"))

        # Replacing one name keeps the other.
        (root / "other").write_bytes(b"other")
        os.replace(root / "other", b)
        assert b.read_bytes() == b"other"
        assert os.stat(a).st_nlink == 1 and a.read_bytes() == b"shared!"

        # An open file outlives each name; fstat follows the count down.
        os.link(a, b.with_name("c"))
        fd = os.open(a, os.O_RDONLY)
        try:
            os.unlink(a)
            assert os.fstat(fd).st_nlink == 1
            os.unlink(sub / "c")
            assert os.fstat(fd).st_nlink == 0
            assert os.pread(fd, 7, 0) == b"shared!"
        finally:
            os.close(fd)

        # The directory a parent hint last named can go.
        (root / "kept").write_bytes(b"k")
        os.link(root / "kept", sub / "k")
        os.unlink(sub / "k")
        os.unlink(b)
        sub.rmdir()
        assert os.stat(root / "kept").st_nlink == 1


def keep(mount):
    kept = mount / "hardlinks-kept"
    (kept / "sub").mkdir(parents=True)
    (kept / "a").write_bytes(b"kept")
    os.link(kept / "a", kept / "sub" / "b")


def verify_kept(mount):
    kept = mount / "hardlinks-kept"
    a, b = kept / "a", kept / "sub" / "b"
    assert os.stat(a).st_ino == os.stat(b).st_ino
    assert os.stat(a).st_nlink == 2
    assert b.read_bytes() == b"kept"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mount", type=Path)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--verify-kept", action="store_true")
    args = parser.parse_args()
    if args.verify_kept:
        verify_kept(args.mount)
    else:
        check(args.mount)
        if args.keep:
            keep(args.mount)
