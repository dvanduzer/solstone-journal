// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#import <Foundation/Foundation.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int owner_root = -1, input_root = -1;
int32_t sb_s3_owner_file_policy(int32_t fd, int32_t directory, int32_t apply);

static BOOL same_object(const struct stat *a, const struct stat *b) {
    return a->st_dev == b->st_dev && a->st_ino == b->st_ino;
}

// Walk the system-supplied canonical container path without following aliases.
// Search-only ancestor descriptors follow the qualified iOS filesystem rule.
static int open_directory(const char *path) {
    if (!path || path[0] != '/' || strlen(path) >= PATH_MAX) return -1;
    char copy[PATH_MAX]; memcpy(copy, path, strlen(path) + 1);
    int parent = open("/", O_SEARCH | O_DIRECTORY | O_CLOEXEC);
    if (parent < 0) return -1;
    char *remaining = copy + 1, *part;
    while ((part = strsep(&remaining, "/"))) {
        if (!*part || !strcmp(part, ".") || !strcmp(part, "..")) { close(parent); return -1; }
        int flags = (remaining ? O_SEARCH : O_RDONLY) | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC;
        int child = openat(parent, part, flags);
        if (child < 0 && errno == ENOENT && mkdirat(parent, part, 0700) == 0) child = openat(parent, part, flags);
        struct stat held, named;
        if (child < 0 || fstat(child, &held) || fstatat(parent, part, &named, AT_SYMLINK_NOFOLLOW) || !same_object(&held, &named) || !S_ISDIR(held.st_mode)) {
            if (child >= 0) close(child);
            close(parent); return -1;
        }
        close(parent); parent = child;
    }
    return parent;
}

static BOOL private_policy(int fd, BOOL directory, BOOL apply) {
    struct stat held, before, after;
    char path[PATH_MAX];
    if (fstat(fd, &held) || fcntl(fd, F_GETPATH, path) || lstat(path, &before) || !same_object(&held, &before) ||
        held.st_uid != geteuid() || (held.st_mode & 07777) != (directory ? 0700 : 0600) ||
        (directory ? !S_ISDIR(held.st_mode) : (!S_ISREG(held.st_mode) || held.st_nlink != 1))) return NO;
    NSString *name = [[NSFileManager defaultManager] stringWithFileSystemRepresentation:path length:strlen(path)];
    if (!name) return NO;
    NSURL *url = [NSURL fileURLWithPath:name isDirectory:directory];
    NSError *error = nil;
    if (apply && (![url setResourceValue:@YES forKey:NSURLIsExcludedFromBackupKey error:&error] ||
        ![[NSFileManager defaultManager] setAttributes:@{NSFileProtectionKey:NSFileProtectionComplete} ofItemAtPath:name error:&error])) return NO;
    [url removeCachedResourceValueForKey:NSURLIsExcludedFromBackupKey];
    NSNumber *excluded = nil;
    NSDictionary *attributes = [[NSFileManager defaultManager] attributesOfItemAtPath:name error:&error];
    if (![url getResourceValue:&excluded forKey:NSURLIsExcludedFromBackupKey error:&error] || ![excluded isKindOfClass:NSNumber.class] || !excluded.boolValue ||
        ![attributes[NSFileProtectionKey] isEqualToString:NSFileProtectionComplete] || lstat(path, &after) || !same_object(&held, &after) ||
        held.st_mode != after.st_mode || held.st_uid != after.st_uid || (!directory && after.st_nlink != 1)) return NO;
    return YES;
}

static BOOL census(int fd, BOOL fresh) {
    int readable = openat(fd, ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (readable < 0) return NO;
    DIR *directory = fdopendir(readable);
    if (!directory) { close(readable); return NO; }
    BOOL valid = YES;
    errno = 0;
    struct dirent *entry;
    while ((entry = readdir(directory))) {
        if (!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, "..")) continue;
        if (fresh || strcmp(entry->d_name, "fixture.json")) { valid = NO; break; }
    }
    if (errno) valid = NO;
    if (closedir(directory)) valid = NO;
    return valid;
}

typedef struct {
    int directory, parent;
    char name[sizeof("callable-s3-alternate-") + 32];
} AlternateRoot;

void *backup_s3_alternate_root_create(const char *name) {
    @autoreleasepool {
    const char *prefix = "callable-s3-alternate-";
    size_t prefix_length = strlen(prefix);
    if (owner_root < 0 || !name || strnlen(name, prefix_length + 33) != prefix_length + 32 ||
        strncmp(name, prefix, prefix_length) || !private_policy(owner_root, YES, NO)) return NULL;
    for (size_t index = prefix_length; index < prefix_length + 32; index++) {
        if (!strchr("0123456789abcdef", name[index])) return NULL;
    }
    int parent = openat(owner_root, "..", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (parent < 0) return NULL;
    int directory = -1;
    AlternateRoot *owned = NULL;
    do {
        // A new empty sibling only. Never reuse, repair or remove an existing
        // namespace; the retained parent is obtained from the owned fixture root.
        if (mkdirat(parent, name, 0700)) break;
        directory = openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        struct stat held, named;
        if (directory < 0 || fstat(directory, &held) || fstatat(parent, name, &named, AT_SYMLINK_NOFOLLOW) ||
            !same_object(&held, &named) || !S_ISDIR(held.st_mode) || held.st_uid != geteuid() ||
            (held.st_mode & 07777) != 0700 || !census(directory, YES) ||
            sb_s3_owner_file_policy(directory, 1, 1) || sb_s3_owner_file_policy(directory, 1, 0) ||
            !private_policy(directory, YES, NO)) break;
        owned = calloc(1, sizeof(*owned));
        if (!owned) break;
        owned->directory = directory; owned->parent = parent;
        memcpy(owned->name, name, strlen(name) + 1);
    } while (0);
    if (!owned) { if (directory >= 0) close(directory); close(parent); }
    return owned;
    }
}

int32_t backup_s3_alternate_root_check(void *pointer, const char *path) {
    @autoreleasepool {
    AlternateRoot *owned = pointer;
    struct stat held, named, observed;
    char retained[PATH_MAX];
    if (!owned || !path || strnlen(path, PATH_MAX + 1) >= PATH_MAX ||
        fstat(owned->directory, &held) || fstatat(owned->parent, owned->name, &named, AT_SYMLINK_NOFOLLOW) ||
        !same_object(&held, &named) || fcntl(owned->directory, F_GETPATH, retained) || strcmp(path, retained) ||
        lstat(path, &observed) || !same_object(&held, &observed) || !census(owned->directory, YES) ||
        sb_s3_owner_file_policy(owned->directory, 1, 0) || !private_policy(owned->directory, YES, NO)) return 1;
    return 0;
    }
}

int32_t backup_s3_alternate_root_free(void *pointer) {
    AlternateRoot *owned = pointer;
    if (!owned) return 1;
    int directory = close(owned->directory), parent = close(owned->parent);
    free(owned);
    return directory || parent ? 1 : 0;
}

int32_t backup_s3_prepare_roots(const char *owner, const char *input, int32_t fresh) {
    @autoreleasepool {
        if (owner_root >= 0 || input_root >= 0) return 1;
        int state = open_directory(owner), incoming = open_directory(input);
        BOOL valid = state >= 0 && incoming >= 0 && private_policy(state, YES, YES) && private_policy(incoming, YES, YES) && census(incoming, fresh != 0);
        if (fresh && valid) valid = census(state, YES);
        if (!valid) { if (state >= 0) close(state); if (incoming >= 0) close(incoming); return 1; }
        owner_root = state; input_root = incoming;
        return 0;
    }
}

char *backup_s3_load_fixture(uint32_t *length, int32_t *code) {
    @autoreleasepool {
        if (!length || !code) return NULL;
        *length = 0; *code = 1;
        if (owner_root < 0 || input_root < 0 || !private_policy(owner_root, YES, NO) || !private_policy(input_root, YES, NO) || !census(input_root, NO)) return NULL;
        int fd = openat(input_root, "fixture.json", O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC);
        if (fd < 0) return NULL;
        struct stat before, named, after;
        if (fstat(fd, &before) || before.st_size <= 0 || before.st_size > 32768 || !private_policy(fd, NO, YES) ||
            fstatat(input_root, "fixture.json", &named, AT_SYMLINK_NOFOLLOW) || !same_object(&before, &named)) { close(fd); return NULL; }
        char *bytes = calloc(1, 32769);
        if (!bytes) { close(fd); return NULL; }
        size_t size = 0;
        while (size < 32769) {
            ssize_t got = read(fd, bytes + size, 32769 - size);
            if (got < 0 && errno == EINTR) continue;
            if (got < 0) { size = 32769; break; }
            if (!got) break;
            size += (size_t)got;
        }
        BOOL valid = size > 0 && size <= 32768 && size == (size_t)before.st_size && fstat(fd, &after) == 0 && same_object(&before, &after) &&
            after.st_size == before.st_size && after.st_mtimespec.tv_sec == before.st_mtimespec.tv_sec && after.st_mtimespec.tv_nsec == before.st_mtimespec.tv_nsec &&
            fstatat(input_root, "fixture.json", &named, AT_SYMLINK_NOFOLLOW) == 0 && same_object(&after, &named) && private_policy(fd, NO, NO);
        if (close(fd)) valid = NO;
        if (!valid) { volatile char *wipe = bytes; for (size_t i = 0; i < 32769; i++) wipe[i] = 0; free(bytes); return NULL; }
        *length = (uint32_t)size; *code = 0;
        return bytes;
    }
}

void backup_s3_fixture_free(char *bytes) {
    if (!bytes) return;
    volatile char *wipe = bytes;
    for (size_t i = 0; i < 32769; i++) wipe[i] = 0;
    free(bytes);
}

// Qualification watcher receives a duplicate of the already held owner root.
// It cannot change that root or install production authority.
int32_t backup_s3_duplicate_owner_root(void) {
    @autoreleasepool {
        if (owner_root < 0 || !private_policy(owner_root, YES, NO)) return -1;
        return fcntl(owner_root, F_DUPFD_CLOEXEC, 0);
    }
}
