// Account mutations run only on a disposable hosted CI machine; no home or GUI login is created.
#include "../src/account_setup.m"
#include <bsm/libbsm.h>
#include <string.h>

@interface PasswordRecordDouble : NSObject
@property bool credentials;
@property bool rejectChange;
@property bool rejectVerification;
@property NSUInteger changes;
@property NSUInteger verifications;
@property(strong) NSError *failure;
@end

@implementation PasswordRecordDouble
- (NSDictionary *)recordDetailsForAttributes:(NSArray *)attributes error:(NSError **)error {
    return @{kODAttributeTypeAuthenticationAuthority:self.credentials ? @[@"test-only"] : @[]};
}
- (BOOL)changePassword:(NSString *)oldPassword toPassword:(NSString *)newPassword error:(NSError **)error {
    self.changes++;
    if (self.rejectChange && error) *error = self.failure;
    return !self.rejectChange;
}
- (BOOL)verifyPassword:(NSString *)password error:(NSError **)error {
    self.verifications++;
    if (self.rejectVerification && error) *error = self.failure;
    return !self.rejectVerification;
}
@end

static bool password_error_contract(void) {
    NSError *failure = [NSError errorWithDomain:NSPOSIXErrorDomain code:EACCES
        userInfo:@{NSLocalizedDescriptionKey:@"untrusted-description-must-not-be-reported"}];
    PasswordRecordDouble *record = [PasswordRecordDouble new];
    record.failure = failure; record.rejectChange = true;
    GlassAccountError diagnostic = {0};
    int status = prepare_password((ODRecord *)record,@"test-only",true,&diagnostic);
    if (status != 4 || diagnostic.domain != 2 || diagnostic.code != EACCES ||
        record.changes != 1 || record.verifications) return false;

    record = [PasswordRecordDouble new];
    record.credentials = true; record.rejectVerification = true; record.failure = failure;
    diagnostic = (GlassAccountError){0};
    status = prepare_password((ODRecord *)record,@"test-only",true,&diagnostic);
    if (status != 5 || diagnostic.domain != 2 || diagnostic.code != EACCES ||
        record.changes || record.verifications != 1) return false;

    record = [PasswordRecordDouble new];
    record.rejectVerification = true; record.failure = failure;
    diagnostic = (GlassAccountError){0};
    status = prepare_password((ODRecord *)record,@"test-only",true,&diagnostic);
    if (status != 8 || diagnostic.domain != 2 || diagnostic.code != EACCES ||
        record.changes != 1 || record.verifications != 1) return false;

    record = [PasswordRecordDouble new];
    diagnostic = (GlassAccountError){0};
    status = prepare_password((ODRecord *)record,@"test-only",false,&diagnostic);
    return status == 5 && !diagnostic.domain && !record.changes && !record.verifications;
}

static void erase_secret(void *buffer, size_t length) {
    volatile unsigned char *bytes = buffer;
    while (length--) *bytes++ = 0;
}

static NSDictionary *new_fixture(uint32_t minimum) {
    uint32_t uid = 0;
    if (glass_account_available_uid(minimum, &uid) || !glass_private_uid_empty(uid)) return nil;
    NSString *uuid = NSUUID.UUID.UUIDString;
    NSString *name = [@"glass_record_test_" stringByAppendingString:
        [[uuid stringByReplacingOccurrencesOfString:@"-" withString:@""] lowercaseString]];
    if (glass_account_name_unused(name.UTF8String)) return nil;
    NSString *home = [@"/Users/" stringByAppendingString:name];
    struct stat info;
    if (lstat(home.fileSystemRepresentation, &info) == 0 || errno != ENOENT) return nil;
    return @{@"name":name, @"uuid":uuid, @"uid":@(uid), @"home":home};
}

static NSDictionary *fixture_identity(NSDictionary *fixture) {
    return @{kODAttributeTypeGUID:fixture[@"uuid"],
        kODAttributeTypeUniqueID:[fixture[@"uid"] stringValue],
        kODAttributeTypePrimaryGroupID:@"20", kODAttributeTypeNFSHomeDirectory:fixture[@"home"]};
}

static bool remove_fixture(ODNode *node, NSDictionary *fixture) {
    NSArray *named = records(node,kODAttributeTypeRecordName,kODMatchEqualTo,fixture[@"name"]);
    NSArray *numbered = records(node,kODAttributeTypeUniqueID,kODMatchEqualTo,[fixture[@"uid"] stringValue]);
    if (!named || !numbered) return false;
    if (!named.count) return numbered.count == 0;
    if (named.count != 1 || numbered.count != 1 ||
        !identity_matches(named[0],fixture_identity(fixture)) ||
        !identity_matches(numbered[0],fixture_identity(fixture))) return false;
    ODRecord *record = named[0];
    if (![record.recordName isEqualToString:fixture[@"name"]] ||
        !glass_private_uid_empty([fixture[@"uid"] unsignedIntValue])) return false;
    struct stat info;
    if (lstat([fixture[@"home"] fileSystemRepresentation],&info) == 0 || errno != ENOENT) return false;
    if (![record deleteRecordAndReturnError:NULL]) return false;
    named = records(node,kODAttributeTypeRecordName,kODMatchEqualTo,fixture[@"name"]);
    numbered = records(node,kODAttributeTypeUniqueID,kODMatchEqualTo,[fixture[@"uid"] stringValue]);
    return named && numbered && !named.count && !numbered.count;
}

static bool attributes_contract(void) {
    NSDictionary *fixture = @{@"name":@"test_only",@"uuid":@"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        @"uid":@59000,@"home":@"/Users/test_only"};
    NSDictionary *attributes = creation_attributes(fixture_identity(fixture),fixture[@"name"]);
    if (attributes.count != 7) return false;
    for (id value in attributes.allValues) {
        if (![value isKindOfClass:NSArray.class] || [value count] != 1 ||
            ![value[0] isKindOfClass:NSString.class]) return false;
    }
    for (NSString *key in fixture_identity(fixture)) {
        if (![attributes[key][0] isEqualToString:fixture_identity(fixture)[key]]) return false;
    }
    return [attributes[kODAttributeTypePassword][0] isEqualToString:@"*"];
}

static bool hosted_disposable_machine(void) {
    const char *actions = getenv("GITHUB_ACTIONS"), *runner = getenv("RUNNER_ENVIRONMENT");
    return actions && runner && !strcmp(actions,"true") && !strcmp(runner,"github-hosted");
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc == 2 && !strcmp(argv[1],"--contract")) return attributes_contract() && password_error_contract() ? 0 : 1;
        if (argc != 2 || strcmp(argv[1],"--disposable-ci") || !hosted_disposable_machine() || getuid() || geteuid()) {
            fputs("Account lifecycle test requires explicit disposable hosted CI execution as root.\n",stderr);
            return 77;
        }
        if (!attributes_contract() || !password_error_contract()) return 1;
        ODNode *node = [ODNode nodeWithSession:[ODSession defaultSession] name:@"/Local/Default" error:NULL];
        if (!node) return 1;
        NSMutableArray *owned = [NSMutableArray array];
        NSMutableDictionary *result = [NSMutableDictionary dictionary];
        auditinfo_addr_t audit = {0};
        if (!getaudit_addr(&audit,sizeof(audit))) {
            result[@"audit_uid"] = @(audit.ai_auid);
            result[@"audit_session"] = @(audit.ai_asid);
        }
        bool passed = false, cleaned = true;
        char secret[37] = {0}, wrong[37] = {0};
        @try {
            NSDictionary *legacy = new_fixture(59000);
            if (!legacy) @throw [NSException exceptionWithName:@"FixtureUnavailable" reason:nil userInfo:nil];
            [owned addObject:legacy];
            NSMutableDictionary *old_attributes = [fixture_identity(legacy) mutableCopy];
            old_attributes[kODAttributeTypeFullName] = [NSString stringWithFormat:@"Glass development (%@)",legacy[@"name"]];
            old_attributes[kODAttributeTypeUserShell] = @"/bin/zsh";
            old_attributes[kODAttributeTypePassword] = @"*";
            NSError *error = nil;
            ODRecord *old_record = [node createRecordWithRecordType:kODRecordTypeUsers name:legacy[@"name"] attributes:old_attributes error:&error];
            GlassAccountError old_error = {0}; capture_error(&old_error,error);
            result[@"legacy_record_created"] = @(old_record != nil);
            result[@"legacy_error_domain"] = @(old_error.domain);
            result[@"legacy_error_code"] = @(old_error.code);
            if (!remove_fixture(node,legacy)) @throw [NSException exceptionWithName:@"LegacyCleanupFailed" reason:nil userInfo:nil];

            NSDictionary *fixture = new_fixture([legacy[@"uid"] unsignedIntValue]+1);
            if (!fixture) @throw [NSException exceptionWithName:@"FixtureUnavailable" reason:nil userInfo:nil];
            [owned addObject:fixture];
            const char *name = [fixture[@"name"] UTF8String], *uuid = [fixture[@"uuid"] UTF8String];
            uint32_t uid = [fixture[@"uid"] unsignedIntValue];
            GlassAccountError diagnostic = {0};
            fputs("{\"phase\":\"create_record\"}\n",stdout);fflush(stdout);
            int status = glass_account_prepare(name,uuid,uid,NULL,0,true,true,&diagnostic);
            result[@"record_status"] = @(status);
            result[@"record_error_domain"] = @(diagnostic.domain);
            result[@"record_error_code"] = @(diagnostic.code);
            if (status) @throw [NSException exceptionWithName:@"RecordPreparationFailed" reason:nil userInfo:nil];

            unsigned char random[16]; arc4random_buf(random,sizeof(random));
            memcpy(secret,"Aa1!",4);
            for (unsigned index=0;index<sizeof(random);index++) snprintf(secret+4+index*2,3,"%02x",random[index]);
            erase_secret(random,sizeof(random));
            fputs("{\"phase\":\"initialize_password\"}\n",stdout);fflush(stdout);
            status = glass_account_prepare(name,uuid,uid,(const uint8_t *)secret,36,false,true,&diagnostic);
            result[@"initial_password_status"] = @(status);
            result[@"initial_password_error_domain"] = @(diagnostic.domain);
            result[@"initial_password_error_code"] = @(diagnostic.code);
            if (status) @throw [NSException exceptionWithName:@"InitialPasswordFailed" reason:nil userInfo:nil];
            status = glass_account_prepare(name,uuid,uid,(const uint8_t *)secret,36,false,false,&diagnostic);
            result[@"resume_password_status"] = @(status);
            result[@"resume_password_error_domain"] = @(diagnostic.domain);
            result[@"resume_password_error_code"] = @(diagnostic.code);
            if (status) @throw [NSException exceptionWithName:@"ResumePasswordFailed" reason:nil userInfo:nil];
            memcpy(wrong,secret,sizeof(secret));wrong[35] = wrong[35]=='0' ? '1' : '0';
            status = glass_account_prepare(name,uuid,uid,(const uint8_t *)wrong,36,false,false,&diagnostic);
            result[@"wrong_password_status"] = @(status);
            result[@"wrong_password_error_domain"] = @(diagnostic.domain);
            result[@"wrong_password_error_code"] = @(diagnostic.code);
            if (status != 5) @throw [NSException exceptionWithName:@"WrongPasswordAccepted" reason:nil userInfo:nil];
            if (!diagnostic.domain) @throw [NSException exceptionWithName:@"WrongPasswordDiagnosticMissing" reason:nil userInfo:nil];
            status = glass_account_prepare(name,uuid,uid,(const uint8_t *)secret,36,false,false,&diagnostic);
            result[@"original_password_status"] = @(status);
            if (status) @throw [NSException exceptionWithName:@"OriginalPasswordChanged" reason:nil userInfo:nil];
            passed = true;
        } @catch (NSException *error) {
            result[@"failed_stage"] = error.name;
        } @finally {
            erase_secret(secret,sizeof(secret));erase_secret(wrong,sizeof(wrong));
            for (NSDictionary *fixture in [owned reverseObjectEnumerator]) {
                if (!remove_fixture(node,fixture)) cleaned = false;
            }
        }
        result[@"passed"] = @(passed && cleaned);
        result[@"cleanup_complete"] = @(cleaned);
        NSData *data = [NSJSONSerialization dataWithJSONObject:result options:0 error:NULL];
        if (data) { fwrite(data.bytes,1,data.length,stdout);fputc('\n',stdout); }
        return passed && cleaned && data ? 0 : 1;
    }
}
