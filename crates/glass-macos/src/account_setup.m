#import <Foundation/Foundation.h>
#import <OpenDirectory/OpenDirectory.h>
#include <pwd.h>
#include <sys/stat.h>
#include <errno.h>
#include <unistd.h>

static NSArray *records(ODNode *node, NSString *attribute, ODMatchType match, id value) {
    ODQuery *query = [ODQuery queryWithNode:node forRecordTypes:kODRecordTypeUsers
        attribute:attribute matchType:match queryValues:value
        returnAttributes:@[kODAttributeTypeUniqueID] maximumResults:65536 error:NULL];
    return query ? [query resultsAllowingPartial:NO error:NULL] : nil;
}

int glass_account_name_unused(const char *bytes) {
    @autoreleasepool {
        if (!bytes || !bytes[0]) return 1;
        ODNode *node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:NULL];
        NSArray *named = node ? records(node,kODAttributeTypeRecordName,kODMatchEqualTo,@(bytes)) : nil;
        if (!named) return 3;
        if (named.count) return 2;
        errno = 0;
        if (getpwnam(bytes)) return 2;
        return errno ? 3 : 0;
    }
}

int glass_account_available_uid(uint32_t minimum, uint32_t *out) {
    @autoreleasepool {
        if (minimum < 501 || minimum >= 60000 || !out) return 1;
        ODNode *node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:NULL];
        NSArray *users = node ? records(node, kODAttributeTypeUniqueID, kODMatchGreaterThan, @"500") : nil;
        if (!users || users.count >= 65536) return 3;
        NSMutableSet *used = [NSMutableSet set];
        for (ODRecord *record in users) {
            NSArray *values = [record valuesForAttribute:kODAttributeTypeUniqueID error:NULL];
            if (values.count != 1 || ![values[0] isKindOfClass:NSString.class]) return 3;
            [used addObject:values[0]];
        }
        for (uint32_t uid = minimum; uid < 60000; uid++) {
            if ([used containsObject:[NSString stringWithFormat:@"%u", uid]]) continue;
            errno = 0;
            struct passwd *existing = getpwuid(uid);
            if (!existing && errno) return 3;
            if (!existing) {
                *out = uid;
                return 0;
            }
        }
        return 2;
    }
}

static bool identity_matches(ODRecord *record, NSDictionary *expected) {
    for (NSString *key in expected) {
        NSArray *values = [record valuesForAttribute:key error:NULL];
        if (values.count != 1 || ![values[0] isKindOfClass:NSString.class]) return false;
        if ([key isEqualToString:kODAttributeTypeGUID]) {
            if ([values[0] caseInsensitiveCompare:expected[key]] != NSOrderedSame) return false;
        } else if (![values[0] isEqualToString:expected[key]]) return false;
    }
    return true;
}

static NSDictionary *creation_attributes(NSDictionary *identity, NSString *name) {
    NSMutableDictionary *attributes = [NSMutableDictionary dictionaryWithCapacity:7];
    for (NSString *key in identity) attributes[key] = @[identity[key]];
    attributes[kODAttributeTypeFullName] = @[[NSString stringWithFormat:@"Glass development (%@)",name]];
    attributes[kODAttributeTypeUserShell] = @[@"/bin/zsh"];
    attributes[kODAttributeTypePassword] = @[@"*"];
    return attributes;
}

typedef struct { uint32_t domain; int64_t code; } GlassAccountError;

static void capture_error(GlassAccountError *output, NSError *error) {
    if (!output || !error) return;
    if ([error.domain isEqualToString:@"com.apple.OpenDirectory"]) output->domain = 1;
    else if ([error.domain isEqualToString:NSPOSIXErrorDomain]) output->domain = 2;
    else if ([error.domain isEqualToString:NSCocoaErrorDomain]) output->domain = 3;
    else if ([error.domain isEqualToString:NSOSStatusErrorDomain]) output->domain = 4;
    else output->domain = 5;
    output->code = error.code;
}

static int prepare_password(ODRecord *record, NSString *password, bool initial, GlassAccountError *diagnostic) {
    NSError *error = nil;
    NSDictionary *details = [record recordDetailsForAttributes:@[kODAttributeTypeAuthenticationAuthority] error:&error];
    if (!details) { capture_error(diagnostic,error); return 3; }
    NSArray *authority = details[kODAttributeTypeAuthenticationAuthority];
    if (authority && ![authority isKindOfClass:NSArray.class]) return 3;
    error = nil;
    if (authority.count) {
        // A resumed account already has credentials: verify, never reset them.
        if (![record verifyPassword:password error:&error]) { capture_error(diagnostic,error); return 5; }
    } else {
        if (!initial) return 5;
        if (![record changePassword:nil toPassword:password error:&error]) { capture_error(diagnostic,error); return 4; }
        error = nil;
        if (![record verifyPassword:password error:&error]) { capture_error(diagnostic,error); return 8; }
    }
    return 0;
}

extern int glass_private_uid_empty(uint32_t uid);

static bool withdraw_new_record(ODRecord *record, NSDictionary *identity, NSString *name) {
    if (!identity_matches(record,identity) || ![record.recordName isEqualToString:name]) return false;
    NSDictionary *details = [record recordDetailsForAttributes:@[kODAttributeTypeAuthenticationAuthority] error:NULL];
    if (!details) return false;
    id authority = details[kODAttributeTypeAuthenticationAuthority];
    if (authority && (![authority isKindOfClass:NSArray.class] || [authority count])) return false;
    struct stat info;
    if (lstat([identity[kODAttributeTypeNFSHomeDirectory] fileSystemRepresentation],&info) == 0 || errno != ENOENT) return false;
    return [record deleteRecordAndReturnError:NULL];
}

// Status codes deliberately exclude OpenDirectory's potentially sensitive error descriptions.
int glass_account_prepare(const char *name_bytes, const char *uuid_bytes, uint32_t uid,
                          const uint8_t *password_bytes, size_t password_length, bool record_only, bool initial,
                          GlassAccountError *diagnostic) {
    @autoreleasepool {
        if (diagnostic) *diagnostic = (GlassAccountError){0};
        if (getuid() || geteuid() || !name_bytes || !uuid_bytes || uid < 501 || uid >= 60000 ||
            (!record_only && (!password_bytes || password_length == 0 || password_length > 1024))) return 1;
        NSString *name = @(name_bytes), *uuid = @(uuid_bytes);
        NSCharacterSet *allowed = [NSCharacterSet characterSetWithCharactersInString:
            @"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_"];
        if (!name.length || name.length > 64 || [name rangeOfCharacterFromSet:allowed.invertedSet].location != NSNotFound ||
            ![[NSUUID alloc] initWithUUIDString:uuid]) return 1;
        ODNode *node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:NULL];
        if (!node) return 3;
        NSArray *named = records(node, kODAttributeTypeRecordName, kODMatchEqualTo, name);
        NSArray *numbered = records(node, kODAttributeTypeUniqueID, kODMatchEqualTo,
                                    [NSString stringWithFormat:@"%u", uid]);
        if (!named || !numbered) return 3;
        if (named.count > 1 || numbered.count > 1) return 2;
        NSDictionary *identity = @{kODAttributeTypeGUID:uuid, kODAttributeTypeUniqueID:
            [NSString stringWithFormat:@"%u", uid], kODAttributeTypePrimaryGroupID:@"20",
            kODAttributeTypeNFSHomeDirectory:[@"/Users/" stringByAppendingString:name]};
        ODRecord *record = named.firstObject;
        bool created_here = false;
        if (!record) {
            if (!record_only || !initial) return 2;
            if (!glass_private_uid_empty(uid)) return 12;
            if (numbered.count) return 2;
            errno = 0;
            if (getpwnam(name_bytes)) return 2;
            if (errno) return 3;
            errno = 0;
            if (getpwuid(uid)) return 2;
            if (errno) return 3;
            NSError *error = nil;
            record = [node createRecordWithRecordType:kODRecordTypeUsers name:name
                attributes:creation_attributes(identity,name) error:&error];
            if (!record) { capture_error(diagnostic,error); return 3; }
            created_here = true;
        }
        if (!identity_matches(record, identity)) return 2;
        NSArray *owners = records(node, kODAttributeTypeUniqueID, kODMatchEqualTo,
                                 [NSString stringWithFormat:@"%u", uid]);
        if (owners.count != 1 || !identity_matches(owners[0], identity) ||
            (created_here && !glass_private_uid_empty(uid))) {
            if (created_here && !withdraw_new_record(record,identity,name)) return 10;
            return 2;
        }
        ODRecord *admin = [node recordWithRecordType:kODRecordTypeGroups name:@"admin" attributes:nil error:NULL];
        NSError *error = nil;
        if (!admin || [admin isMemberRecord:record error:&error] || error) return 7;
        if (record_only) return 0;
        // OD may retain its NSString argument; it must not borrow the caller's temporary bytes.
        NSString *password = [[NSString alloc] initWithBytes:password_bytes
            length:password_length encoding:NSUTF8StringEncoding];
        if (!password) return 1;
        int password_status = prepare_password(record,password,initial,diagnostic);
        if (password_status) return password_status;
        owners = records(node,kODAttributeTypeUniqueID,kODMatchEqualTo,[NSString stringWithFormat:@"%u",uid]);
        if (owners.count != 1 || !identity_matches(owners[0],identity)) return 11;
        return identity_matches(record, identity) ? 0 : 2;
    }
}

int glass_account_verify_identity(const char *name_bytes, const char *uuid_bytes, uint32_t uid) {
    @autoreleasepool {
        if (!name_bytes || !uuid_bytes || uid < 501) return 1;
        ODNode *node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:NULL];
        NSArray *users = node ? records(node,kODAttributeTypeUniqueID,kODMatchEqualTo,
                                        [NSString stringWithFormat:@"%u",uid]) : nil;
        if (!users) return 3;
        if (users.count != 1) return 2;
        ODRecord *record = users[0];
        NSArray *values = [record valuesForAttribute:kODAttributeTypeGUID error:NULL];
        return [record.recordName isEqualToString:@(name_bytes)] && values.count == 1 &&
            [values[0] isKindOfClass:NSString.class] &&
            [values[0] caseInsensitiveCompare:@(uuid_bytes)] == NSOrderedSame ? 0 : 2;
    }
}
