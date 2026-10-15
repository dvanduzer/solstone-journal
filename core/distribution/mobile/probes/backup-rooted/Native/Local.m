// The maintained host C qualification caller, with its test entrypoint renamed.
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include "librestic_bridge.h"
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/file.h>
#include <unistd.h>

static int join(char *out, size_t cap, const char *base, const char *name) {
    int n = snprintf(out, cap, "%s/%s", base, name);
    return n > 0 && (size_t)n < cap ? 0 : 1;
}
static int write_fixture(const char *path, const char *value) {
    FILE *file = fopen(path, "wb");
    if (!file) return 1;
    int failed = fwrite(value, 1, strlen(value), file) != strlen(value);
    return fclose(file) || failed;
}
static int call(restic_request *request, restic_result *result) {
    uint64_t id = 0;
    if (restic_bridge_start(request, &id) || !id) return 1;
    if (restic_bridge_wait(id, result) || restic_bridge_free(id)) return 1;
    return result->status != 0;
}
static int lease_is_held(const char *target) {
    char path[4096];
    if (join(path,sizeof(path),target,"backup-activity.lock")) return 0;
    int fd=open(path,O_RDWR|O_NOFOLLOW);if (fd<0) return 0;
    int rc=flock(fd,LOCK_EX|LOCK_NB),saved=errno;
    if (!rc) (void)flock(fd,LOCK_UN);
    if (close(fd)) return 0;
    return rc==-1 && (saved==EWOULDBLOCK || saved==EAGAIN);
}
int rooted_local_fixture_run(int argc, char **argv) {
    if (argc != 2) return 1;
    char repo[4096], source[4096], retained[4096], target[4096], path[4096];
    if (join(repo,sizeof(repo),argv[1],"rooted-abi-repo") || join(source,sizeof(source),argv[1],"rooted-abi-source") ||
        join(retained,sizeof(retained),argv[1],"rooted-abi-source-retained") || join(target,sizeof(target),argv[1],"rooted-abi-target")) return 2;
    if (mkdir(repo,0700) || mkdir(source,0700) || mkdir(target,0700)) return 3;
    if (join(path,sizeof(path),source,"owner") || write_fixture(path,"selected owner bytes")) return 4;
    if (join(path,sizeof(path),source,"backup-activity.lock") || write_fixture(path,"snapshot lock")) return 5;
    if (join(path,sizeof(path),target,"backup-activity.lock") || write_fixture(path,"retained lease")) return 6;
    char password[]="rooted-abi-synthetic-key";
    restic_request request={0};request.operation=1;request.password=password;request.password_len=(uint32_t)strlen(password);
    request.repo_path=repo;request.repo_path_len=(uint32_t)strlen(repo);
    restic_result *result=calloc(1,sizeof(*result));if (!result || call(&request,result)) return 7;
    request.operation=4;request.source_path=source;request.source_path_len=(uint32_t)strlen(source);
    if (call(&request,result) || strlen(result->snapshot_id)!=64) return 8;
    char snapshot[65];memcpy(snapshot,result->snapshot_id,sizeof(snapshot));
    if (rename(source,retained)) return 9;
    request.operation=6;request.source_path=NULL;request.source_path_len=0;request.restore_path=target;request.restore_path_len=(uint32_t)strlen(target);
    request.snapshot_id=snapshot;request.snapshot_id_len=64;
    int fd=open(target,O_RDONLY|O_DIRECTORY);if (fd<0) return 10;
    char selector[]="rooted-abi-source";uint32_t size=(uint32_t)strlen(selector);uint64_t id=123;
    if (restic_bridge_restore_root_v1_start(2,&request,fd,selector,size,&id)!=1 || id) return 11;
    id=123;if (restic_bridge_restore_root_v1_start(1,&request,fd,(char *)(uintptr_t)1,4097,&id)!=5 || id) return 12;
    id=123;if (restic_bridge_restore_root_v1_start(1,&request,-1,selector,size,&id)!=1 || id) return 13;
    id=123;if (restic_bridge_restore_root_v1_start(1,&request,fd,"../rooted-abi-source",20,&id)!=1 || id) return 14;
    restic_s3_request remote={0};remote.version=1;remote.operation=6;
    id=123;if (restic_bridge_s3_restore_root_v1_start(&remote,fd,selector,size,&id)!=1 || id) return 15;
    if (restic_bridge_s3_restore_root_v1_start(NULL,-1,NULL,0,NULL)!=1) return 16;
    restic_restore_root_outcome_v1 outcome;memset(&outcome,255,sizeof(outcome));
    if (restic_bridge_restore_root_v1_outcome(0,2,&outcome)!=1 || outcome.version || outcome.target_admitted || outcome.target_may_have_mutated) return 17;
    if (restic_bridge_restore_root_v1_outcome(0,1,&outcome)!=7 || outcome.version) return 18;
    char wrong[]="wrong-key";request.password=wrong;request.password_len=(uint32_t)strlen(wrong);
    if (restic_bridge_restore_root_v1_start(1,&request,fd,selector,size,&id) || !id) return 19;
    if (restic_bridge_wait(id,result) || !result->status) return 20;
    if (restic_bridge_restore_root_v1_outcome(id,1,&outcome) || outcome.version!=1 || outcome.target_admitted!=1 || outcome.target_may_have_mutated) return 21;
    uint32_t required=99;if (restic_bridge_result_metadata(id,NULL,0,&required)==0 || required) return 22;
    if (restic_bridge_free(id)) return 23;
    request.password=password;request.password_len=(uint32_t)strlen(password);
    if (restic_bridge_restore_root_v1_start(1,&request,fd,selector,size,&id) || !id) return 24;
    if (close(fd)) return 25;
    memset(selector,'x',size);
    if (restic_bridge_wait(id,result) || result->status) return 26;
    if (restic_bridge_restore_root_v1_outcome(id,1,&outcome) || outcome.target_admitted!=1 || outcome.target_may_have_mutated!=1) return 27;
    required=0;if (restic_bridge_result_metadata(id,NULL,0,&required)!=5 || !required || required>4*1024*1024) return 28;
    char *metadata=calloc(1,(size_t)required+1);if (!metadata) return 29;
    if (restic_bridge_result_metadata(id,metadata,required,&required) || !strstr(metadata,"existing-root-restore")) return 30;
    free(metadata);if (restic_bridge_free(id)) return 31;
    if (restic_bridge_restore_root_v1_outcome(id,1,&outcome)!=7 || outcome.version) return 32;
    if (join(path,sizeof(path),target,"owner")) return 33;
    FILE *file=fopen(path,"rb");if (!file) return 34;
    char bytes[64]={0};size_t count=fread(bytes,1,sizeof(bytes),file);
    if (fclose(file) || count!=20 || memcmp(bytes,"selected owner bytes",20)) return 35;
    if (join(path,sizeof(path),target,"backup-activity.lock")) return 36;
    file=fopen(path,"rb");if (!file) return 37;
    memset(bytes,0,sizeof(bytes));count=fread(bytes,1,sizeof(bytes),file);
    if (fclose(file) || count!=14 || memcmp(bytes,"retained lease",14)) return 38;

    char admitted[4096];
    if (join(admitted,sizeof(admitted),argv[1],"rooted-abi-admitted-target") || mkdir(admitted,0700)) return 39;
    if (join(path,sizeof(path),admitted,"backup-activity.lock") || write_fixture(path,"whole restore lease")) return 40;
    struct stat lock_before,lock_after;if (stat(path,&lock_before)) return 41;
    fd=open(admitted,O_RDONLY|O_DIRECTORY);if (fd<0) return 42;
    uint64_t owner=123;
    if (restic_bridge_target_admit_v1(2,fd,admitted,(uint32_t)strlen(admitted),&owner)!=1 || owner) return 43;
    owner=123;if (restic_bridge_target_admit_v1(1,fd,(char *)(uintptr_t)1,4097,&owner)!=5 || owner) return 44;
    owner=123;if (restic_bridge_target_admit_v1(1,fd,target,(uint32_t)strlen(target),&owner)!=1 || owner) return 45;
    if (restic_bridge_target_admit_v1(1,fd,admitted,(uint32_t)strlen(admitted),&owner) || !owner || close(fd)) return 46;
    if (!lease_is_held(admitted) || restic_bridge_free(owner)!=7 || restic_bridge_target_revalidate_v1(1,id)!=7) return 47;
    if (restic_bridge_target_release_v1(1,id)!=7 || restic_bridge_target_revalidate_v1(2,owner)!=1) return 48;
    request.restore_path=admitted;request.restore_path_len=(uint32_t)strlen(admitted);
    memcpy(selector,"rooted-abi-source",sizeof(selector));
    id=123;if (restic_bridge_restore_admitted_v1_start(2,&request,owner,selector,size,&id)!=1 || id) return 49;
    id=123;if (restic_bridge_restore_admitted_v1_start(1,&request,0,selector,size,&id)!=7 || id) return 50;
    id=123;if (restic_bridge_s3_restore_admitted_v1_start(&remote,owner,selector,size,&id)!=1 || id) return 51;
    if (restic_bridge_target_revalidate_v1(1,owner)) return 52;
    if (restic_bridge_restore_admitted_v1_start(1,&request,owner,selector,size,&id) || !id) return 53;
    uint64_t second=123;
    if (restic_bridge_target_release_v1(1,owner)!=6 ||
        restic_bridge_restore_admitted_v1_start(1,&request,owner,selector,size,&second)!=6 || second) return 54;
    if (restic_bridge_wait(id,result) || result->status || !lease_is_held(admitted)) return 55;
    if (restic_bridge_restore_root_v1_outcome(id,1,&outcome) || outcome.target_admitted!=1 || outcome.target_may_have_mutated!=1) return 56;
    if (restic_bridge_target_release_v1(1,owner)!=6 || restic_bridge_free(id) || restic_bridge_target_revalidate_v1(1,owner)) return 57;
    if (!lease_is_held(admitted) || join(path,sizeof(path),admitted,"owner")) return 58;
    file=fopen(path,"rb");if (!file) return 59;
    memset(bytes,0,sizeof(bytes));count=fread(bytes,1,sizeof(bytes),file);
    if (fclose(file) || count!=20 || memcmp(bytes,"selected owner bytes",20)) return 60;
    if (join(path,sizeof(path),admitted,"post-work") || write_fixture(path,"held repair work")) return 61;
    fd=open(admitted,O_RDONLY|O_DIRECTORY);if (fd<0) return 62;
    second=123;if (restic_bridge_target_admit_v1(1,fd,admitted,(uint32_t)strlen(admitted),&second)!=6 || second) return 63;
    request.password=wrong;request.password_len=(uint32_t)strlen(wrong);
    if (restic_bridge_restore_admitted_v1_start(1,&request,owner,selector,size,&id) || !id || restic_bridge_wait(id,result) || !result->status) return 64;
    if (restic_bridge_restore_root_v1_outcome(id,1,&outcome) || outcome.target_admitted!=1 || outcome.target_may_have_mutated) return 65;
    if (restic_bridge_free(id) || !lease_is_held(admitted)) return 66;
    if (join(path,sizeof(path),admitted,"backup-activity.lock") || stat(path,&lock_after) ||
        lock_before.st_dev!=lock_after.st_dev || lock_before.st_ino!=lock_after.st_ino) return 67;
    file=fopen(path,"rb");if (!file) return 68;
    memset(bytes,0,sizeof(bytes));count=fread(bytes,1,sizeof(bytes),file);
    if (fclose(file) || count!=19 || memcmp(bytes,"whole restore lease",19)) return 69;
    if (restic_bridge_target_release_v1(1,owner) || restic_bridge_target_revalidate_v1(1,owner)!=7 || restic_bridge_target_release_v1(1,owner)!=7) return 70;
    if (restic_bridge_target_admit_v1(1,fd,admitted,(uint32_t)strlen(admitted),&second) || !second || close(fd) || restic_bridge_target_release_v1(1,second)) return 71;
    free(result);return 0;
}
