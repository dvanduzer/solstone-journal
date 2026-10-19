//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
#include <stdlib.h>
typedef struct {
    uint32_t version;
    uint32_t check_read_mode;
    uint64_t retry_lock_ms;
    uint32_t group;
    uint32_t groups;
    uint32_t reserved1;
    uint32_t reserved2;
} rotation_test_options;
int32_t restic_bridge_s3_remove_recovery_key_v1_start(void*, void*, int32_t, const char*, uint32_t, uint32_t, uint64_t*);
*/
import "C"
import "unsafe"

// The request uses the maintained frozen S3 layout; only this additive entry
// receives maintenance authority and the caller's retained state descriptor.
func rotationCS3Remove(input sourceCInput, stateFD int, keyID string, maintenance uint32) (uint64, int32) {
	var allocations []unsafe.Pointer
	defer func() {
		for _, p := range allocations {
			C.free(p)
		}
	}()
	request := sourceCS3Request(input, OpRemoveRecoveryKey, &allocations)
	options := C.rotation_test_options{version: 2, reserved1: 1}
	key := C.CBytes([]byte(keyID))
	defer C.free(key)
	id := C.uint64_t(0xfedcba9876543210)
	status := C.restic_bridge_s3_remove_recovery_key_v1_start(unsafe.Pointer(&request), unsafe.Pointer(&options),
		C.int32_t(stateFD), (*C.char)(key), C.uint32_t(len(keyID)), C.uint32_t(maintenance), &id)
	return uint64(id), int32(status)
}
