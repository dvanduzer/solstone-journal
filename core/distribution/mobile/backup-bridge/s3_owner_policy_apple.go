//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
int32_t sb_s3_owner_file_policy(int32_t fd, int32_t directory, int32_t apply);
*/
import "C"

func s3StateRootPolicy(fd int) error {
	if C.sb_s3_owner_file_policy(C.int32_t(fd), 1, 0) != 0 {
		return errS3OwnerPolicy
	}
	return nil
}

func s3StateFilePolicy(fd int, apply bool) error {
	var shouldApply C.int32_t
	if apply {
		shouldApply = 1
	}
	if C.sb_s3_owner_file_policy(C.int32_t(fd), 0, shouldApply) != 0 {
		return errS3OwnerPolicy
	}
	return nil
}

// Used only by private test setup; production callers must supply the root with
// policy already applied. It is not a C/Swift bridge export or transport hook.
func s3PrepareStateRootPolicy(fd int) error {
	if C.sb_s3_owner_file_policy(C.int32_t(fd), 1, 1) != 0 {
		return errS3OwnerPolicy
	}
	return nil
}
