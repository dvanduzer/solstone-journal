//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#cgo CFLAGS: -fblocks
#cgo LDFLAGS: -framework Foundation -framework Security
#include <stdlib.h>
#include "native_http_internal.h"
*/
import "C"

import (
	"context"
	"encoding/json"
	"unsafe"
)

func performNativeHTTP(ctx context.Context, encoded, body []byte, limit int) (nativeHTTPReply, error) {
	if len(encoded) == 0 || len(encoded) > maxHTTPHeaderBytes || len(body) > maxHTTPObjectBytes || limit <= 0 || limit > maxHTTPObjectBytes {
		return nativeHTTPReply{}, errHTTPArgument
	}
	metadata := C.CBytes(encoded)
	payload := C.CBytes(body)
	call := C.sb_http_create(metadata, C.uint32_t(len(encoded)), payload, C.uint32_t(len(body)), C.uint32_t(limit))
	// The fixed native constructor copies these buffers before returning.
	if len(encoded) != 0 {
		zeroBytes(unsafe.Slice((*byte)(metadata), len(encoded)))
	}
	if len(body) != 0 {
		zeroBytes(unsafe.Slice((*byte)(payload), len(body)))
	}
	C.free(metadata)
	C.free(payload)
	if call == nil {
		return nativeHTTPReply{}, errHTTPArgument
	}
	defer C.sb_http_release(call)
	cancelFinished := make(chan struct{})
	stopCancellation := context.AfterFunc(ctx, func() {
		C.sb_http_cancel(call)
		close(cancelFinished)
	})
	defer func() {
		if !stopCancellation() {
			<-cancelFinished
		}
	}()
	C.sb_http_start(call)
	code := int32(C.sb_http_wait(call))
	if code != 0 {
		return nativeHTTPReply{}, nativeHTTPError(code)
	}
	var metadataLen, bodyLen C.uint32_t
	metadataPtr := C.sb_http_metadata(call, &metadataLen)
	bodyPtr := C.sb_http_body(call, &bodyLen)
	if metadataLen == 0 || metadataLen > maxHTTPMetadataBytes || bodyLen > C.uint32_t(limit) || metadataPtr == nil || bodyLen != 0 && bodyPtr == nil {
		return nativeHTTPReply{}, errHTTPLimit
	}
	decoded := C.GoBytes(metadataPtr, C.int(metadataLen))
	defer zeroBytes(decoded)
	var reply nativeHTTPReply
	if json.Unmarshal(decoded, &reply) != nil {
		return nativeHTTPReply{}, errHTTPProtocol
	}
	reply.Body = C.GoBytes(bodyPtr, C.int(bodyLen))
	return reply, nil
}

func nativeHTTPError(code int32) error {
	switch code {
	case 1:
		return errHTTPArgument
	case 2:
		return errHTTPLimit
	case 3:
		return context.Canceled
	case 4:
		return errHTTPRedirect
	case 5:
		return errHTTPAuth
	case 8:
		return errHTTPProtocol
	case 9:
		return errHTTPTLS
	default:
		return errHTTPNetwork
	}
}
