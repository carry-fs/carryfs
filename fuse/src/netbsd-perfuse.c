/* Compatibility for the base libperfuse on NetBSD 10 and 11. Loaded only into
 * CarryFS's own perfused child. Keep the relay's permission checks in libpuffs,
 * which also knows the request's supplementary groups.
 */
#include <sys/types.h>
#include <dlfcn.h>
#include <err.h>
#include <puffs.h>
#include <stdio.h>
#include <stdlib.h>

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
