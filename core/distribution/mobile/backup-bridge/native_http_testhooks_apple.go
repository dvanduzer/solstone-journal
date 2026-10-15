//go:build integration && (darwin || ios)

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
#include <stdlib.h>
#include "native_http_internal.h"
int32_t sb_http_test_install(int32_t mode);
int32_t sb_http_test_wait(uint32_t stage);
int32_t sb_http_test_drain(void);
void sb_http_test_release(uint32_t stage);
const void *sb_http_test_snapshot(uint32_t *length);
void sb_http_test_uninstall(void);
*/
import "C"

func nativeHTTPCancelBeforeStart() int32 {
	encoded := []byte(`{"url":"https://storage.example/object","method":"GET","headers":{}}`)
	pointer := C.CBytes(encoded)
	defer C.free(pointer)
	call := C.sb_http_create(pointer, C.uint32_t(len(encoded)), nil, 0, 1024)
	if call == nil {
		return -1
	}
	defer C.sb_http_release(call)
	C.sb_http_cancel(call)
	C.sb_http_cancel(call)
	C.sb_http_start(call)
	C.sb_http_start(call)
	C.sb_http_cancel(call)
	return int32(C.sb_http_wait(call))
}

func nativeHTTPNullBoundaries() bool {
	var size C.uint32_t = 123
	metadata := C.sb_http_metadata(nil, &size)
	if metadata != nil || size != 0 {
		return false
	}
	size = 123
	body := C.sb_http_body(nil, &size)
	if body != nil || size != 0 {
		return false
	}
	C.sb_http_start(nil)
	C.sb_http_cancel(nil)
	C.sb_http_release(nil)
	return C.sb_http_wait(nil) == 1 && C.sb_http_create(nil, 0, nil, 0, 0) == nil
}

func installNativeHTTPFixture(mode int) bool  { return C.sb_http_test_install(C.int32_t(mode)) == 0 }
func waitNativeHTTPFixture(stage uint32) bool { return C.sb_http_test_wait(C.uint32_t(stage)) == 0 }
func releaseNativeHTTPFixture(stage uint32)   { C.sb_http_test_release(C.uint32_t(stage)) }
func drainNativeHTTPFixture() bool            { return C.sb_http_test_drain() == 0 }
func uninstallNativeHTTPFixture()             { C.sb_http_test_uninstall() }
func nativeHTTPFixtureSnapshot() []byte {
	var size C.uint32_t
	data := C.sb_http_test_snapshot(&size)
	if size > maxHTTPMetadataBytes {
		return nil
	}
	return C.GoBytes(data, C.int(size))
}
