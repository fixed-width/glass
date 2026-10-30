#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/attr.h>
#include <sys/fsgetpath.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
#include <uuid/uuid.h>

typedef struct {
    uint64_t device, inode, birth_seconds, birth_nanoseconds;
    uint32_t owner;
    int32_t fsid[2];
    uuid_t volume_uuid;
} GlassFileIdentity;

static int volume_uuid(int fd, const char *mount, uuid_t out) {
    struct attrlist attributes = {0};
    attributes.bitmapcount = ATTR_BIT_MAP_COUNT;
    attributes.volattr = ATTR_VOL_INFO | ATTR_VOL_UUID;
    struct { uint32_t length; uuid_t uuid; } value = {0};
    int result = mount ? getattrlist(mount, &attributes, &value, sizeof(value), FSOPT_NOFOLLOW) :
        fgetattrlist(fd, &attributes, &value, sizeof(value), 0);
    if (result) return errno;
    uuid_t zero = {0};
    if (value.length != sizeof(value) || !memcmp(value.uuid, zero, sizeof(zero))) return ENOTSUP;
    memcpy(out, value.uuid, sizeof(uuid_t));
    return 0;
}

int glass_private_file_identity(int fd, GlassFileIdentity *out) {
    struct stat st;
    struct statfs fs;
    if (fstat(fd, &st) || fstatfs(fd, &fs)) return errno;
    *out = (GlassFileIdentity){(uint64_t)st.st_dev, st.st_ino,
        (uint64_t)st.st_birthtimespec.tv_sec, (uint64_t)st.st_birthtimespec.tv_nsec,
        st.st_uid, {fs.f_fsid.val[0], fs.f_fsid.val[1]}, {0}};
    return volume_uuid(fd, NULL, out->volume_uuid);
}

int glass_private_file_path(int fd, char *out) {
    return fcntl(fd, F_GETPATH, out) == 0 ? 0 : errno;
}

int glass_private_resolve_file(const GlassFileIdentity *identity, char *out, size_t size) {
    fsid_t fsid = {{identity->fsid[0], identity->fsid[1]}};
    int count = getfsstat(NULL, 0, MNT_NOWAIT);
    if (count < 0 || count > 4096) return errno ? errno : EOVERFLOW;
    struct statfs *mounts = calloc((size_t)count + 1, sizeof(struct statfs));
    if (!mounts) return ENOMEM;
    int filled = getfsstat(mounts, ((int)sizeof(struct statfs)) * (count + 1), MNT_NOWAIT);
    if (filled < 0 || filled >= count + 1) {
        int failure = filled < 0 ? (errno ? errno : EIO) : EOVERFLOW;
        free(mounts);
        return failure;
    }
    uuid_t zero = {0};
    bool persistent = memcmp(identity->volume_uuid, zero, sizeof(zero)) != 0;
    int matches = 0;
    for (int index = 0; index < filled; index++) {
        bool same = false;
        if (persistent) {
            // The automounter is a placeholder, not a persistent volume.
            if (!strcmp(mounts[index].f_fstypename, "autofs")) continue;
            uuid_t observed = {0};
            same = volume_uuid(-1, mounts[index].f_mntonname, observed) == 0 &&
                !memcmp(observed, identity->volume_uuid, sizeof(observed));
        } else {
            same = mounts[index].f_fsid.val[0] == fsid.val[0] && mounts[index].f_fsid.val[1] == fsid.val[1];
        }
        if (same) { fsid = mounts[index].f_fsid; matches++; }
    }
    free(mounts);
    if (matches > 1) return EOVERFLOW;
    if (!matches) return EAGAIN;
    return fsgetpath(out, size, &fsid, identity->inode) >= 0 ? 0 : errno;
}

