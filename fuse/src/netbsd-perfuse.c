/* Compatibility for the base libperfuse on NetBSD 10 and 11. Loaded only into
 * CarryFS's own perfused child. Keep the relay's permission checks in libpuffs,
 * which also knows the request's supplementary groups.
 */
#include <sys/types.h>
#include <sys/statvfs.h>
#include <dlfcn.h>
#include <err.h>
#include <puffs.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

struct perfuse_callbacks;
struct perfuse_mount_info;

static void *
next_symbol(const char *name)
{
    void *symbol = dlsym(RTLD_NEXT, name);
    if (symbol == NULL)
        errx(1, "CarryFS relay: %s: %s", name, dlerror());
    return symbol;
}

struct puffs_usermount *
perfuse_init(struct perfuse_callbacks *callbacks, struct perfuse_mount_info *info)
{
    struct puffs_usermount *(*next)(struct perfuse_callbacks *, struct perfuse_mount_info *);
    struct puffs_usermount *mount;
    struct vattr *attributes;
    const char *root = getenv("CARRYFS_PERFUSE_ROOT");
    unsigned uid, gid, mode;
    char trailing;

    if (root == NULL || sscanf(root, "%u:%u:%o%c", &uid, &gid, &mode, &trailing) != 3)
        errx(1, "CarryFS relay: invalid root attributes");
    next = next_symbol("perfuse_init");
    mount = next(callbacks, info);
    if (mount == NULL)
        return NULL;
    /* libperfuse otherwise leaves a 0755 root with an unknown owner, even
     * after GETATTR. Its lookup checks use this private cached vattr.
     */
    attributes = puffs_pn_getvap(puffs_getroot(mount));
    attributes->va_uid = uid;
    attributes->va_gid = gid;
    attributes->va_mode = mode;
    return mount;
}

int
perfuse_node_access(struct puffs_usermount *mount, puffs_cookie_t node, int mode,
    const struct puffs_cred *credentials)
{
    int (*next)(struct puffs_usermount *, puffs_cookie_t, int, const struct puffs_cred *);
    /* VREAD/VWRITE/VEXEC moved to bits 6..8; libperfuse still consumes the
     * PUFFS_VREAD/PUFFS_VWRITE/PUFFS_VEXEC bits 0..2.
     */
    if (mode & 0700)
        mode = (mode >> 6) & 7;
    next = next_symbol("perfuse_node_access");
    return next(mount, node, mode, credentials);
}

static int
still_mounted(const char *source)
{
    struct statvfs *mounts;
    int count = getmntinfo(&mounts, ST_NOWAIT);

    if (count < 0)
        err(1, "CarryFS relay: getmntinfo");
    for (int i = 0; i < count; i++)
        if (strcmp(mounts[i].f_mntfromname, source) == 0 &&
            strncmp(mounts[i].f_fstypename, "puffs", 5) == 0)
            return 1;
    return 0;
}

int
perfuse_mainloop(struct puffs_usermount *mount)
{
    int (*next)(struct puffs_usermount *) = next_symbol("perfuse_mainloop");
    const char *source = getenv("CARRYFS_PERFUSE_SOURCE");
    const struct timespec pause = { 0, 10 * 1000 * 1000 };
    int result;

    if (source == NULL)
        errx(1, "CarryFS relay: missing mount source");
    result = next(mount);
    /* The loop returns once it has answered UNMOUNT, while the kernel is still
     * tearing the mount down. Closing /dev/puffs then can free the putter
     * instance before puffs_vfsop_unmount's seldestroy, which panics NetBSD
     * 10.2 and 11.1. Exit only after the mount has left the mount table; the
     * device lease keeps its source unique.
     */
    for (int waited = 0; result == 0 && still_mounted(source); waited++) {
        if (waited == 2000) {
            warnx("CarryFS relay: %s is still mounted after 20 s", source);
            break;
        }
        nanosleep(&pause, NULL);
    }
    return result;
}
