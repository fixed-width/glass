/* macOS worker attachment, no event posting or account mutation. */
#import <Foundation/Foundation.h>
#import <OpenDirectory/OpenDirectory.h>
#include <ApplicationServices/ApplicationServices.h>
#include <Security/AuthSession.h>
#include <Security/Security.h>
#include <SystemConfiguration/SystemConfiguration.h>
#include <bsm/libbsm.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <bsm/audit.h>
#include <fcntl.h>
#include <sys/acl.h>
#include <uuid/uuid.h>
#include <limits.h>
#include <membership.h>
#include <pwd.h>
#include <stdatomic.h>
#include <string.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>
#include <libproc.h>
#include <sys/proc_info.h>
#include <sys/mount.h>
#include <sys/fsgetpath.h>
#include <sys/sysctl.h>
#include <sys/user.h>
#include <errno.h>
#include <stdlib.h>
#include <dirent.h>
#include <stdio.h>

typedef struct {
    uint64_t device, inode, birth_seconds, birth_nanoseconds;
    uint32_t owner;
    int32_t fsid[2];
} GlassFileIdentity;

int glass_private_file_identity(int fd, GlassFileIdentity *out) {
    struct stat st;
    struct statfs fs;
    if (fstat(fd, &st) || fstatfs(fd, &fs)) return errno;
    *out = (GlassFileIdentity){(uint64_t)st.st_dev, st.st_ino,
        (uint64_t)st.st_birthtimespec.tv_sec, (uint64_t)st.st_birthtimespec.tv_nsec,
        st.st_uid, {fs.f_fsid.val[0], fs.f_fsid.val[1]}};
    return 0;
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
    bool present = false;
    for (int index = 0; index < filled; index++)
        if (mounts[index].f_fsid.val[0] == fsid.val[0] && mounts[index].f_fsid.val[1] == fsid.val[1])
            present = true;
    free(mounts);
    if (filled < 0) return errno ? errno : EIO;
    if (!present) return EAGAIN;
    return fsgetpath(out, size, &fsid, identity->inode) >= 0 ? 0 : errno;
}

typedef struct { uint32_t pid; uint64_t seconds, microseconds; } GlassProcessIdentity;

int glass_private_processes(uint32_t uid, GlassProcessIdentity *out, int capacity) {
    int *pids = calloc((size_t)capacity, sizeof(int));
    if (!pids) return -ENOMEM;
    errno = 0;
    int bytes = proc_listpids(PROC_UID_ONLY, uid, pids, capacity * (int)sizeof(int));
    if (bytes < 0 || (bytes == 0 && errno) || bytes >= capacity * (int)sizeof(int)) {
        int failure = bytes >= capacity * (int)sizeof(int) ? EOVERFLOW : (errno ? errno : EIO);
        free(pids);
        return -failure;
    }
    int count = 0;
    for (int index = 0; index < bytes / (int)sizeof(int); index++) {
        if (pids[index] <= 0) continue;
        struct kinfo_proc info = {0};
        size_t length = sizeof(info);
        int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, pids[index]};
        errno = 0;
        int status = sysctl(mib, 4, &info, &length, NULL, 0);
        if (status != 0 || length != sizeof(info)) {
            if (status == 0 && length == 0) continue;
            if (errno == ESRCH || errno == ENOENT) continue;
            int failure = errno ? errno : EIO;
            free(pids);
            return -failure;
        }
        if (info.kp_eproc.e_ucred.cr_uid != uid) continue;
        out[count++] = (GlassProcessIdentity){(uint32_t)info.kp_proc.p_pid,
            (uint64_t)info.kp_proc.p_starttime.tv_sec, (uint64_t)info.kp_proc.p_starttime.tv_usec};
    }
    free(pids);
    return count;
}

int glass_private_uid_empty(uint32_t uid) {
    GlassProcessIdentity sample[16];
    return glass_private_processes(uid, sample, 16) == 0;
}

static atomic_uint pinned_asid;
static atomic_uint pinned_uid;
static atomic_uint pinned_owner;
static atomic_uint registration_owner;

typedef struct {
    bool user, dictionary_owner, console_known, on_console, login_known, login_done;
    bool security_known, graphic, root_security, audit_owner, audit_session;
    unsigned asid;
} Context;

static CFTypeRef item(CFDictionaryRef d, CFStringRef a, CFStringRef b) {
    if (!d) return NULL;
    CFTypeRef v = CFDictionaryGetValue(d, a);
    return v ? v : CFDictionaryGetValue(d, b);
}

static bool boolean(CFTypeRef v, bool *out) {
    if (!v || CFGetTypeID(v) != CFBooleanGetTypeID()) return false;
    *out = CFBooleanGetValue(v);
    return true;
}

static int login_observation(CFPropertyListRef value, uint32_t owner, uint32_t target) {
    if (!value || CFGetTypeID(value) != CFDictionaryGetTypeID()) return -EPROTO;
    CFTypeRef sessions = CFDictionaryGetValue(value, CFSTR("SessionInfo"));
    if (!sessions || CFGetTypeID(sessions) != CFArrayGetTypeID()) return -EPROTO;
    CFIndex count = CFArrayGetCount(sessions);
    if (!count || count > 1024) return -EPROTO;
    unsigned controls = 0, matches = 0;
    bool completed = false;
    for (CFIndex i = 0; i < count; i++) {
        CFTypeRef session = CFArrayGetValueAtIndex(sessions, i);
        if (!session || CFGetTypeID(session) != CFDictionaryGetTypeID()) return -EPROTO;
        CFTypeRef number = item(session, CFSTR("kCGSSessionUserIDKey"), CFSTR("kCGSessionUserIDKey"));
        int64_t uid = -1;
        bool done = false;
        if (!number || CFGetTypeID(number) != CFNumberGetTypeID() || CFNumberIsFloatType(number) ||
            !CFNumberGetValue(number, kCFNumberSInt64Type, &uid) || uid < 0 || uid > UINT32_MAX ||
            !boolean(item(session, CFSTR("kCGSessionLoginDoneKey"), CFSTR("kCGSSessionLoginDoneKey")), &done))
            return -EPROTO;
        if ((uint32_t)uid == owner && done) controls++;
        if ((uint32_t)uid == target) { matches++; completed = done; }
    }
    if (controls != 1 || matches > 1) return -EAGAIN;
    return matches ? (completed ? 2 : 1) : 0;
}

// A setup hint only; authenticated worker facts still decide whether Glass is ready.
int glass_private_login_observation(uint32_t target) {
    if (target < 501 || getuid() < 501 || getuid() != geteuid()) return -EINVAL;
    SCDynamicStoreRef store = SCDynamicStoreCreate(NULL, CFSTR("Glass account setup"), NULL, NULL);
    if (!store) return -EIO;
    CFPropertyListRef value = SCDynamicStoreCopyValue(store, CFSTR("State:/Users/ConsoleUser"));
    int result = login_observation(value, getuid(), target);
    if (value) CFRelease(value);
    CFRelease(store);
    return result;
}

#ifdef GLASS_PRIVATE_SESSION_TEST
static int login_observation_test(void) {
    @autoreleasepool {
        NSDictionary *owner = @{@"kCGSSessionUserIDKey":@501, @"kCGSessionLoginDoneKey":@YES};
        NSDictionary *complete = @{@"kCGSSessionUserIDKey":@502, @"kCGSessionLoginDoneKey":@YES};
        NSDictionary *starting = @{@"kCGSSessionUserIDKey":@502, @"kCGSessionLoginDoneKey":@NO};
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[owner,complete]},501,502) != 2) return 1;
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[owner,starting]},501,502) != 1) return 2;
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[owner]},501,502) != 0) return 3;
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[complete]},501,502) >= 0) return 4;
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[owner,complete,complete]},501,502) >= 0) return 5;
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[owner,@{}]},501,502) >= 0) return 6;
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[]},501,502) >= 0) return 7;
        if (login_observation((__bridge CFPropertyListRef)@{@"SessionInfo":@[owner,@{@"kCGSSessionUserIDKey":@502,@"kCGSessionLoginDoneKey":@1}]},501,502) >= 0) return 8;
        return 0;
    }
}
#endif

static Context read_context(void) {
    Context c = {0};
    c.user = getuid() != 0 && getuid() == geteuid();
    CFDictionaryRef d = CGSessionCopyCurrentDictionary();
    c.console_known = boolean(item(d, CFSTR("kCGSSessionOnConsoleKey"),
                                  CFSTR("kCGSessionOnConsoleKey")), &c.on_console);
    c.login_known = boolean(item(d, CFSTR("kCGSessionLoginDoneKey"),
                                CFSTR("kCGSSessionLoginDoneKey")), &c.login_done);
    CFTypeRef u = item(d, CFSTR("kCGSSessionUserIDKey"), CFSTR("kCGSessionUserIDKey"));
    int64_t uid = -1;
    c.dictionary_owner = u && CFGetTypeID(u) == CFNumberGetTypeID() &&
        !CFNumberIsFloatType(u) && CFNumberGetValue(u, kCFNumberSInt64Type, &uid) &&
        uid >= 0 && (uint64_t)uid == getuid();
    if (d) CFRelease(d);
    SecuritySessionId sid = 0;
    SessionAttributeBits bits = 0;
    c.security_known = SessionGetInfo(callerSecuritySession, &sid, &bits) == errSecSuccess && sid;
    c.graphic = (bits & sessionHasGraphicAccess) != 0;
    c.root_security = (bits & sessionIsRoot) != 0;
    auditinfo_addr_t audit = {0};
    if (getaudit_addr(&audit, sizeof(audit)) == 0) {
        c.audit_owner = audit.ai_auid == getuid();
        c.audit_session = audit.ai_asid > 0 && audit.ai_asid != AU_DEFAUDITSID;
        c.asid = audit.ai_asid;
    }
    return c;
}

static bool context_allowed(Context c, bool onboarding) {
    return c.user && c.dictionary_owner && c.console_known && (onboarding || !c.on_console) &&
        c.login_known && c.login_done && c.security_known && c.graphic && !c.root_security &&
        c.audit_owner && c.audit_session;
}

int glass_private_writes_protected(int fd, uint32_t owner) {
    uuid_t owner_id, root_id;
    if (mbr_uid_to_uuid(owner, owner_id) || mbr_uid_to_uuid(0, root_id)) return EIO;
    acl_t acl = acl_get_fd_np(fd, ACL_TYPE_EXTENDED);
    if (!acl) return errno == ENOENT ? 0 : (errno ? errno : EIO);
    acl_entry_t entry;
    int index = ACL_FIRST_ENTRY, result = 0, next;
    const acl_perm_t writes[] = {ACL_WRITE_DATA, ACL_APPEND_DATA, ACL_DELETE, ACL_DELETE_CHILD,
        ACL_WRITE_ATTRIBUTES, ACL_WRITE_EXTATTRIBUTES, ACL_WRITE_SECURITY, ACL_CHANGE_OWNER};
    while ((next = acl_get_entry(acl, index, &entry)) == 0) {
        index = ACL_NEXT_ENTRY;
        acl_tag_t tag;
        if (acl_get_tag_type(entry, &tag)) { result = EIO; break; }
        if (tag != ACL_EXTENDED_ALLOW) continue;
        void *who = acl_get_qualifier(entry);
        if (!who) { result = EIO; break; }
        bool allowed = memcmp(who, owner_id, sizeof(uuid_t)) == 0 ||
            memcmp(who, root_id, sizeof(uuid_t)) == 0;
        acl_free(who);
        if (allowed) continue;
        acl_permset_t perms;
        if (acl_get_permset(entry, &perms)) { result = EIO; break; }
        for (unsigned i = 0; i < sizeof(writes) / sizeof(writes[0]); i++) {
            int present = acl_get_perm_np(perms, writes[i]);
            if (present != 0) { result = present < 0 ? EIO : EPERM; break; }
        }
        if (result) break;
    }
    // Darwin reports EINVAL when iteration reaches the end of a valid ACL.
    if (!result && (next != -1 || errno != EINVAL)) result = errno ? errno : EIO;
    acl_free(acl);
    return result;
}

static bool protected_directory(int fd) {
    struct stat s;
    return fd >= 0 && fstat(fd, &s) == 0 && S_ISDIR(s.st_mode) && s.st_uid == 0 &&
        !(s.st_mode & 0022) && glass_private_writes_protected(fd, 0) == 0;
}

static NSDictionary *enrollment_at(bool legacy, bool allow_disabled) {
    int fd = open("/", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    char owner[16];
    uint32_t selected = atomic_load(&registration_owner);
    snprintf(owner, sizeof(owner), "%u", selected ? selected : getuid());
    const char *parents[] = {"Library", "Application Support", "Glass", "accounts", owner};
    for (unsigned i = 0; i < (legacy ? 3u : 5u); i++) {
        if (!protected_directory(fd)) { if (fd >= 0) close(fd); return nil; }
        int next = openat(fd, parents[i], O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        close(fd);
        fd = next;
    }
    if (!protected_directory(fd)) { if (fd >= 0) close(fd); return nil; }
    struct stat disabled;
    if (!allow_disabled && (fstatat(fd, "disabled", &disabled, AT_SYMLINK_NOFOLLOW) == 0 || errno != ENOENT)) {
        close(fd); return nil;
    }
    int file = openat(fd, "account.plist", O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK);
    close(fd);
    struct stat s, after;
    if (file < 0) return nil;
    if (fstat(file, &s) || !S_ISREG(s.st_mode) || s.st_uid != 0 || s.st_nlink != 1 ||
        (s.st_mode & 0022) || glass_private_writes_protected(file, 0) || s.st_size <= 0 || s.st_size > 4096) { close(file); return nil; }
    char bytes[4097];
    ssize_t n = read(file, bytes, sizeof(bytes));
    bool stable = n == s.st_size && fstat(file, &after) == 0 &&
        s.st_size == after.st_size && s.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec &&
        s.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec &&
        s.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec &&
        s.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec;
    close(file);
    if (!stable) return nil;
    id p = [NSPropertyListSerialization propertyListWithData:[NSData dataWithBytes:bytes length:n]
        options:NSPropertyListImmutable format:NULL error:NULL];
    return [p isKindOfClass:[NSDictionary class]] ? p : nil;
}

static bool integer(id n, uint32_t *out) {
    if (!n || CFGetTypeID((__bridge CFTypeRef)n) != CFNumberGetTypeID() ||
        CFNumberIsFloatType((__bridge CFNumberRef)n)) return false;
    int64_t v;
    if (!CFNumberGetValue((__bridge CFNumberRef)n, kCFNumberSInt64Type, &v) || v <= 0 ||
        v > UINT32_MAX) return false;
    *out = (uint32_t)v;
    return true;
}

static bool owner_bound(uint32_t recorded, uint32_t selected, uint32_t caller) {
    return recorded == (selected ? selected : caller);
}

static bool account_matches(bool worker) {
    NSDictionary *p = enrollment_at(false, false);
    NSSet *keys = [NSSet setWithArray:@[@"version", @"uid", @"gid", @"record_uuid", @"name", @"home", @"owner_uid"]];
    if (!p || ![[NSSet setWithArray:p.allKeys] isEqual:keys]) return false;
    uint32_t version, uid, gid, owner;
    if (!integer(p[@"version"], &version) || version != 1 || !integer(p[@"uid"], &uid) ||
        !integer(p[@"gid"], &gid) || !integer(p[@"owner_uid"], &owner) || owner == uid || getuid() != geteuid() ||
        (worker ? (uid != getuid() || gid != getgid() || getgid() != getegid()) : owner != getuid())) return false;
    uint32_t selected = atomic_load(&registration_owner);
    if (!owner_bound(owner, selected, getuid())) return false;
    for (NSString *key in @[@"record_uuid", @"name", @"home"])
        if (![p[key] isKindOfClass:[NSString class]] || ![p[key] length]) return false;
    NSUUID *uuid = [[NSUUID alloc] initWithUUIDString:p[@"record_uuid"]];
    if (!uuid || ![p[@"home"] hasPrefix:@"/"]) return false;
    struct passwd *pw = getpwuid(uid);
    if (!pw || pw->pw_gid != gid || ![p[@"name"] isEqualToString:@(pw->pw_name)] ||
        ![p[@"home"] isEqualToString:@(pw->pw_dir)]) return false;
    ODNode *node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:NULL];
    ODRecord *record = [node recordWithRecordType:kODRecordTypeUsers name:p[@"name"]
        attributes:@[kODAttributeTypeGUID, kODAttributeTypeUniqueID, kODAttributeTypePrimaryGroupID,
                     kODAttributeTypeNFSHomeDirectory] error:NULL];
    if (!record) return false;
    NSDictionary *expected = @{kODAttributeTypeGUID:uuid.UUIDString,
        kODAttributeTypeUniqueID:[NSString stringWithFormat:@"%u", uid],
        kODAttributeTypePrimaryGroupID:[NSString stringWithFormat:@"%u", gid],
        kODAttributeTypeNFSHomeDirectory:p[@"home"]};
    for (NSString *key in expected) {
        NSArray *values = [record valuesForAttribute:key error:NULL];
        if (values.count != 1 || ![values[0] isKindOfClass:[NSString class]]) return false;
        if ([key isEqualToString:kODAttributeTypeGUID]) {
            if ([values[0] caseInsensitiveCompare:expected[key]] != NSOrderedSame) return false;
        } else if (![values[0] isEqualToString:expected[key]]) return false;
    }
    uuid_t user_id, admin_id;
    int admin = 1;
    if (mbr_uid_to_uuid(uid, user_id) || mbr_gid_to_uuid(80, admin_id) ||
        mbr_check_membership(user_id, admin_id, &admin) || admin) return false;
    atomic_store(&pinned_owner, owner);
    return true;
}

uint32_t glass_private_legacy_owner(void) {
    @autoreleasepool {
        NSDictionary *p = enrollment_at(true, true);
        uint32_t version, owner;
        return p && p.count == 7 && integer(p[@"version"], &version) && version == 1 &&
            integer(p[@"owner_uid"], &owner) && owner >= 501 ? owner : 0;
    }
}

int glass_private_set_owner(uint32_t owner) {
    if (owner < 501 || atomic_load(&pinned_asid)) return 77;
    uint32_t previous = 0;
    return atomic_compare_exchange_strong(&registration_owner, &previous, owner) || previous == owner ? 0 : 77;
}

int glass_private_is_owner(void) {
    @autoreleasepool {
        if (!getuid() || getuid() != geteuid()) return 0;
        return account_matches(false) ? 1 : 0;
    }
}

uint32_t glass_private_owner_uid(void) { return atomic_load(&pinned_owner); }

int glass_private_attached(void) {
    return atomic_load(&pinned_uid) == getuid() && atomic_load(&pinned_asid) != 0;
}

uint32_t glass_private_worker_uid(void) {
    @autoreleasepool {
        NSDictionary *p = enrollment_at(false, false);
        uint32_t version, owner, uid;
        if (!p || p.count != 7 || !integer(p[@"version"], &version) || version != 1 ||
            !integer(p[@"owner_uid"], &owner) || !integer(p[@"uid"], &uid) ||
            owner != getuid() || owner == uid || getuid() != geteuid()) return 0;
        return uid;
    }
}

char *glass_private_owner_home(void) {
    if (!glass_private_is_owner()) return NULL;
    struct passwd *owner = getpwuid(getuid());
    return owner && owner->pw_dir ? strdup(owner->pw_dir) : NULL;
}

int glass_private_attach(int onboarding) {
    @autoreleasepool {
        Context c = read_context();
        if (!context_allowed(c, onboarding != 0)) return 77;
        if (!account_matches(true)) return 78;
        atomic_store(&pinned_uid, getuid());
        atomic_store(&pinned_asid, c.asid);
        return 0;
    }
}

int glass_private_still_private(void) {
    Context c = read_context();
    return context_allowed(c, false) && atomic_load(&pinned_uid) == getuid() &&
        atomic_load(&pinned_asid) == c.asid ? 0 : 77;
}

#ifdef GLASS_PRIVATE_SESSION_TEST
static bool platform_service_status(uint32_t status);
int main(void) {
    Context good = {true,true,true,false,true,true,true,true,false,true,true,42};
    if (!context_allowed(good, false) || !context_allowed(good, true)) return 1;
    Context c = good; c.on_console = true;
    if (context_allowed(c, false) || !context_allowed(c, true)) return 2;
    bool *required[] = {&good.user, &good.dictionary_owner, &good.console_known, &good.login_known,
                       &good.login_done, &good.security_known, &good.graphic, &good.audit_owner, &good.audit_session};
    for (unsigned i = 0; i < 9; i++) {
        *required[i] = false;
        if (context_allowed(good, false) || context_allowed(good, true)) return 3;
        *required[i] = true;
    }
    c = good; c.root_security = true;
    if (context_allowed(c, false) || context_allowed(c, true)) return 4;
    if (glass_private_still_private() == 0 || glass_private_attach(0) == 0 || glass_private_attach(1) == 0) return 5;
    uint32_t n;
    if (integer(@YES, &n) || integer(@0, &n) || integer(@(-1), &n) || integer(@1.5, &n) ||
        integer(@4294967296LL, &n) || integer(@"1", &n) || !integer(@1, &n) || n != 1) return 6;
    if (!owner_bound(501, 501, 502) || owner_bound(503, 501, 502) ||
        !owner_bound(501, 0, 501) || owner_bound(502, 0, 501)) return 7;
    if (login_observation_test()) return 8;
    uint32_t runtime = kSecCodeStatusValid | kSecCodeSignatureRuntime;
    if (platform_service_status(runtime) ||
        !platform_service_status(runtime | kSecCodeSignatureLibraryValidation) ||
        !platform_service_status(runtime | 0x10) ||
        platform_service_status(runtime | 0x10 | kSecCodeStatusDebugged) ||
        platform_service_status((runtime | 0x10) & ~kSecCodeStatusValid)) return 9;
    puts("{\"completed\":true,\"context_refusal_cases\":11,\"integer_type_cases\":7,\"login_observation_cases\":8,\"platform_status_cases\":5,\"ssh_attachment_refused\":true,\"gui_started\":false}");
    return 0;
}
#endif

int glass_private_owns_pid(int pid) {
    unsigned uid = atomic_load(&pinned_uid);
    if (!uid || uid != getuid() || pid <= 0) return 0;
    struct proc_bsdinfo info = {0};
    return proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == (int)sizeof(info)
        && info.pbi_pid == (unsigned)pid && info.pbi_uid == uid;
}

int glass_private_is_worker(void) {
    @autoreleasepool {
        if (atomic_load(&registration_owner)) return account_matches(true) ? 1 : 0;
        DIR *entries = opendir("/Library/Application Support/Glass/accounts");
        if (!entries) return 0;
        uint32_t found = 0;
        unsigned count = 0;
        struct dirent *entry;
        bool ambiguous = false;
        while ((entry = readdir(entries))) {
            if (++count > 1024) { ambiguous = true; break; }
            char *end = NULL;
            errno = 0;
            unsigned long value = strtoul(entry->d_name, &end, 10);
            if (errno || !*entry->d_name || *end || value < 501 || value > UINT32_MAX) continue;
            char canonical[16]; snprintf(canonical, sizeof(canonical), "%u", (uint32_t)value);
            if (strcmp(canonical, entry->d_name)) continue;
            atomic_store(&registration_owner, (uint32_t)value);
            if (account_matches(true)) {
                if (found) { ambiguous = true; break; }
                found = (uint32_t)value;
            }
        }
        closedir(entries);
        atomic_store(&registration_owner, ambiguous ? 0 : found);
        if (ambiguous || !found) { atomic_store(&pinned_owner, 0); return 0; }
        return 1;
    }
}

uint32_t glass_private_session_id(void) { return atomic_load(&pinned_asid); }

int glass_private_boot_identity(uint8_t *out) {
    char text[37] = {0};
    size_t size = sizeof(text);
    if (sysctlbyname("kern.bootsessionuuid", text, &size, NULL, 0)) return errno;
    if (size != sizeof(text) || text[36] != '\0' || uuid_parse(text, out)) return EINVAL;
    return 0;
}

// Read-only lifetime lookup; its positive control must pass before absence is trusted.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
int glass_private_session_exists(uint32_t asid, uint32_t uid) {
    if (!asid || asid > INT_MAX) return -EINVAL;
    auditinfo_addr_t own = {0}, control = {0}, selected = {0};
    if (getaudit_addr(&own, sizeof(own)) || !own.ai_asid || own.ai_asid == AU_DEFAUDITSID)
        return -(errno ? errno : EIO);
    control.ai_asid = own.ai_asid;
    if (auditon(A_GETSINFO_ADDR, &control, sizeof(control))) return -errno;
    if (control.ai_asid != own.ai_asid || control.ai_auid != own.ai_auid) return -EIO;
    selected.ai_asid = asid;
    if (auditon(A_GETSINFO_ADDR, &selected, sizeof(selected)))
        return errno == EINVAL ? 0 : -errno;
    if (selected.ai_asid != (au_asid_t)asid || selected.ai_auid != uid) return -EACCES;
    return 1;
}

#pragma clang diagnostic pop

static bool platform_service_status(uint32_t status) {
    // XNU cs_blobs.h exposes CS_FORCED_LV (0x10) through kSecCodeInfoStatus.
    const uint32_t library_validation = kSecCodeSignatureLibraryValidation | 0x10;
    return (status & kSecCodeStatusValid) && (status & kSecCodeSignatureRuntime) &&
        (status & library_validation) && !(status & kSecCodeStatusDebugged);
}

static OSStatus hardened_service(SecCodeRef code, bool platform_runtime) {
    CFDictionaryRef info = NULL;
    OSStatus status = SecCodeCopySigningInformation(code,
        kSecCSSigningInformation | kSecCSDynamicInformation, &info);
    if (status) return status;
    uint32_t flags = 0, dynamic = 0;
    CFTypeRef value = CFDictionaryGetValue(info, kSecCodeInfoFlags);
    CFTypeRef state = CFDictionaryGetValue(info, kSecCodeInfoStatus);
    if (!value || !state || CFGetTypeID(value) != CFNumberGetTypeID() ||
        CFGetTypeID(state) != CFNumberGetTypeID() ||
        !CFNumberGetValue(value, kCFNumberSInt32Type, &flags) ||
        !CFNumberGetValue(state, kCFNumberSInt32Type, &dynamic) ||
        !((flags | (platform_runtime ? dynamic : 0)) & kSecCodeSignatureRuntime) ||
        !(dynamic & kSecCodeStatusValid) ||
        (dynamic & kSecCodeStatusDebugged)) status = errSecParam;
    if (platform_runtime && !platform_service_status(dynamic)) status = errSecParam;
    CFTypeRef entitlements = CFDictionaryGetValue(info, kSecCodeInfoEntitlementsDict);
    if (entitlements) {
        if (CFGetTypeID(entitlements) != CFDictionaryGetTypeID()) status = errSecParam;
        else {
            CFStringRef unsafe_keys[] = {CFSTR("com.apple.security.get-task-allow"),
                CFSTR("com.apple.security.cs.disable-library-validation"),
                CFSTR("com.apple.security.cs.allow-dyld-environment-variables")};
            for (unsigned i = 0; i < sizeof(unsafe_keys) / sizeof(unsafe_keys[0]); i++) {
                CFTypeRef enabled = CFDictionaryGetValue(entitlements, unsafe_keys[i]);
                if (enabled && enabled != kCFBooleanFalse) status = errSecParam;
            }
        }
    }
    CFRelease(info);
    return status;
}

static bool process_matches(const GlassProcessIdentity *process, uint32_t uid) {
    struct kinfo_proc info = {0};
    size_t size = sizeof(info);
    int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, (int)process->pid};
    return sysctl(mib, 4, &info, &size, NULL, 0) == 0 && size == sizeof(info) &&
        info.kp_eproc.e_ucred.cr_uid == uid &&
        (uint64_t)info.kp_proc.p_starttime.tv_sec == process->seconds &&
        (uint64_t)info.kp_proc.p_starttime.tv_usec == process->microseconds;
}

// This is an eligibility observation, not permission to ignore a process without its launchd job.
int glass_private_idle_executable(const GlassProcessIdentity *process, uint32_t uid,
                                 char *out, size_t capacity) {
    @autoreleasepool {
        uint32_t dirty = 0;
        if (!process_matches(process, uid) || proc_get_dirty((int)process->pid, &dirty) ||
            dirty != (PROC_DIRTY_TRACKED | PROC_DIRTY_ALLOWS_IDLE_EXIT)) return 0;
        char path[PROC_PIDPATHINFO_MAXSIZE] = {0};
        if (proc_pidpath((int)process->pid, path, sizeof(path)) <= 0) return 0;
        struct stat before = {0}, after = {0};
        if (lstat(path, &before) || !S_ISREG(before.st_mode) || before.st_uid != 0 ||
            (before.st_mode & 022)) return 0;
        SecCodeRef code = NULL;
        SecStaticCodeRef disk = NULL;
        SecRequirementRef apple = NULL;
        CFDictionaryRef live_info = NULL, disk_info = NULL;
        NSDictionary *attributes = @{(__bridge NSString *)kSecGuestAttributePid: @(process->pid)};
        NSURL *url = [NSURL fileURLWithFileSystemRepresentation:path isDirectory:NO relativeToURL:nil];
        OSStatus status = SecRequirementCreateWithString(CFSTR("anchor apple"), 0, &apple);
        if (!status) status = SecCodeCopyGuestWithAttributes(NULL,
            (__bridge CFDictionaryRef)attributes, 0, &code);
        if (!status) status = SecCodeCheckValidity(code, kSecCSStrictValidate, apple);
        // OS code can receive runtime enforcement from the kernel rather than its static flags.
        if (!status) status = hardened_service(code, true);
        if (!status) status = SecCodeCopySigningInformation(code,
            kSecCSSigningInformation | kSecCSDynamicInformation, &live_info);
        // Preserve the live architecture (including arm64e) when validating its on-disk code.
        if (!status) status = SecCodeCopyStaticCode(code, 0, &disk);
        if (!status) status = SecStaticCodeCheckValidity(disk, kSecCSStrictValidate, apple);
        if (!status) status = SecCodeCopySigningInformation(disk, kSecCSSigningInformation, &disk_info);
        bool eligible = false;
        if (!status) {
            NSDictionary *live = (__bridge NSDictionary *)live_info;
            NSDictionary *stored = (__bridge NSDictionary *)disk_info;
            id platform = live[(__bridge NSString *)kSecCodeInfoPlatformIdentifier];
            id digest = live[(__bridge NSString *)kSecCodeInfoUnique];
            NSURL *executable = live[(__bridge NSString *)kSecCodeInfoMainExecutable];
            eligible = [platform isKindOfClass:NSNumber.class] && [platform unsignedIntValue] != 0 &&
                [digest isKindOfClass:NSData.class] && [digest length] != 0 &&
                [digest isEqual:stored[(__bridge NSString *)kSecCodeInfoUnique]] &&
                [executable isKindOfClass:NSURL.class] && [executable.path isEqual:url.path];
            NSDictionary *entitlements = live[(__bridge NSString *)kSecCodeInfoEntitlementsDict];
            for (NSString *key in @[@"com.apple.security.cs.allow-jit",
                    @"com.apple.security.cs.allow-unsigned-executable-memory",
                    @"com.apple.private.security.clear-library-validation"]) {
                id value = entitlements[key];
                if (value && ![value isEqual:@NO]) eligible = false;
            }
        }
        if (live_info) CFRelease(live_info);
        if (disk_info) CFRelease(disk_info);
        if (apple) CFRelease(apple);
        if (disk) CFRelease(disk);
        if (code) CFRelease(code);
        if (!eligible || lstat(path, &after) || before.st_dev != after.st_dev ||
            before.st_ino != after.st_ino || before.st_mtimespec.tv_sec != after.st_mtimespec.tv_sec ||
            before.st_mtimespec.tv_nsec != after.st_mtimespec.tv_nsec ||
            !process_matches(process, uid) || proc_get_dirty((int)process->pid, &dirty) ||
            dirty != (PROC_DIRTY_TRACKED | PROC_DIRTY_ALLOWS_IDLE_EXIT) ||
            strlen(path) + 1 > capacity) return 0;
        memcpy(out, path, strlen(path) + 1);
        return 1;
    }
}

int glass_private_copy_requirement(uint8_t **out, size_t *length) {
    SecCodeRef own = NULL;
    SecStaticCodeRef code = NULL;
    SecRequirementRef requirement = NULL;
    CFDataRef bytes = NULL;
    OSStatus status = SecCodeCopySelf(kSecCSDefaultFlags, &own);
    if (!status) status = hardened_service(own, false);
    if (!status) status = SecCodeCopyStaticCode(own, kSecCSDefaultFlags, &code);
    if (!status) status = SecCodeCopyDesignatedRequirement(code, kSecCSDefaultFlags, &requirement);
    if (!status) status = SecRequirementCopyData(requirement, kSecCSDefaultFlags, &bytes);
    if (!status && !bytes) status = errSecParam;
    if (!status) {
        CFIndex count = CFDataGetLength(bytes);
        if (count <= 0 || count > 65536) status = errSecParam;
        else {
            *out = malloc((size_t)count);
            if (!*out) status = errSecAllocate;
            else { memcpy(*out, CFDataGetBytePtr(bytes), (size_t)count); *length = (size_t)count; }
        }
    }
    if (bytes) CFRelease(bytes);
    if (requirement) CFRelease(requirement);
    if (code) CFRelease(code);
    if (own) CFRelease(own);
    return status;
}

bool glass_private_peer_credentials_match(uint32_t expected_uid, uint32_t audit_uid,
                                         bool privileged, uint32_t effective, uint32_t audit) {
    // System-authorized root helpers can have no audit user; code authentication still follows.
    return effective == expected_uid && (audit == audit_uid ||
        (privileged && expected_uid == 0 && (audit == 0 || audit == AU_DEFAUDITID)));
}

static int authenticate_peer(int fd, const uint8_t *required, size_t length,
                            uint32_t expected_uid, uint32_t audit_uid, bool privileged,
                            uint32_t *pid, uint32_t *session) {
    @autoreleasepool {
        audit_token_t token = {0};
        socklen_t size = sizeof(token);
        if (getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, &token, &size) || size != sizeof(token))
            return errno ? errno : EIO;
        uint32_t audit = audit_token_to_auid(token);
        if (!glass_private_peer_credentials_match(expected_uid, audit_uid, privileged,
                                                 audit_token_to_euid(token), audit))
            return EACCES;
        SecCodeRef code = NULL;
        SecRequirementRef requirement = NULL;
        CFDataRef bytes = CFDataCreate(NULL, required, (CFIndex)length);
        NSDictionary *attributes = @{(__bridge id)kSecGuestAttributeAudit:
            [NSData dataWithBytes:&token length:sizeof(token)]};
        OSStatus status = SecCodeCopyGuestWithAttributes(NULL, (__bridge CFDictionaryRef)attributes,
                                                       kSecCSDefaultFlags, &code);
        if (!status) status = SecRequirementCreateWithData(bytes, kSecCSDefaultFlags, &requirement);
        if (!status) status = SecCodeCheckValidity(code, kSecCSStrictValidate, requirement);
        if (!status) status = hardened_service(code, false);
        if (!status) {
            pid_t process = audit_token_to_pid(token);
            au_asid_t asid = audit_token_to_asid(token);
            if (process <= 0 || asid <= 0) status = errSecParam;
            else { *pid = (uint32_t)process; *session = (uint32_t)asid; }
        }
        if (requirement) CFRelease(requirement);
        if (code) CFRelease(code);
        if (bytes) CFRelease(bytes);
        return status;
    }
}

int glass_private_authenticate_peer(int fd, const uint8_t *required, size_t length,
                                   uint32_t expected_uid, uint32_t *pid, uint32_t *session) {
    return authenticate_peer(fd, required, length, expected_uid, expected_uid, false, pid, session);
}

int glass_setup_authenticate_root(int fd, const uint8_t *required, size_t length,
                                 uint32_t owner, uint32_t *pid, uint32_t *session) {
    if (owner < 501) return EINVAL;
    return authenticate_peer(fd, required, length, 0, owner, true, pid, session);
}
