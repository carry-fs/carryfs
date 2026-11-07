#!/usr/bin/env python3
"""Compile the real mount guard for supported and unsupported page sizes."""

from pathlib import Path
import subprocess
import tempfile


def main():
    repo = Path(__file__).resolve().parents[2]
    source = (repo / "linux/glue.c").read_text()
    start = source.index("static int carryfs_get_tree(")
    end = source.index("\n}", start) + 2
    # Substitute the kernel boundary so any call past the guard is visible.
    program = """#include <assert.h>
#define PAGE_SIZE PAGE_BYTES
#define EOPNOTSUPP 95
#define SB_SYNCHRONOUS 16
#define SB_DIRSYNC 128
struct fs_context { unsigned int sb_flags; };
static int calls;
static int carryfs_fill_super_fc(void *sb, void *fc) {
    (void)sb; (void)fc; return 0;
}
static int get_tree_bdev(struct fs_context *fc, int (*fill)(void *, void *)) {
    (void)fc; (void)fill; ++calls; return 123;
}
""" + source[start:end] + """
int main(void) {
    struct fs_context fc = {0};
    assert(carryfs_get_tree(&fc) == (PAGE_SIZE == 4096 ? 123 : -EOPNOTSUPP));
    assert(calls == (PAGE_SIZE == 4096));
    unsigned int flags[] = { SB_SYNCHRONOUS, SB_DIRSYNC, SB_SYNCHRONOUS | SB_DIRSYNC };
    for (unsigned int i = 0; i < sizeof(flags) / sizeof(flags[0]); ++i) {
        fc.sb_flags = flags[i];
        assert(carryfs_get_tree(&fc) == -EOPNOTSUPP);
        assert(calls == (PAGE_SIZE == 4096));
    }
}
"""
    with tempfile.TemporaryDirectory(prefix="carryfs-mount-guards-") as directory:
        work = Path(directory)
        unit = work / "guard.c"
        unit.write_text(program)
        for size in (4096, 65536):
            binary = work / f"guard-{size}"
            subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", f"-DPAGE_BYTES={size}",
                            str(unit), "-o", str(binary)], check=True)
            subprocess.run([binary], check=True)
            print(f"PAGE_SIZE={size}: unsupported requests stopped before get_tree_bdev")


if __name__ == "__main__":
    main()
