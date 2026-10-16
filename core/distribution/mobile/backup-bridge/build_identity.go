// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
*/
import "C"

import (
	"encoding/hex"
	"unsafe"
)

// Supplied only by the immutable build recipe through -X. There is no runtime
// setter, environment lookup, receipt reader or operation authority here.
var linkedBuildIdentitySha256 string

func decodeBuildIdentity(value string) ([32]byte, bool) {
	var identity [32]byte
	if len(value) != 64 {
		return identity, false
	}
	for _, b := range []byte(value) {
		if !(b >= '0' && b <= '9' || b >= 'a' && b <= 'f') {
			return identity, false
		}
	}
	_, err := hex.Decode(identity[:], []byte(value))
	return identity, err == nil
}

//export restic_bridge_build_identity_v1
func restic_bridge_build_identity_v1(output *C.uint8_t, capacity C.uint32_t) C.int32_t {
	if output == nil || capacity != 32 {
		return C.int32_t(StatusInvalidArgument)
	}
	buffer := unsafe.Slice((*byte)(unsafe.Pointer(output)), 32)
	clear(buffer)
	identity, valid := decodeBuildIdentity(linkedBuildIdentitySha256)
	if !valid {
		return C.int32_t(StatusInvalidArgument)
	}
	copy(buffer, identity[:])
	return C.int32_t(StatusOk)
}
