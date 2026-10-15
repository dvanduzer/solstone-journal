// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#import <Foundation/Foundation.h>
#include <CommonCrypto/CommonDigest.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

int32_t backup_s3_duplicate_owner_root(void);
int32_t sb_s3_owner_file_policy(int32_t fd, int32_t directory, int32_t apply);

@interface S3IntentObserver : NSObject
@property int directory;
@property int queue;
@property int operation;
@property BOOL requireWrite;
@property(nonatomic, copy) NSDictionary *destination;
@property(nonatomic, copy) NSString *previousID;
@property(nonatomic, copy) NSString *salt;
@property(nonatomic, copy) NSString *credentialCommitment;
@property(nonatomic, copy) NSString *passwordCommitment;
@end
@implementation S3IntentObserver
@end

static BOOL hexadecimal(id value, NSUInteger length) {
    if (![value isKindOfClass:NSString.class] || [value length] != length) return NO;
    return [value rangeOfCharacterFromSet:[[NSCharacterSet characterSetWithCharactersInString:@"0123456789abcdef"] invertedSet]].location == NSNotFound;
}

static BOOL identifier(id value) { return hexadecimal(value, 64); }
static BOOL intent_identifier(id value) { return hexadecimal(value, 32); }

static void wipe_bytes(void *pointer, size_t length) {
    volatile unsigned char *bytes = pointer;
    for (size_t i = 0; i < length; i++) bytes[i] = 0;
}

static BOOL fields(id value, NSArray *names) {
    return [value isKindOfClass:NSDictionary.class] && [value count] == names.count &&
        [[NSSet setWithArray:[value allKeys]] isEqual:[NSSet setWithArray:names]];
}

static BOOL operation_number(id value) {
    return [value isKindOfClass:NSNumber.class] && CFGetTypeID((__bridge CFTypeRef)value) != CFBooleanGetTypeID() &&
        !CFNumberIsFloatType((__bridge CFNumberRef)value) && [value integerValue] >= 1 && [value integerValue] <= 7;
}

static BOOL printable(id value, NSUInteger maximum, BOOL optional) {
    if (![value isKindOfClass:NSString.class] || [value length] > maximum || (!optional && ![value length])) return NO;
    for (NSUInteger i = 0; i < [value length]; i++) if ([value characterAtIndex:i] < 33 || [value characterAtIndex:i] > 126) return NO;
    return YES;
}

static BOOL password_shape(id value) {
    if (![value isKindOfClass:NSString.class] || [value length] == 0 || [value length] > 344 || [value length] % 4) return NO;
    NSUInteger padding = [value hasSuffix:@"=="] ? 2 : ([value hasSuffix:@"="] ? 1 : 0);
    if ([value length] / 4 * 3 - padding > 256) return NO;
    NSCharacterSet *alphabet = [NSCharacterSet characterSetWithCharactersInString:@"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"];
    for (NSUInteger i = 0; i < [value length] - padding; i++) if (![alphabet characterIsMember:[value characterAtIndex:i]]) return NO;
    return YES;
}

// Hash parsed strings directly through one owned scratch buffer. No second
// secret-bearing JSON serialization is made. Foundation parsed objects remain
// scoped to read_intent's autorelease pool; only our owned buffers are wiped.
static NSString *string_commitment(const char *domain, NSString *salt, NSArray *values) {
    unsigned char scratch[16384] = {0}, output[CC_SHA256_DIGEST_LENGTH];
    CC_SHA256_CTX context;
    CC_SHA256_Init(&context);
    CC_SHA256_Update(&context, domain, (CC_LONG)strlen(domain) + 1);
    BOOL valid = YES;
    NSArray *parts = [@[salt] arrayByAddingObjectsFromArray:values];
    uint64_t count = CFSwapInt64HostToBig(values.count);
    for (NSUInteger i = 0; i < parts.count; i++) {
        NSString *part = parts[i];
        NSUInteger used = 0; NSRange remaining = NSMakeRange(0, 0);
        if (![part isKindOfClass:NSString.class] || (part.length && (![part getBytes:scratch maxLength:sizeof(scratch) usedLength:&used
            encoding:NSUTF8StringEncoding options:0 range:NSMakeRange(0, part.length) remainingRange:&remaining] || remaining.length))) { valid = NO; break; }
        uint64_t length = CFSwapInt64HostToBig(used);
        CC_SHA256_Update(&context, &length, sizeof(length));
        CC_SHA256_Update(&context, scratch, (CC_LONG)used);
        if (i == 0) CC_SHA256_Update(&context, &count, sizeof(count));
        wipe_bytes(scratch, sizeof(scratch));
    }
    if (valid) CC_SHA256_Final(output, &context);
    wipe_bytes(scratch, sizeof(scratch)); wipe_bytes(&context, sizeof(context));
    if (!valid) return nil;
    NSMutableString *result = [NSMutableString stringWithCapacity:64];
    for (NSUInteger i = 0; i < sizeof(output); i++) [result appendFormat:@"%02x", output[i]];
    return result;
}

typedef struct { const unsigned char *bytes; size_t size, offset; uint32_t nodes; } JSONCensus;
static void json_space(JSONCensus *input) {
    while (input->offset < input->size && input->bytes[input->offset] && strchr(" \t\r\n", input->bytes[input->offset])) input->offset++;
}
static BOOL json_string(JSONCensus *input) {
    if (input->offset >= input->size || input->bytes[input->offset++] != '"') return NO;
    while (input->offset < input->size) {
        unsigned char byte = input->bytes[input->offset++];
        if (byte < 32) return NO;
        if (byte == '"') return YES;
        if (byte == '\\') {
            if (input->offset >= input->size) return NO;
            if (input->bytes[input->offset++] == 'u') {
                if (input->size - input->offset < 4) return NO;
                input->offset += 4;
            }
        }
    }
    return NO;
}
static BOOL unique_value(JSONCensus *input, uint32_t depth) {
    json_space(input);
    if (depth > 12 || ++input->nodes > 1024 || input->offset >= input->size) return NO;
    unsigned char kind = input->bytes[input->offset];
    if (kind == '"') return json_string(input);
    if (kind == '{' || kind == '[') {
        input->offset++; json_space(input);
        unsigned char end = kind == '{' ? '}' : ']';
        if (input->offset < input->size && input->bytes[input->offset] == end) { input->offset++; return YES; }
        NSMutableSet *keys = [NSMutableSet set];
        uint32_t entries = 0;
        while (input->offset < input->size) {
            if (++entries > 32) return NO;
            if (kind == '{') {
                size_t start = input->offset;
                if (!json_string(input)) return NO;
                NSData *literal = [NSData dataWithBytesNoCopy:(void *)(input->bytes + start) length:input->offset - start freeWhenDone:NO];
                id key = [NSJSONSerialization JSONObjectWithData:literal options:NSJSONReadingFragmentsAllowed error:NULL];
                if (![key isKindOfClass:NSString.class] || [keys containsObject:key]) return NO;
                [keys addObject:key]; json_space(input);
                if (input->offset >= input->size || input->bytes[input->offset++] != ':') return NO;
            }
            if (!unique_value(input, depth + 1)) return NO;
            json_space(input);
            if (input->offset >= input->size) return NO;
            unsigned char delimiter = input->bytes[input->offset++];
            if (delimiter == end) return YES;
            if (delimiter != ',') return NO;
            json_space(input);
        }
        return NO;
    }
    size_t start = input->offset;
    while (input->offset < input->size && !strchr(",]} \t\r\n", input->bytes[input->offset])) input->offset++;
    return input->offset > start;
}

static NSString *digest(NSData *data) {
    unsigned char bytes[CC_SHA256_DIGEST_LENGTH];
    CC_SHA256(data.bytes, (CC_LONG)data.length, bytes);
    NSMutableString *value = [NSMutableString stringWithCapacity:64];
    for (size_t i = 0; i < sizeof(bytes); i++) [value appendFormat:@"%02x", bytes[i]];
    return value;
}

// A failed or racing read produces no observation. The raw bounded state bytes
// are wiped; only closed, non-secret fields and commitments leave this helper.
static NSDictionary *read_intent(S3IntentObserver *observer) {
    @autoreleasepool {
    if (sb_s3_owner_file_policy(observer.directory, 1, 0)) return nil;
    int fd = openat(observer.directory, "state.json", O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) return nil;
    NSDictionary *result = nil;
    unsigned char *raw = calloc(1, 131073);
    struct stat before = {0}, after = {0}, named = {0};
    do {
        if (!raw || fstat(fd, &before) || !S_ISREG(before.st_mode) || before.st_uid != geteuid() ||
            (before.st_mode & 07777) != 0600 || before.st_nlink != 1 || before.st_size <= 0 || before.st_size > 131072 ||
            sb_s3_owner_file_policy(fd, 0, 0)) break;
        size_t length = 0;
        while (length < 131073) {
            ssize_t got = read(fd, raw + length, 131073 - length);
            if (got < 0 && errno == EINTR) continue;
            if (got < 0) { length = 131073; break; }
            if (!got) break;
            length += (size_t)got;
        }
        if (length != (size_t)before.st_size || length > 131072 || fstat(fd, &after) ||
            fstatat(observer.directory, "state.json", &named, AT_SYMLINK_NOFOLLOW) ||
            before.st_dev != after.st_dev || before.st_ino != after.st_ino || after.st_dev != named.st_dev || after.st_ino != named.st_ino ||
            before.st_size != after.st_size || before.st_mtimespec.tv_sec != after.st_mtimespec.tv_sec ||
            before.st_mtimespec.tv_nsec != after.st_mtimespec.tv_nsec || sb_s3_owner_file_policy(fd, 0, 0)) break;
        NSData *bytes = [NSData dataWithBytesNoCopy:raw length:length freeWhenDone:NO];
        JSONCensus census = {raw, length, 0, 0};
        if (!unique_value(&census, 0)) break;
        json_space(&census); if (census.offset != length) break;
        id record = [NSJSONSerialization JSONObjectWithData:bytes options:0 error:NULL];
        if (!fields(record, @[@"version", @"destination", @"credentials", @"passwords", @"active", @"complete"]) || ![record[@"version"] isKindOfClass:NSNumber.class] ||
            CFGetTypeID((__bridge CFTypeRef)record[@"version"]) == CFBooleanGetTypeID() || ![record[@"version"] isEqual:@1] ||
            CFNumberIsFloatType((__bridge CFNumberRef)record[@"version"]) ||
            ![record[@"destination"] isEqual:observer.destination] || ![record[@"credentials"] isKindOfClass:NSArray.class] ||
            [record[@"credentials"] count] == 0 || [record[@"credentials"] count] > 4 || ![record[@"passwords"] isKindOfClass:NSArray.class] ||
            [record[@"passwords"] count] == 0 || [record[@"passwords"] count] > 16) break;
        NSArray *credentials = record[@"credentials"], *passwords = record[@"passwords"];
        if ([NSSet setWithArray:credentials].count != credentials.count || [NSSet setWithArray:passwords].count != passwords.count) break;
        NSMutableArray *material = [NSMutableArray arrayWithObject:[NSString stringWithFormat:@"credentials:%lu", (unsigned long)credentials.count]];
        BOOL validMaterial = YES, currentCredential = NO, currentPassword = NO;
        for (id credential in credentials) {
            if (!fields(credential, @[@"access", @"secret", @"token"]) || !printable(credential[@"access"], 256, NO) ||
                !printable(credential[@"secret"], 1024, NO) || !printable(credential[@"token"], 16384, YES)) { validMaterial = NO; break; }
            NSArray *parts = @[credential[@"access"], credential[@"secret"], credential[@"token"]];
            NSString *commitment = string_commitment("ios-s3-credential-match-v1", observer.salt, parts);
            if (!commitment) { validMaterial = NO; break; }
            if ([commitment isEqual:observer.credentialCommitment]) currentCredential = YES;
            [material addObjectsFromArray:parts];
        }
        [material addObject:[NSString stringWithFormat:@"passwords:%lu", (unsigned long)passwords.count]];
        for (id password in passwords) {
            if (!password_shape(password)) { validMaterial = NO; break; }
            NSString *commitment = string_commitment("ios-s3-password-match-v1", observer.salt, @[password]);
            if (!commitment) { validMaterial = NO; break; }
            if ([commitment isEqual:observer.passwordCommitment]) currentPassword = YES;
            [material addObject:password];
        }
        if (!validMaterial || !currentCredential || !currentPassword) break;
        id active = record[@"active"];
        id complete = record[@"complete"];
        if ((active == NSNull.null) == (complete == NSNull.null)) break;
        NSDictionary *intent = @{};
        if (active != NSNull.null) {
            if (!fields(active, @[@"id", @"operation", @"may_have_written", @"locks"])) break;
            id locks = active[@"locks"] == NSNull.null ? @[] : active[@"locks"];
            if (!intent_identifier(active[@"id"]) ||
                !operation_number(active[@"operation"]) || ![active[@"may_have_written"] isKindOfClass:NSNumber.class] ||
                CFGetTypeID((__bridge CFTypeRef)active[@"may_have_written"]) != CFBooleanGetTypeID() ||
                ![locks isKindOfClass:NSArray.class] || [locks count] > 32 || ([locks count] && ![active[@"may_have_written"] boolValue]) ||
                [NSSet setWithArray:locks].count != [locks count]) break;
            BOOL locksValid = YES;
            for (id lock in locks) if (!identifier(lock)) locksValid = NO;
            if (!locksValid) break;
            intent = @{@"id":active[@"id"], @"operation":active[@"operation"], @"may_have_written":active[@"may_have_written"], @"locks":locks};
        }
        NSString *materialCommitment = string_commitment("ios-s3-retained-material-v1", observer.salt, material);
        if (!materialCommitment) break;
        NSString *previous = @"";
        if (complete != NSNull.null) {
            if (!fields(complete, @[@"id", @"operation"]) || !operation_number(complete[@"operation"])) break;
            previous = complete[@"id"]; if (!intent_identifier(previous)) break;
        }
        result = @{@"active":intent, @"previous_id":previous, @"record_sha256":digest(bytes), @"recovery_material_sha256":materialCommitment,
            @"current_credential_present":@YES, @"current_password_present":@YES};
        // Detach the closed non-secret result before wiping caller-owned input.
        NSData *closed = [NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingSortedKeys error:NULL];
        result = closed && closed.length < 8192 ? [NSJSONSerialization JSONObjectWithData:closed options:0 error:NULL] : nil;
    } while (0);
    if (raw) { wipe_bytes(raw, 131073); free(raw); }
    if (close(fd)) return nil;
    return result;
    }
}

static char *encode(id value) {
    NSData *bytes = value ? [NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingSortedKeys error:NULL] : nil;
    if (!bytes || bytes.length >= 8192) return NULL;
    char *result = calloc(1, bytes.length + 1);
    if (result) memcpy(result, bytes.bytes, bytes.length);
    return result;
}

static BOOL bind_observation(S3IntentObserver *observer, const char *input) {
    if (!input || strnlen(input, 8193) > 8192) return NO;
    id value = [NSJSONSerialization JSONObjectWithData:[NSData dataWithBytes:input length:strlen(input)] options:0 error:NULL];
    NSArray *destinationFields = @[@"endpoint", @"region", @"bucket", @"prefix"];
    if (!fields(value, [destinationFields arrayByAddingObjectsFromArray:@[@"nonce", @"credential_commitment", @"password_commitment"]])) return NO;
    for (NSString *key in destinationFields) if (![value[key] isKindOfClass:NSString.class] || ![value[key] length]) return NO;
    if (!intent_identifier(value[@"nonce"]) || !identifier(value[@"credential_commitment"]) || !identifier(value[@"password_commitment"])) return NO;
    observer.destination = [value dictionaryWithValuesForKeys:destinationFields];
    observer.salt = value[@"nonce"]; observer.credentialCommitment = value[@"credential_commitment"]; observer.passwordCommitment = value[@"password_commitment"];
    return YES;
}

void *backup_s3_observer_create(const char *destination, int32_t operation, int32_t requireWrite) {
    @autoreleasepool {
        if (!destination || strnlen(destination, 8193) > 8192 ||
            !((operation == 2 && requireWrite == 0) || (operation == 4 && requireWrite == 1))) return NULL;
        S3IntentObserver *observer = [S3IntentObserver new];
        if (!bind_observation(observer, destination)) return NULL;
        observer.directory = backup_s3_duplicate_owner_root(); observer.queue = -1;
        observer.operation = operation; observer.requireWrite = requireWrite != 0;
        if (observer.directory < 0) return NULL;
        observer.queue = kqueue();
        struct kevent event;
        EV_SET(&event, (uintptr_t)observer.directory, EVFILT_VNODE, EV_ADD | EV_CLEAR,
            NOTE_WRITE | NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE, 0, NULL);
        NSDictionary *initial = observer.queue >= 0 && kevent(observer.queue, &event, 1, NULL, 0, NULL) == 0 ? read_intent(observer) : nil;
        if (!initial || [initial[@"active"] count] != 0) {
            if (observer.queue >= 0) close(observer.queue);
            close(observer.directory); return NULL;
        }
        observer.previousID = initial[@"previous_id"];
        return (__bridge_retained void *)observer;
    }
}

char *backup_s3_observer_wait(void *pointer, uint32_t milliseconds) {
    @autoreleasepool {
        if (!pointer || milliseconds == 0 || milliseconds > 30000) return NULL;
        S3IntentObserver *observer = (__bridge S3IntentObserver *)pointer;
        uint64_t deadline = clock_gettime_nsec_np(CLOCK_MONOTONIC) + (uint64_t)milliseconds * 1000000;
        for (uint32_t notifications = 0; notifications < 512 && clock_gettime_nsec_np(CLOCK_MONOTONIC) < deadline; notifications++) {
            NSDictionary *record = read_intent(observer), *active = record[@"active"];
            if ([active[@"operation"] intValue] == observer.operation && intent_identifier(active[@"id"]) &&
                ![active[@"id"] isEqual:observer.previousID] && (!observer.requireWrite ||
                ([active[@"may_have_written"] isEqual:@YES] && [active[@"locks"] count] > 0))) {
                NSMutableDictionary *observed = [record mutableCopy];
                observed[@"armed_previous_id"] = observer.previousID;
                return encode(observed);
            }
            uint64_t now = clock_gettime_nsec_np(CLOCK_MONOTONIC);
            if (now >= deadline) break;
            uint64_t remaining = deadline - now;
            struct timespec timeout = {(time_t)(remaining / 1000000000), (long)(remaining % 1000000000)};
            struct kevent event;
            int count = kevent(observer.queue, NULL, 0, &event, 1, &timeout);
            if (count < 0 && errno == EINTR) continue;
            if (count <= 0 || event.flags & EV_ERROR || event.fflags & (NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE)) break;
        }
        return NULL;
    }
}

int32_t backup_s3_observer_free(void *pointer) {
    @autoreleasepool {
        if (!pointer) return 1;
        S3IntentObserver *observer = CFBridgingRelease(pointer);
        int queueResult = close(observer.queue), directoryResult = close(observer.directory);
        observer.queue = observer.directory = -1;
        return queueResult || directoryResult ? 1 : 0;
    }
}

char *backup_s3_read_owner(const char *destination) {
    @autoreleasepool {
        S3IntentObserver *observer = [S3IntentObserver new];
        if (!bind_observation(observer, destination)) return NULL;
        observer.directory = backup_s3_duplicate_owner_root();
        if (observer.directory < 0) return NULL;
        NSDictionary *result = read_intent(observer);
        if (close(observer.directory)) return NULL;
        return encode(result);
    }
}
