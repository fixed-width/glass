/* macOS worker attachment, no event posting or account mutation. */
#import <Foundation/Foundation.h>
#import <OpenDirectory/OpenDirectory.h>
#include <ApplicationServices/ApplicationServices.h>
#include <Security/AuthSession.h>
#include <Security/Security.h>
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

static atomic_uint pinned_asid;
static atomic_uint pinned_uid;
static atomic_uint pinned_owner;

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
    if (!result && next != 1) result = EIO;
    acl_free(acl);
    return result;
}

static bool protected_directory(int fd) {
    struct stat s;
    return fd >= 0 && fstat(fd, &s) == 0 && S_ISDIR(s.st_mode) && s.st_uid == 0 &&
        !(s.st_mode & 0022) && glass_private_writes_protected(fd, 0) == 0;
}

static NSDictionary *enrollment(void) {
    int fd = open("/", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    const char *parents[] = {"Library", "Application Support", "Glass"};
    for (unsigned i = 0; i < 3; i++) {
        if (!protected_directory(fd)) { if (fd >= 0) close(fd); return nil; }
        int next = openat(fd, parents[i], O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        close(fd);
        fd = next;
    }
    if (!protected_directory(fd)) { if (fd >= 0) close(fd); return nil; }
    struct stat disabled;
    if (fstatat(fd, "disabled", &disabled, AT_SYMLINK_NOFOLLOW) == 0 || errno != ENOENT) {
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

static bool account_matches(bool worker) {
    NSDictionary *p = enrollment();
    NSSet *keys = [NSSet setWithArray:@[@"version", @"uid", @"gid", @"record_uuid", @"name", @"home", @"owner_uid"]];
    if (!p || ![[NSSet setWithArray:p.allKeys] isEqual:keys]) return false;
    uint32_t version, uid, gid, owner;
    if (!integer(p[@"version"], &version) || version != 1 || !integer(p[@"uid"], &uid) ||
        !integer(p[@"gid"], &gid) || !integer(p[@"owner_uid"], &owner) || owner == uid || getuid() != geteuid() ||
        (worker ? (uid != getuid() || gid != getgid() || getgid() != getegid()) : owner != getuid())) return false;
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
        NSDictionary *p = enrollment();
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
    puts("{\"completed\":true,\"context_refusal_cases\":11,\"integer_type_cases\":7,\"ssh_attachment_refused\":true,\"gui_started\":false}");
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
    @autoreleasepool { return account_matches(true) ? 1 : 0; }
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

static OSStatus hardened_service(SecCodeRef code) {
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
        !(flags & kSecCodeSignatureRuntime) || !(dynamic & kSecCodeStatusValid) ||
        (dynamic & kSecCodeStatusDebugged)) status = errSecParam;
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

int glass_private_copy_requirement(uint8_t **out, size_t *length) {
    SecCodeRef own = NULL;
    SecStaticCodeRef code = NULL;
    SecRequirementRef requirement = NULL;
    CFDataRef bytes = NULL;
    OSStatus status = SecCodeCopySelf(kSecCSDefaultFlags, &own);
    if (!status) status = hardened_service(own);
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

int glass_private_authenticate_peer(int fd, const uint8_t *required, size_t length,
                                   uint32_t expected_uid, uint32_t *pid, uint32_t *session) {
    @autoreleasepool {
        audit_token_t token = {0};
        socklen_t size = sizeof(token);
        if (getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, &token, &size) || size != sizeof(token))
            return errno ? errno : EIO;
        if (audit_token_to_euid(token) != expected_uid || audit_token_to_auid(token) != expected_uid)
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
        if (!status) status = hardened_service(code);
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
