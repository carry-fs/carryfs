/* Check the macOS helper ioctl constants. Run macos/tests/layout.sh. */
#include <sys/disk.h>

#define CHECK(name, actual, expected) _Static_assert((actual) == (expected), name)

/* macos/src/raw.rs hardcodes these; _IOR encodes the argument size, so a
   changed header would silently make a probe read the wrong thing. */
CHECK("DKIOCGETBLOCKSIZE", DKIOCGETBLOCKSIZE, 0x40046418);
CHECK("DKIOCGETBLOCKCOUNT", DKIOCGETBLOCKCOUNT, 0x40086419);
CHECK("DKIOCGETPHYSICALBLOCKSIZE", DKIOCGETPHYSICALBLOCKSIZE, 0x4004644d);
CHECK("DKIOCSYNCHRONIZECACHE", DKIOCSYNCHRONIZECACHE, 0x20006416);

int main(void) { return 0; }
