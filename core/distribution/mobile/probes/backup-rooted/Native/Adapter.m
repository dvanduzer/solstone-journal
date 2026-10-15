// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <mach/mach.h>
#include <stdlib.h>
#include <string.h>
#include "../Shared/Bridge.h"

int rooted_local_fixture_run(int argc, char **argv);

int32_t backup_rooted_run(const char *directory) {
    if (!directory) return 99;
    char *owned=strdup(directory);
    if (!owned) return 98;
    char program[]="rooted-local-fixture";
    char *args[]={program,owned,NULL};
    int code=rooted_local_fixture_run(2,args);
    free(owned);
    return code;
}

uint64_t backup_rooted_footprint(void) {
    task_vm_info_data_t info={0};mach_msg_type_number_t count=TASK_VM_INFO_COUNT;
    return task_info(mach_task_self(),TASK_VM_INFO,(task_info_t)&info,&count)==KERN_SUCCESS?info.phys_footprint:0;
}
