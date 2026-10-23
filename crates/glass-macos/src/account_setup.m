#import <Foundation/Foundation.h>
#import <OpenDirectory/OpenDirectory.h>
#include <pwd.h>
#include <sys/stat.h>
#include <errno.h>
#include <unistd.h>
#include <membership.h>
#include <uuid/uuid.h>

static NSArray *records(ODNode *node, NSString *attribute, ODMatchType match, id value) {
    ODQuery *query = [ODQuery queryWithNode:node forRecordTypes:kODRecordTypeUsers
        attribute:attribute matchType:match queryValues:value
        returnAttributes:@[kODAttributeTypeUniqueID] maximumResults:65536 error:NULL];
    return query ? [query resultsAllowingPartial:NO error:NULL] : nil;
}

typedef struct {
    uint32_t stage, domain;
    int64_t code;
    int32_t mutation_acknowledged;
} GlassDesktopAccessDiagnostic;

static void desktop_error(GlassDesktopAccessDiagnostic *out, NSError *error) {
    if (!out || !error) return;
    if ([error.domain isEqualToString:@"com.apple.OpenDirectory"]) out->domain = 1;
    else if ([error.domain isEqualToString:NSPOSIXErrorDomain]) out->domain = 2;
    else if ([error.domain isEqualToString:NSCocoaErrorDomain]) out->domain = 3;
    else if ([error.domain isEqualToString:NSOSStatusErrorDomain]) out->domain = 4;
    else out->domain = 5;
    out->code = error.code;
}

static ODRecord *desktop_group(ODNode *node, char uuid[37], NSError **error) {
    NSError *observed = nil;
    if (error) *error = nil;
    ODQuery *query = [ODQuery queryWithNode:node forRecordTypes:kODRecordTypeGroups
        attribute:kODAttributeTypeRecordName matchType:kODMatchEqualTo
        queryValues:@"com.apple.access_screensharing" returnAttributes:@[kODAttributeTypeGUID,
        kODAttributeTypeGroupMembers,kODAttributeTypeGroupMembership] maximumResults:2 error:&observed];
    if (!query) {
        if (error) *error = observed ?: [NSError errorWithDomain:NSPOSIXErrorDomain code:EPROTO userInfo:nil];
        return nil;
    }
    observed = nil;
    NSArray *records = [query resultsAllowingPartial:NO error:&observed];
    if (!records || records.count > 1) {
        if (error) *error = !records && observed ? observed :
            [NSError errorWithDomain:NSPOSIXErrorDomain code:EPROTO userInfo:nil];
        return nil;
    }
    if (!records.count) return nil;
    ODRecord *group = records[0];
    observed = nil;
    NSArray *values = [group valuesForAttribute:kODAttributeTypeGUID error:&observed];
    uuid_t identity;
    if (values.count != 1 || ![values[0] isKindOfClass:NSString.class] ||
        uuid_parse([values[0] UTF8String], identity)) {
        if (error) *error = !values && observed ? observed :
            [NSError errorWithDomain:NSPOSIXErrorDomain code:EPROTO userInfo:nil];
        return nil;
    }
    uuid_unparse_lower(identity, uuid);
    return group;
}

static NSArray *desktop_members(ODRecord *group, NSString *attribute, NSError **error) {
    NSError *observed = nil;
    NSArray *values = [group valuesForAttribute:attribute error:&observed];
    if (!values) {
        if ([observed.domain isEqualToString:@"com.apple.OpenDirectory"] &&
            observed.code == kODErrorRecordAttributeNotFound) return @[];
        if (error) *error = observed ?: [NSError errorWithDomain:NSPOSIXErrorDomain code:EPROTO userInfo:nil];
        return nil;
    }
    if (![values isKindOfClass:NSArray.class]) {
        if (error) *error = [NSError errorWithDomain:NSPOSIXErrorDomain code:EPROTO userInfo:nil];
        return nil;
    }
    return values;
}

static int desktop_direct_member(ODRecord *group, const char *name, const char *uuid,
                                 int *member, NSError **error) {
    NSArray *ids = desktop_members(group,kODAttributeTypeGroupMembers,error);
    if (error && *error) return EIO;
    NSArray *names = desktop_members(group,kODAttributeTypeGroupMembership,error);
    if (error && *error) return EIO;
    bool found = false;
    for (id value in ids) {
        uuid_t parsed;
        if (![value isKindOfClass:NSString.class] || uuid_parse([value UTF8String],parsed)) return EPROTO;
        if ([value caseInsensitiveCompare:@(uuid)] == NSOrderedSame) found = true;
    }
    for (id value in names) {
        if (![value isKindOfClass:NSString.class]) return EPROTO;
        if ([value caseInsensitiveCompare:@(name)] == NSOrderedSame) found = true;
    }
    *member = found ? 1 : 0;
    return 0;
}

static int desktop_authorized(const uuid_t user, int *authorized) {
    int allowed = 0;
    int status = mbr_check_service_membership(user, "screensharing", &allowed);
    if (status == ENOENT) allowed = 1;
    else if (status) return status;
    *authorized = allowed ? 1 : 0;
    return 0;
}

static int desktop_mutate(ODRecord *group, ODRecord *user, int action,
                           GlassDesktopAccessDiagnostic *diagnostic) {
    NSError *error = nil;
    BOOL changed = action == 1 ? [group addMemberRecord:user error:&error] :
                                [group removeMemberRecord:user error:&error];
    if (!changed) { desktop_error(diagnostic,error); return EIO; }
    diagnostic->mutation_acknowledged = 1;
    return 0;
}

// Mutations are limited to one enrolled user and the existing Screen Sharing group.
int glass_desktop_access(const char *name, const char *expected_user, uint32_t uid, int action,
                         const char *expected_group, char group_uuid[37], int *member, int *authorized,
                         GlassDesktopAccessDiagnostic *diagnostic) {
    @autoreleasepool {
        if (!diagnostic) return EINVAL;
        *diagnostic = (GlassDesktopAccessDiagnostic){0};
        diagnostic->stage = 1;
        if (!name || !expected_user || !group_uuid || !member || !authorized || uid < 501 || action < 0 || action > 2) return EINVAL;
        if (action && (getuid() != 0 || geteuid() != 0)) return EPERM;
        uuid_t user_id, expected;
        if (mbr_uid_to_uuid(uid, user_id)) return EIO;
        if (uuid_parse(expected_user, expected) || memcmp(expected, user_id, sizeof(uuid_t))) return EACCES;
        NSError *error = nil;
        ODNode *node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:&error];
        if (!node) { desktop_error(diagnostic,error); return EIO; }
        error = nil;
        ODRecord *user = [node recordWithRecordType:kODRecordTypeUsers name:@(name)
            attributes:@[kODAttributeTypeUniqueID,kODAttributeTypeGUID] error:&error];
        if (!user) { desktop_error(diagnostic,error); return EIO; }
        error = nil;
        NSArray *ids = [user valuesForAttribute:kODAttributeTypeUniqueID error:&error];
        if (!ids) { desktop_error(diagnostic,error); return EIO; }
        error = nil;
        NSArray *uuids = [user valuesForAttribute:kODAttributeTypeGUID error:&error];
        if (!uuids) { desktop_error(diagnostic,error); return EIO; }
        if (ids.count != 1 || uuids.count != 1 || ![ids[0] isKindOfClass:NSString.class] ||
            ![uuids[0] isKindOfClass:NSString.class] ||
            ![ids[0] isEqualToString:[NSString stringWithFormat:@"%u",uid]] ||
            [uuids[0] caseInsensitiveCompare:@(expected_user)] != NSOrderedSame) return EACCES;
        diagnostic->stage = 2;
        int status = desktop_authorized(user_id,authorized);
        if (status) return status;
        diagnostic->stage = 3;
        group_uuid[0] = 0;
        error = nil;
        ODRecord *group = desktop_group(node,group_uuid,&error);
        if (!group) {
            desktop_error(diagnostic,error);
            *member = error ? -1 : 0;
            return error ? EIO : (action || !*authorized ? ENOENT : 0);
        }
        diagnostic->stage = 4;
        status = desktop_direct_member(group,name,expected_user,member,&error);
        if (status) { desktop_error(diagnostic,error); return status; }
        if (!action) return 0;
        uuid_t selected, actual;
        if (!expected_group || uuid_parse(expected_group,selected) || uuid_parse(group_uuid,actual) ||
            memcmp(selected,actual,sizeof(uuid_t))) return EACCES;
        // A pre-existing direct grant is not a mutation this operation owns.
        if (action == 1 && *member) return EEXIST;
        if (action == 2 && !*member) return 0;
        diagnostic->stage = 5;
        *member = -1;
        status = desktop_mutate(group,user,action,diagnostic);
        if (status) return status;
        diagnostic->stage = 6;
        for (int attempt = 0; attempt < 101; attempt++) {
            *member = -1;
            error = nil;
            node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:&error];
            char refreshed[37] = {0};
            if (!node) { desktop_error(diagnostic,error); return EIO; }
            group = desktop_group(node,refreshed,&error);
            if (!group) { desktop_error(diagnostic,error); return EIO; }
            if (strcmp(group_uuid,refreshed)) return EACCES;
            status = desktop_direct_member(group,name,expected_user,member,&error);
            if (status) { desktop_error(diagnostic,error); return status; }
            if (*member == (action == 1 ? 1 : 0)) break;
            if (attempt == 100) return ETIMEDOUT;
            usleep(50000);
        }
        diagnostic->stage = 7;
        for (int attempt = 0; attempt < 101; attempt++) {
            status = desktop_authorized(user_id,authorized);
            if (status) return status;
            if (action != 1 || *authorized) return 0;
            if (attempt < 100) usleep(50000);
        }
        return ETIMEDOUT;
    }
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

#ifdef GLASS_DESKTOP_ACCESS_TEST
@interface GlassMembershipRecord : NSObject
@property(nonatomic,copy) NSArray *ids;
@property(nonatomic,copy) NSArray *names;
@property(nonatomic) BOOL readFails;
@property(nonatomic) BOOL idsAbsent;
@property(nonatomic) BOOL namesAbsent;
@property(nonatomic) BOOL readWarning;
@property(nonatomic) BOOL writeSucceeds;
@property(nonatomic) BOOL writeError;
@property(nonatomic) unsigned cachedMembershipReads;
@end
@implementation GlassMembershipRecord
- (NSArray *)valuesForAttribute:(NSString *)attribute error:(NSError **)error {
    if (self.readFails) { *error=[NSError errorWithDomain:NSPOSIXErrorDomain code:EIO userInfo:nil];return nil; }
    if ((self.idsAbsent && [attribute isEqualToString:kODAttributeTypeGroupMembers]) ||
        (self.namesAbsent && [attribute isEqualToString:kODAttributeTypeGroupMembership])) {
        *error=[NSError errorWithDomain:@"com.apple.OpenDirectory" code:kODErrorRecordAttributeNotFound userInfo:nil];
        return nil;
    }
    if (self.readWarning) *error=[NSError errorWithDomain:NSPOSIXErrorDomain code:EIO userInfo:nil];
    return [attribute isEqualToString:kODAttributeTypeGroupMembers] ? self.ids:self.names;
}
- (BOOL)isMemberRecord:(ODRecord *)record error:(NSError **)error {
    (void)record;(void)error;self.cachedMembershipReads++;return NO;
}
- (BOOL)addMemberRecord:(ODRecord *)record error:(NSError **)error {
    (void)record;
    if(self.writeError)*error=[NSError errorWithDomain:NSPOSIXErrorDomain code:EPERM userInfo:nil];
    return self.writeSucceeds;
}
- (BOOL)removeMemberRecord:(ODRecord *)record error:(NSError **)error {
    return [self addMemberRecord:record error:error];
}
@end

int main(void) {
    @autoreleasepool {
        const char *uuid="00112233-4455-6677-8899-aabbccddeeff";
        GlassMembershipRecord *record=[GlassMembershipRecord new];
        record.ids=@[@"00112233-4455-6677-8899-AABBCCDDEEFF"];record.names=@[];
        NSError *error=nil;int member=-1;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)||member!=1||record.cachedMembershipReads)return 1;
        record.ids=@[];record.names=@[@"development"];member=-1;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)||member!=1)return 2;
        record.names=@[];member=-1;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)||member!=0)return 3;
        record.ids=@[@1];member=-1;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)!=EPROTO||member!=-1)return 4;
        record.ids=@[];record.readFails=YES;member=-1;error=nil;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)!=EIO||member!=-1||!error)return 5;
        record.readFails=NO;record.writeSucceeds=YES;
        GlassDesktopAccessDiagnostic diagnostic={0};
        if(desktop_mutate((id)record,nil,1,&diagnostic)||diagnostic.mutation_acknowledged!=1)return 6;
        record.readFails=YES;member=-1;error=nil;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)!=EIO||diagnostic.mutation_acknowledged!=1)return 7;
        record.writeSucceeds=NO;record.writeError=YES;diagnostic=(GlassDesktopAccessDiagnostic){0};
        if(desktop_mutate((id)record,nil,1,&diagnostic)!=EIO||diagnostic.mutation_acknowledged||diagnostic.domain!=2||diagnostic.code!=EPERM)return 8;
        record.writeSucceeds=YES;diagnostic=(GlassDesktopAccessDiagnostic){0};
        if(desktop_mutate((id)record,nil,1,&diagnostic)||diagnostic.mutation_acknowledged!=1)return 9;
        if(sizeof(GlassDesktopAccessDiagnostic)!=24 || offsetof(GlassDesktopAccessDiagnostic,mutation_acknowledged)!=16)return 10;
        record.readFails=NO;record.idsAbsent=YES;record.namesAbsent=YES;member=-1;error=nil;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)||member!=0||error)return 11;
        record.idsAbsent=NO;record.ids=@[@(uuid)];member=-1;error=nil;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)||member!=1)return 12;
        record.idsAbsent=YES;record.namesAbsent=NO;record.names=@[@"development"];member=-1;error=nil;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)||member!=1)return 13;
        record.idsAbsent=NO;record.ids=@[@(uuid)];record.names=@[];record.readWarning=YES;member=-1;error=nil;
        if(desktop_direct_member((id)record,"development",uuid,&member,&error)||member!=1||error)return 14;
        record.writeSucceeds=NO;record.writeError=NO;diagnostic=(GlassDesktopAccessDiagnostic){0};
        if(desktop_mutate((id)record,nil,1,&diagnostic)!=EIO||diagnostic.mutation_acknowledged||diagnostic.domain||diagnostic.code)return 15;
        puts("{\"direct_membership_cases\":9,\"mutation_outcome_cases\":5,\"ffi_layout_cases\":1,\"directory_mutated\":false}");
    }
    return 0;
}
#endif
