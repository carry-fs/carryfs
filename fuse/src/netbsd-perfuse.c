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

/* The leading fields of libperfuse's private struct perfuse_node_data, the
 * same in NetBSD 10 and 11 (lib/libperfuse/perfuse_priv.h). perfuse_init
 * checks them against the root node and refuses to mount if they differ.
 */
struct node_data {
    uint64_t rfh, wfh, nodeid, parent_nodeid;
    uint32_t hashidx;
    uint64_t fuse_nlookup;
    int puffs_nlookup;
    uint64_t lock_owner;
    void *dirent;
    off_t dirent_len;
    void *all_fd;
    size_t all_fd_len;
    uint64_t fd_cookie;
    struct { void *first; void **last; } queue;
    int flags;
    struct { void *next; void **prev; } ident;
    puffs_cookie_t node;
};
#define NODE_DIRTY 0x004
#define FUSE_ROOT 1

static struct node_data *
node_data(puffs_cookie_t node)
{
    return puffs_pn_getpriv((struct puffs_node *)node);
}

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
    struct node_data *top = node_data(puffs_getroot(mount));
    if (top->nodeid != FUSE_ROOT || top->parent_nodeid != FUSE_ROOT ||
        top->node != puffs_getroot(mount))
        errx(1, "CarryFS relay: unsupported libperfuse node layout");
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

/* libperfuse forwards a directory's fsync only once the directory is dirty,
 * and its CREATE and LINK leave the parent clean; mkdir, symlink, remove and
 * rename mark it. Without this, fsync of a directory after creating or linking
 * a file never reaches CarryFS.
 */
int
perfuse_node_create(struct puffs_usermount *mount, puffs_cookie_t parent,
    struct puffs_newinfo *info, const struct puffs_cn *name, const struct vattr *attributes)
{
    int (*next)(struct puffs_usermount *, puffs_cookie_t, struct puffs_newinfo *,
        const struct puffs_cn *, const struct vattr *) = next_symbol("perfuse_node_create");
    int error = next(mount, parent, info, name, attributes);

    if (error == 0)
        node_data(parent)->flags |= NODE_DIRTY;
    return error;
}

int
perfuse_node_link(struct puffs_usermount *mount, puffs_cookie_t parent,
    puffs_cookie_t target, const struct puffs_cn *name)
{
    int (*next)(struct puffs_usermount *, puffs_cookie_t, puffs_cookie_t,
        const struct puffs_cn *) = next_symbol("perfuse_node_link");
    int error = next(mount, parent, target, name);

    if (error == 0)
        node_data(parent)->flags |= NODE_DIRTY;
    return error;
}

/* libperfuse's inactive syncs every dirty node, with no credentials, so each
 * close of a written file published the volume. Close is not a durability
 * point on the other platforms either. The node stays dirty, so a later
 * fsync, which always carries credentials, still reaches CarryFS.
 */
int
perfuse_node_fsync(struct puffs_usermount *mount, puffs_cookie_t node,
    const struct puffs_cred *credentials, int flags, off_t from, off_t to)
{
    int (*next)(struct puffs_usermount *, puffs_cookie_t, const struct puffs_cred *, int,
        off_t, off_t) = next_symbol("perfuse_node_fsync");

    if (credentials == NULL)
        return 0;
    return next(mount, node, credentials, flags, from, to);
}

static int
still_mounted(const char *source)
{
    struct statvfs *mounts;
    int count = getmntinfo(&mounts, ST_NOWAIT);

    /* Zero means the mount table could not be read. Keep the relay alive. */
    if (count == 0)
        return 1;
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
    unsigned warn_after = 2000;
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
    while (result == 0 && still_mounted(source)) {
        nanosleep(&pause, NULL);
        if (warn_after != 0 && --warn_after == 0)
            warnx("CarryFS relay: %s is still mounted after 20 s", source);
    }
    return result;
}
