//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#import <Foundation/Foundation.h>
#include <TargetConditionals.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

// Fixed local file policy, never an app-installed authority callback. The actual
// retained inode must agree with the path Foundation observes before/after use.
int32_t sb_s3_owner_file_policy(int32_t fd, int32_t directory, int32_t apply) {
    @autoreleasepool {
        struct stat held, observed;
        char path[PATH_MAX];
        if (fd < 0 || fstat(fd, &held) != 0 || fcntl(fd, F_GETPATH, path) != 0 ||
            (directory ? !S_ISDIR(held.st_mode) : !S_ISREG(held.st_mode)) ||
            held.st_uid != geteuid() || (held.st_mode & 07777) != (directory ? 0700 : 0600) ||
            (!directory && held.st_nlink != 1)) return 1;
        NSString *name = [[NSFileManager defaultManager] stringWithFileSystemRepresentation:path length:strlen(path)];
        if (!name) return 1;
        NSURL *url = [NSURL fileURLWithPath:name isDirectory:directory != 0];
        if (apply) {
            NSError *error = nil;
            if (![url setResourceValue:@YES forKey:NSURLIsExcludedFromBackupKey error:&error]) return 1;
#if TARGET_OS_IPHONE
            if (![[NSFileManager defaultManager] setAttributes:@{NSFileProtectionKey: NSFileProtectionComplete}
                ofItemAtPath:name error:&error]) return 1;
#endif
        }
        NSNumber *excluded = nil;
        NSError *error = nil;
        [url removeCachedResourceValueForKey:NSURLIsExcludedFromBackupKey];
        if (![url getResourceValue:&excluded forKey:NSURLIsExcludedFromBackupKey error:&error] ||
            ![excluded isKindOfClass:[NSNumber class]] || !excluded.boolValue) return 1;
#if TARGET_OS_IPHONE
        NSDictionary *attributes = [[NSFileManager defaultManager] attributesOfItemAtPath:name error:&error];
        if (![attributes[NSFileProtectionKey] isEqualToString:NSFileProtectionComplete]) return 1;
#endif
        if (lstat(path, &observed) != 0 || observed.st_dev != held.st_dev || observed.st_ino != held.st_ino ||
            observed.st_uid != held.st_uid || observed.st_mode != held.st_mode ||
            (!directory && observed.st_nlink != 1)) return 1;
        return 0;
    }
}
