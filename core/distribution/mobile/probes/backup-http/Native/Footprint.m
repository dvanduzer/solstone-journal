// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
#include <stdint.h>
#include <mach/mach.h>

uint64_t http_probe_footprint(void) {
    task_vm_info_data_t information = {0};
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    if (task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&information, &count) != KERN_SUCCESS) return 0;
    return information.phys_footprint;
}
