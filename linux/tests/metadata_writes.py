#!/usr/bin/env python3
"""Check metadata write results when completed buffers can be reclaimed."""

from pathlib import Path
import subprocess
import tempfile


def main():
    repo = Path(__file__).resolve().parents[2]
    source = (repo / "linux/glue.c").read_text()
    start = source.index("int carryfs_write_blocks(")
    end = source.index("\n}", start) + 2
    program = r"""#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#define GFP_NOFS 0
#define REQ_SYNC 0
#define BH_Carryfs_Verified 0
typedef uint64_t u64;
typedef uint8_t u8;
struct super_block { unsigned int s_blocksize; };
struct blk_plug { int unused; };
struct buffer_head {
    u8 b_data[4096];
    unsigned long b_state;
    unsigned int refs;
    bool cached, dirty, locked, failed;
};
struct buffer_head cache[64];
int failed_block;
bool fail_allocation;
unsigned int waited;
void *kcalloc(size_t n, size_t size, int flags) {
    (void)flags;
    return fail_allocation ? NULL : calloc(n, size);
}
void kfree(void *p) { free(p); }
void blk_start_plug(struct blk_plug *plug) { (void)plug; }
void blk_finish_plug(struct blk_plug *plug) {
    (void)plug;
    for (unsigned int i = 0; i < 64; ++i) {
        struct buffer_head *bh = &cache[i];
        if (bh->locked) {
            bh->locked = false;
            bh->failed = (int)i == failed_block;
        }
        /* Completed, clean buffers with no references can be reclaimed. */
        if (!bh->refs && !bh->dirty && !bh->locked)
            bh->cached = false;
    }
}
struct buffer_head *sb_getblk(struct super_block *sb, u64 block) {
    (void)sb;
    assert(block < 64);
    struct buffer_head *bh = &cache[block];
    bh->cached = true;
    ++bh->refs;
    return bh;
}
struct buffer_head *sb_find_get_block(struct super_block *sb, u64 block) {
    return cache[block].cached ? sb_getblk(sb, block) : NULL;
}
void lock_buffer(struct buffer_head *bh) { bh->locked = true; }
void unlock_buffer(struct buffer_head *bh) { bh->locked = false; }
void set_buffer_uptodate(struct buffer_head *bh) { (void)bh; }
void set_bit(int bit, unsigned long *state) { *state |= 1UL << bit; }
void mark_buffer_dirty(struct buffer_head *bh) { bh->dirty = true; }
void write_dirty_buffer(struct buffer_head *bh, int flags) {
    (void)flags;
    bh->dirty = false;
    bh->locked = true;
}
void brelse(struct buffer_head *bh) { assert(bh->refs); --bh->refs; }
void wait_on_buffer(struct buffer_head *bh) {
    assert(bh->refs && !bh->locked);
    ++waited;
}
bool buffer_write_io_error(struct buffer_head *bh) { return bh->failed; }
bool buffer_uptodate(struct buffer_head *bh) { return !bh->failed; }
""" + source[start:end] + r"""
int main(void) {
    struct super_block sb = {4096};
    u8 page[4096] = {0};
    const u8 *pages[64];
    u64 blocks[64];
    for (unsigned int i = 0; i < 64; ++i) {
        pages[i] = page;
        blocks[i] = i;
    }
    unsigned int counts[] = {1, 64};
    for (unsigned int test = 0; test < 2; ++test) {
        unsigned int count = counts[test];
        for (unsigned int fail = 0; fail <= count; ++fail) {
            memset(cache, 0, sizeof(cache));
            failed_block = fail == count ? -1 : (int)fail;
            waited = 0;
            assert(carryfs_write_blocks(&sb, blocks, pages, count) ==
                   (failed_block < 0 ? 0 : -EIO));
            assert(waited == count);
            for (unsigned int i = 0; i < 64; ++i)
                assert(!cache[i].refs && !cache[i].locked && !cache[i].dirty);
        }
    }
    fail_allocation = true;
    waited = 0;
    assert(carryfs_write_blocks(&sb, blocks, pages, 64) == -ENOMEM);
    assert(waited == 0);
}
"""
    with tempfile.TemporaryDirectory(prefix="carryfs-metadata-writes-") as directory:
        unit = Path(directory) / "writes.c"
        binary = Path(directory) / "writes"
        unit.write_text(program)
        subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", str(unit), "-o", str(binary)], check=True)
        subprocess.run([binary], check=True)
    print("metadata writes: reclaimed buffers cannot hide errors; references released")


if __name__ == "__main__":
    main()
