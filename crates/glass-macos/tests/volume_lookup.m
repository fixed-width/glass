// Native syscall interposition for metadata-only volume selection; no GUI or mounts created.
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/attr.h>
#include <sys/fsgetpath.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
#include <uuid/uuid.h>

enum { REAL, UNIQUE, MISSING, DUPLICATE, LEGACY, TRUNCATED };
static int scenario, read_opens, queries, automount_queries;

static int __attribute__((unused)) trapped_open(const char *path, int flags, ...) {
    (void)path; (void)flags;
    read_opens++;
    errno = EPERM;
    return -1;
}

static int mounted(struct statfs *out, int bytes, int flags) {
    if (scenario == REAL) return getfsstat(out, bytes, flags);
    assert(flags == MNT_NOWAIT);
    if (!out) return 4;
    assert(bytes >= (int)(5 * sizeof(*out)));
    if (scenario == TRUNCATED) return 5;
    memset(out, 0, (size_t)bytes);
    const char *names[] = {"/automounter", "/denied", "/selected", "/other"};
    for (int i = 0; i < 4; i++) {
        strcpy(out[i].f_mntonname, names[i]);
        strcpy(out[i].f_fstypename, i == 0 ? "autofs" : "apfs");
        out[i].f_fsid.val[0] = i == 2 ? 17 : 20 + i;
        out[i].f_fsid.val[1] = 26;
    }
    return 4;
}

static int queried_attributes(const char *path, struct attrlist *list, void *out, size_t size, unsigned long flags) {
    if (scenario == REAL) return getattrlist(path, list, out, size, flags);
    queries++;
    assert(list->bitmapcount == ATTR_BIT_MAP_COUNT && list->volattr == (ATTR_VOL_INFO | ATTR_VOL_UUID));
    assert(list->commonattr == 0 && list->dirattr == 0 && list->fileattr == 0 && list->forkattr == 0);
    assert(flags == FSOPT_NOFOLLOW);
    if (!strcmp(path, "/automounter")) { automount_queries++; errno = EIO; return -1; }
    if (!strcmp(path, "/denied")) { errno = EACCES; return -1; }
    struct { uint32_t length; uuid_t uuid; } value = {0};
    assert(size == sizeof(value));
    value.length = sizeof(value);
    value.uuid[0] = scenario == MISSING ? 2 :
        (!strcmp(path, "/selected") || scenario == DUPLICATE ? 1 : 2);
    memcpy(out, &value, sizeof(value));
    return 0;
}

static ssize_t pathname(void *out, size_t size, fsid_t *fsid, uint64_t inode) {
    if (scenario == REAL) return fsgetpath(out, size, fsid, inode);
    assert(fsid->val[0] == 17 && fsid->val[1] == 26 && inode == 99);
    const char *path = "/selected/object";
    assert(size > strlen(path));
    strcpy(out, path);
    return (ssize_t)strlen(path);
}

#define open trapped_open
#define getfsstat mounted
#define getattrlist queried_attributes
#define fsgetpath pathname
#include "file_identity.m"
#undef open
#undef getfsstat
#undef getattrlist
#undef fsgetpath

int main(int argc, char **argv) {
    assert(argc == 2);
    int fd = openat(AT_FDCWD, argv[1], O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    assert(fd >= 0);
    GlassFileIdentity identity = {0};
    assert(glass_private_file_identity(fd, &identity) == 0);
    identity.device ^= 1;
    identity.fsid[0] = identity.fsid[1] = -1;
    char path[4096] = {0};
    assert(glass_private_resolve_file(&identity, path, sizeof(path)) == 0);
    assert(!strcmp(path, argv[1]) && read_opens == 0);
    close(fd);
    identity = (GlassFileIdentity){.device=17,.inode=99,.fsid={17,26},.volume_uuid={1}};
    scenario = UNIQUE;
    assert(glass_private_resolve_file(&identity, path, sizeof(path)) == 0);
    assert(queries == 3 && automount_queries == 0 && read_opens == 0);
    scenario = MISSING;
    assert(glass_private_resolve_file(&identity, path, sizeof(path)) == EAGAIN);
    scenario = DUPLICATE;
    assert(glass_private_resolve_file(&identity, path, sizeof(path)) == EOVERFLOW);
    scenario = TRUNCATED;
    assert(glass_private_resolve_file(&identity, path, sizeof(path)) == EOVERFLOW);
    scenario = LEGACY; queries = 0; memset(identity.volume_uuid, 0, sizeof(uuid_t));
    assert(glass_private_resolve_file(&identity, path, sizeof(path)) == 0);
    assert(queries == 0 && automount_queries == 0 && read_opens == 0);
    puts("real remount lookup and denied/autofs/missing/duplicate/truncated/legacy cases passed; zero read opens");
    return 0;
}
