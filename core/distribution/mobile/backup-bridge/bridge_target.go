// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
*/
import "C"

// A failed admission with nonzero outID retains refused cleanup custody.
// Release is explicit. Neither cancellation nor an operation Wait/Free releases
// the target owner. There is no password, repository or transport authority here.
//
//export restic_bridge_target_admit_v1
func restic_bridge_target_admit_v1(version C.uint32_t, targetFD C.int32_t, path *C.char, pathLen C.uint32_t, outID *C.uint64_t) C.int32_t {
	if outID == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	*outID = 0
	if version != 1 || path == nil || pathLen == 0 {
		return C.int32_t(StatusInvalidArgument)
	}
	if pathLen > maxPathLen {
		return C.int32_t(StatusCapExceeded)
	}
	id, status := admitRootedTarget(int(targetFD), string(copyCBytes(path, pathLen)))
	*outID = C.uint64_t(id)
	return C.int32_t(status)
}

//export restic_bridge_target_revalidate_v1
func restic_bridge_target_revalidate_v1(version C.uint32_t, id C.uint64_t) C.int32_t {
	if version != 1 {
		return C.int32_t(StatusInvalidArgument)
	}
	return C.int32_t(revalidateRootedTarget(uint64(id)))
}

//export restic_bridge_target_release_v1
func restic_bridge_target_release_v1(version C.uint32_t, id C.uint64_t) C.int32_t {
	if version != 1 {
		return C.int32_t(StatusInvalidArgument)
	}
	return C.int32_t(releaseRootedTarget(uint64(id)))
}
