/* Keep the Rust disk and mount-table ABI in sync with the supported headers. */
#include <sys/types.h>
#include <sys/dkio.h>
#include <sys/disklabel.h>
#include <sys/statvfs.h>
#include <stddef.h>

_Static_assert(DIOCGSECTORSIZE == 0x40046485UL, "sector ioctl changed");
_Static_assert(DIOCGMEDIASIZE == 0x40086484UL, "media ioctl changed");
_Static_assert(DIOCCACHESYNC == 0x80046476UL, "cache flush ioctl changed");
_Static_assert(sizeof(mode_t) == 4, "mode_t changed");
_Static_assert(sizeof(struct statvfs) == 3296, "statvfs size changed");
_Static_assert(_Alignof(struct statvfs) == 8, "statvfs alignment changed");
_Static_assert(offsetof(struct statvfs, f_mntfromname) == 1248,
    "statvfs mount source moved");

_Static_assert(MAXPARTITIONS == 16 && RAW_PART == 3, "amd64 partition encoding changed");

int main(void) { return 0; }
