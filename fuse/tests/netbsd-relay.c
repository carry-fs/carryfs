/* Exercise the relay's exit policy with a fake mount table and clock. */
#include <sys/statvfs.h>
#include <assert.h>
#include <dlfcn.h>
#include <time.h>

static void *find_mainloop(void *, const char *);
static int read_mounts(struct statvfs **, int);
static int sleep_once(const struct timespec *, struct timespec *);

#define dlsym find_mainloop
#define getmntinfo read_mounts
#define nanosleep sleep_once
#include "../src/netbsd-perfuse.c"

static const char source[] = "/dev/rvnd0d";
static unsigned polls, sleeps;
static int fail_first_poll;

static int
finish_mainloop(struct puffs_usermount *mount)
{
    assert(mount == NULL);
    return 0;
}

static void *
find_mainloop(void *handle, const char *name)
{
    assert(handle == RTLD_NEXT);
    assert(strcmp(name, "perfuse_mainloop") == 0);
    return finish_mainloop;
}

static int
read_mounts(struct statvfs **mounts, int flags)
{
    static struct statvfs mount;
    assert(flags == ST_NOWAIT);
    polls++;
    if (fail_first_poll && polls == 1)
        return 0;
    *mounts = &mount;
    strcpy(mount.f_fstypename, "puffs");
    strcpy(mount.f_mntfromname, polls <= 2001 ? source : "/dev/other");
    return 1;
}

static int
sleep_once(const struct timespec *delay, struct timespec *remaining)
{
    assert(delay->tv_sec == 0 && delay->tv_nsec > 0 && remaining == NULL);
    sleeps++;
    return 0;
}

int
main(void)
{
    if (setenv("CARRYFS_PERFUSE_SOURCE", source, 1) != 0)
        err(1, "set relay device");
    for (fail_first_poll = 0; fail_first_poll <= 1; fail_first_poll++) {
        polls = sleeps = 0;
        assert(perfuse_mainloop(NULL) == 0);
        assert(polls == 2002);
        assert(sleeps == 2001);
    }
    return 0;
}
