// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#import <Foundation/Foundation.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include "librestic_bridge.h"

// The fixed production implementation is linked from the one private archive.
int32_t sb_s3_owner_file_policy(int32_t fd, int32_t directory, int32_t apply);

static int make_directory(NSString *name) {
    if (mkdir(name.fileSystemRepresentation, 0700)) return -1;
    return open(name.fileSystemRepresentation, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
}

static BOOL excluded(NSString *name, BOOL value) {
    return [[NSURL fileURLWithPath:name] setResourceValue:@(value) forKey:NSURLIsExcludedFromBackupKey error:NULL];
}

static BOOL protection(NSString *name, NSString *value) {
    return [[NSFileManager defaultManager] setAttributes:@{NSFileProtectionKey:value} ofItemAtPath:name error:NULL];
}

static NSDictionary *go_probe(NSString *name, int32_t *code) {
    char output[16384] = {0};
    const char *bytes = name.fileSystemRepresentation;
    *code = restic_bridge_probe_s3_owner_policy((char *)bytes, (uint32_t)strlen(bytes), output, sizeof(output));
    size_t length = strnlen(output, sizeof(output));
    if (!length || length == sizeof(output)) return nil;
    id parsed = [NSJSONSerialization JSONObjectWithData:[NSData dataWithBytes:output length:length] options:0 error:NULL];
    return [parsed isKindOfClass:NSDictionary.class] ? parsed : nil;
}

static NSDictionary *primitive_control(int baseFD, NSString *base, NSString *kind) {
    NSString *component = [kind stringByAppendingString:@".tmp"];
    NSString *name = [base stringByAppendingPathComponent:component];
    int fd = openat(baseFD, component.fileSystemRepresentation, O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
    if (fd < 0) return @{@"kind":kind, @"passed":@NO};
    // Each negative gets a fresh empty inode. No payload is written here.
    BOOL prepared, changed;
    if ([kind isEqualToString:@"missing-exclusion"]) {
        prepared = protection(name, NSFileProtectionComplete);
        changed = excluded(name, NO);
    } else {
        prepared = sb_s3_owner_file_policy(fd, 0, 1) == 0 && sb_s3_owner_file_policy(fd, 0, 0) == 0;
        changed = [kind isEqualToString:@"wrong-protection"] ? protection(name, NSFileProtectionNone) : excluded(name, NO);
    }
    BOOL refused = prepared && changed && sb_s3_owner_file_policy(fd, 0, 0) != 0;
    struct stat before = {0}, after = {0};
    BOOL emptyBefore = fstat(fd, &before) == 0 && before.st_size == 0;
    BOOL corrected = sb_s3_owner_file_policy(fd, 0, 1) == 0 && sb_s3_owner_file_policy(fd, 0, 0) == 0;
    BOOL emptyAfter = emptyBefore && fstat(fd, &after) == 0 && after.st_size == 0 && before.st_dev == after.st_dev && before.st_ino == after.st_ino;
    BOOL closed = close(fd) == 0;
    BOOL passed = refused && emptyBefore && corrected && emptyAfter && closed;
    return @{@"kind":kind, @"bad_policy_refused":@(refused), @"corrected_policy_passed":@(corrected),
        @"empty_before_and_after":@((BOOL)(emptyBefore && emptyAfter)), @"closed":@(closed), @"passed":@(passed)};
}

char *backup_owner_policy_run(const char *root) {
    @autoreleasepool {
        NSMutableDictionary *result = [@{@"passed":@NO, @"reason":@"POLICY_CONTROL_FAILED"} mutableCopy];
        do {
        NSString *base = root ? [[NSFileManager defaultManager] stringWithFileSystemRepresentation:root length:strlen(root)] : nil;
        int baseFD = base ? make_directory(base) : -1;
        if (baseFD < 0) break;
        NSMutableArray *primitives = [NSMutableArray array];
        BOOL allPrimitives = YES;
        for (NSString *kind in @[@"missing-exclusion", @"wrong-protection", @"wrong-exclusion"]) {
            NSDictionary *control = primitive_control(baseFD, base, kind);
            [primitives addObject:control];
            if (![control[@"passed"] isEqual:@YES]) allPrimitives = NO;
        }
        result[@"primitives"] = primitives;
        if (!allPrimitives || primitives.count != 3) { close(baseFD); break; }

        NSMutableArray *refusals = [NSMutableArray array];
        BOOL allRoots = YES;
        for (NSString *kind in @[@"root-exclusion", @"root-protection"]) {
            NSString *name = [base stringByAppendingPathComponent:kind];
            int directory = make_directory(name);
            if (directory < 0) { allRoots = NO; break; }
            BOOL prepared = sb_s3_owner_file_policy(directory, 1, 1) == 0;
            BOOL changed = [kind isEqualToString:@"root-exclusion"] ? excluded(name, NO) : protection(name, NSFileProtectionNone);
            BOOL refused = prepared && changed && sb_s3_owner_file_policy(directory, 1, 0) != 0;
            int32_t code = -1;
            NSDictionary *probe = refused ? go_probe(name, &code) : nil;
            struct stat state;
            BOOL absent = fstatat(directory, "state.json", &state, AT_SYMLINK_NOFOLLOW) == -1 && errno == ENOENT;
            NSArray *entries = [[NSFileManager defaultManager] contentsOfDirectoryAtPath:name error:NULL];
            BOOL emptyRoot = entries != nil && entries.count == 0;
            BOOL released = close(directory) == 0;
            BOOL passed = refused && code == 2 && [probe[@"reason"] isEqualToString:@"OWNER_POLICY_REFUSED"] &&
                [probe[@"passed"] isEqual:@NO] && absent && emptyRoot && released;
            [refusals addObject:@{@"kind":kind, @"native_policy_refused":@(refused), @"go_admission_status":@(code),
                @"go_admission_refused":@([probe[@"reason"] isEqualToString:@"OWNER_POLICY_REFUSED"]),
                @"no_state_or_temp_written":@((BOOL)(absent && emptyRoot)), @"closed":@(released), @"passed":@(passed)}];
            if (!passed) allRoots = NO;
        }
        result[@"root_refusals"] = refusals;
        if (!allRoots || refusals.count != 2) { close(baseFD); break; }
        NSString *writer = [base stringByAppendingPathComponent:@"actual-writer"];
        int writerFD = make_directory(writer);
        if (writerFD < 0) { close(baseFD); break; }
        BOOL prepared = sb_s3_owner_file_policy(writerFD, 1, 1) == 0;
        int32_t code = -1;
        NSDictionary *probe = prepared ? go_probe(writer, &code) : nil;
        result[@"actual_writer"] = probe ?: @{};
        result[@"actual_writer_status"] = @(code);
        BOOL writerClosed = close(writerFD) == 0, baseClosed = close(baseFD) == 0;
        if (prepared && code == 0 && [probe[@"passed"] isEqual:@YES] && writerClosed && baseClosed) {
            result[@"passed"] = @YES; result[@"reason"] = @"";
        }
        } while (0);
        NSData *encoded = [NSJSONSerialization dataWithJSONObject:result options:NSJSONWritingSortedKeys error:NULL];
        if (!encoded || encoded.length >= 16384) return NULL;
        char *bytes = calloc(1, encoded.length + 1);
        if (bytes) memcpy(bytes, encoded.bytes, encoded.length);
        return bytes;
    }
}

void backup_owner_policy_free(char *value) { free(value); }
