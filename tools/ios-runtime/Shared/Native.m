// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#import <Foundation/Foundation.h>
#include <mach/mach.h>
#include <os/proc.h>
#include <sys/resource.h>
#include <unistd.h>
#include "Bridge.h"
char *runtime_resources(void) {
    task_vm_info_data_t vm={0}; mach_msg_type_number_t n=TASK_VM_INFO_COUNT;
    kern_return_t rc=task_info(mach_task_self(), TASK_VM_INFO,(task_info_t)&vm,&n);
    struct rusage usage={0}; int urc=getrusage(RUSAGE_SELF,&usage);
    NSDictionary *v=@{@"pid":@(getpid()),@"uid":@(getuid()),@"vm_rc":@(rc),@"rss_bytes":@(vm.resident_size),@"footprint_bytes":@(vm.phys_footprint),@"available_memory_bytes":@(os_proc_available_memory()),@"rusage_rc":@(urc),@"output_blocks":@(usage.ru_oublock),@"input_blocks":@(usage.ru_inblock),@"user_seconds":@(usage.ru_utime.tv_sec+usage.ru_utime.tv_usec/1e6),@"system_seconds":@(usage.ru_stime.tv_sec+usage.ru_stime.tv_usec/1e6)};
    NSData *d=[NSJSONSerialization dataWithJSONObject:v options:0 error:nil];return strndup(d.bytes,d.length);
}
void runtime_resources_free(char *p){free(p);}
