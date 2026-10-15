// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#import <Foundation/Foundation.h>
#include <stdlib.h>
#include <mach/mach.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <fcntl.h>
#include <pthread.h>
#include "librestic_bridge.h"
#include "../Shared/Bridge.h"

uint64_t backup_probe_start(const char *json, int32_t *code) {
    @autoreleasepool {
        if (!json || !code) return 0;
        NSData *bytes = [NSData dataWithBytes:json length:strlen(json)];
        NSDictionary *value = [NSJSONSerialization JSONObjectWithData:bytes options:0 error:NULL];
        if (![value isKindOfClass:NSDictionary.class]) { *code=1; return 0; }
        NSArray *fields=@[@"endpoint",@"region",@"bucket",@"prefix",@"access_key",@"secret_key",@"session_token",@"password",@"recovery_password",@"state_path",@"source_path",@"restore_path",@"snapshot_id",@"workspace_path"];
        NSMutableArray *data=[NSMutableArray array];
        for (NSString *field in fields) {
            id item=value[field] ?: @"";
            if (![item isKindOfClass:NSString.class]) { *code=1; return 0; }
            [data addObject:[item dataUsingEncoding:NSUTF8StringEncoding]];
        }
        restic_s3_request request={0};
        request.version=1; request.operation=[value[@"operation"] intValue];
#define FIELD(index,name) request.name=[data[index] bytes]; request.name##_len=(uint32_t)[data[index] length];
        FIELD(0,endpoint); FIELD(1,region); FIELD(2,bucket); FIELD(3,prefix);
        FIELD(4,access_key); FIELD(5,secret_key); FIELD(6,session_token);
        FIELD(7,password); FIELD(8,recovery_password); FIELD(9,state_path);
        FIELD(10,source_path); FIELD(11,restore_path); FIELD(12,snapshot_id); FIELD(13,workspace_path);
#undef FIELD
        uint64_t id=0;
        *code=restic_bridge_s3_start(&request,&id);
        return id;
    }
}
static restic_result emergency_result;
static pthread_mutex_t emergency_result_lock = PTHREAD_MUTEX_INITIALIZER;

char *backup_probe_wait(uint64_t id, int32_t *wait_code) {
    @autoreleasepool {
        if (!wait_code) return NULL;
        *wait_code = -1;
        restic_result *result=calloc(1,sizeof(*result));
        BOOL emergency = result == NULL;
        if (emergency) {
            if (pthread_mutex_lock(&emergency_result_lock)) return NULL;
            memset(&emergency_result, 0, sizeof(emergency_result));
            result = &emergency_result;
        }
        int32_t code=restic_bridge_wait(id,result);
        // This terminal witness is independent of subsequent JSON allocation or
        // decoding. The Swift owner can retire a confirmed-terminal handle even
        // if the adapter cannot construct a result document.
        *wait_code = code;
        NSMutableDictionary *object=[@{@"wait_code":@(code)} mutableCopy];
        if (!code) {
            object[@"status"]=@(result->status);
            object[@"key_id"]=([NSString stringWithUTF8String:result->key_id] ?: @"");
            object[@"snapshot_id"]=([NSString stringWithUTF8String:result->snapshot_id] ?: @"");
            object[@"diagnostic"]=([NSString stringWithUTF8String:result->diagnostic] ?: @"");
            object[@"snapshot_count"]=@(result->snapshot_count);
            if (result->snapshot_count<=4096) {
                NSMutableArray *snapshots=[NSMutableArray array];
                for (uint32_t i=0;i<result->snapshot_count;i++) [snapshots addObject:([NSString stringWithUTF8String:result->snapshot_ids[i]] ?: @"")];
                object[@"snapshot_ids"]=snapshots;
            } else object[@"adapter_overflow"]=@YES;
        }
        if (emergency) {
            memset(&emergency_result, 0, sizeof(emergency_result));
            if (pthread_mutex_unlock(&emergency_result_lock)) object[@"adapter_cleanup_failed"] = @YES;
        } else free(result);
        NSData *bytes=[NSJSONSerialization dataWithJSONObject:object options:NSJSONWritingSortedKeys error:NULL];
        if (!bytes) return NULL;
        char *output=calloc(1,bytes.length+1);
        if (output) memcpy(output,bytes.bytes,bytes.length);
        return output;
    }
}
int32_t backup_probe_cancel(uint64_t id) { return restic_bridge_cancel(id); }
int32_t backup_probe_free(uint64_t id) { return restic_bridge_free(id); }
void backup_probe_json_free(char *json) { free(json); }

uint64_t backup_probe_footprint(void) {
    task_vm_info_data_t info={0}; mach_msg_type_number_t count=TASK_VM_INFO_COUNT;
    return task_info(mach_task_self(),TASK_VM_INFO,(task_info_t)&info,&count)==KERN_SUCCESS ? info.phys_footprint : 0;
}

int32_t backup_probe_set_fixture_metadata(const char *path) {
    const char value[]="synthetic retained metadata";
    if (setxattr(path,"user.fixture",value,sizeof(value)-1,0,0)) return 1;
    if (chmod(path,0640)) return 2;
    struct timespec times[2]={{1700000000,0},{1700000000,0}};
    return utimensat(AT_FDCWD,path,times,AT_SYMLINK_NOFOLLOW) ? 3 : 0;
}
int32_t backup_probe_check_fixture_metadata(const char *path) {
    struct stat st;
    if (lstat(path,&st)) return 1;
    if (!S_ISREG(st.st_mode) || (st.st_mode&07777)!=0640 || st.st_mtimespec.tv_sec!=1700000000 || st.st_mtimespec.tv_nsec!=0) return 2;
    char value[64]={0};
    ssize_t n=getxattr(path,"user.fixture",value,sizeof(value),0,0);
    const char expected[]="synthetic retained metadata";
    return n==(ssize_t)(sizeof(expected)-1) && memcmp(value,expected,sizeof(expected)-1)==0 ? 0 : 3;
}

uint64_t backup_probe_device(const char *path) {
    struct stat st;
    return stat(path,&st)==0 ? (uint64_t)st.st_dev : 0;
}
