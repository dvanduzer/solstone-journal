// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
typedef struct {
    uint32_t version;
    int32_t operation;
    const char *endpoint; uint32_t endpoint_len;
    const char *region; uint32_t region_len;
    const char *bucket; uint32_t bucket_len;
    const char *prefix; uint32_t prefix_len;
    const char *access_key; uint32_t access_key_len;
    const char *secret_key; uint32_t secret_key_len;
    const char *session_token; uint32_t session_token_len;
    const char *password; uint32_t password_len;
    const char *recovery_password; uint32_t recovery_password_len;
    const char *state_path; uint32_t state_path_len;
    const char *source_path; uint32_t source_path_len;
    const char *restore_path; uint32_t restore_path_len;
    const char *snapshot_id; uint32_t snapshot_id_len;
    const char *workspace_path; uint32_t workspace_path_len;
} restic_s3_request;
*/
import "C"

import (
	"crypto/subtle"
	"errors"
	"golang.org/x/sys/unix"
	"os"
)

func validateS3Shape(version uint32, operation int32, fields [14]requestField) int32 {
	if version != 1 || operation < OpInit || operation > OpS3Inspect {
		return StatusInvalidArgument
	}
	caps := [14]uint32{maxHTTPURLBytes, 64, 63, 512, 256, 1024, 16384, maxPasswordLen, maxPasswordLen, maxPathLen, maxPathLen, maxPathLen, idHexLen, maxPathLen}
	for index, field := range fields {
		if field.length > caps[index] {
			return StatusCapExceeded
		}
		if field.length != 0 && !field.present {
			return StatusInvalidArgument
		}
	}
	for _, index := range []int{0, 1, 2, 3, 4, 5, 9} {
		if fields[index].length == 0 {
			return StatusInvalidArgument
		}
	}
	if operation != OpS3Inspect && fields[7].length == 0 {
		return StatusInvalidArgument
	}
	if operation == OpS3Inspect && fields[7].length != 0 {
		return StatusInvalidArgument
	}
	if operation == OpAddRecoveryKey && fields[8].length == 0 {
		return StatusInvalidArgument
	}
	if operation != OpInit && operation != OpAddRecoveryKey && fields[8].length != 0 {
		return StatusInvalidArgument
	}
	if operation == OpSnapshot || operation == OpRestore {
		if fields[10].length == 0 {
			return StatusInvalidArgument
		}
	} else if fields[10].length != 0 {
		return StatusInvalidArgument
	}
	if operation == OpRestore {
		if fields[11].length == 0 || fields[12].length != idHexLen {
			return StatusInvalidArgument
		}
	} else if fields[11].length != 0 || fields[12].length != 0 {
		return StatusInvalidArgument
	}
	if operation == OpSnapshot {
		if fields[13].length == 0 {
			return StatusInvalidArgument
		}
	} else if fields[13].length != 0 {
		return StatusInvalidArgument
	}
	return StatusOk
}

func validateS3Request(request *C.restic_s3_request) (result bridgeRequest, status int32) {
	if request == nil {
		return bridgeRequest{}, StatusInvalidArgument
	}
	fields := [14]requestField{
		{uint32(request.endpoint_len), request.endpoint != nil}, {uint32(request.region_len), request.region != nil},
		{uint32(request.bucket_len), request.bucket != nil}, {uint32(request.prefix_len), request.prefix != nil},
		{uint32(request.access_key_len), request.access_key != nil}, {uint32(request.secret_key_len), request.secret_key != nil},
		{uint32(request.session_token_len), request.session_token != nil}, {uint32(request.password_len), request.password != nil},
		{uint32(request.recovery_password_len), request.recovery_password != nil}, {uint32(request.state_path_len), request.state_path != nil},
		{uint32(request.source_path_len), request.source_path != nil}, {uint32(request.restore_path_len), request.restore_path != nil},
		{uint32(request.snapshot_id_len), request.snapshot_id != nil}, {uint32(request.workspace_path_len), request.workspace_path != nil},
	}
	operation := int32(request.operation)
	if status := validateS3Shape(uint32(request.version), operation, fields); status != StatusOk {
		return bridgeRequest{}, status
	}
	readString := func(pointer *C.char, length C.uint32_t) string {
		owned := copyCBytes(pointer, length)
		defer zeroBytes(owned)
		return string(owned)
	}
	cfg := s3Config{endpoint: readString(request.endpoint, request.endpoint_len), region: readString(request.region, request.region_len),
		bucket: readString(request.bucket, request.bucket_len), prefix: readString(request.prefix, request.prefix_len),
		access: readString(request.access_key, request.access_key_len), secret: readString(request.secret_key, request.secret_key_len), token: readString(request.session_token, request.session_token_len)}
	if cfg.validate() != nil {
		return bridgeRequest{}, StatusInvalidArgument
	}
	result = bridgeRequest{operation: operation, password: copyCBytes(request.password, request.password_len), recoveryPassword: copyCBytes(request.recovery_password, request.recovery_password_len),
		sourcePath: readString(request.source_path, request.source_path_len), restorePath: readString(request.restore_path, request.restore_path_len), snapshotID: readString(request.snapshot_id, request.snapshot_id_len)}
	defer func() {
		if status != StatusOk {
			zeroBytes(result.password)
			zeroBytes(result.recoveryPassword)
		}
	}()
	if len(result.recoveryPassword) != 0 && len(result.password) == len(result.recoveryPassword) && subtle.ConstantTimeCompare(result.password, result.recoveryPassword) == 1 {
		return result, StatusInvalidArgument
	}
	if operation == OpRestore && (!validS3ID(result.snapshotID) || validateRestoreDest(result.restorePath) != nil) {
		return result, StatusInvalidArgument
	}
	if result.sourcePath != "" && validateExistingDir(result.sourcePath) != nil {
		return result, StatusInvalidArgument
	}
	statePath := readString(request.state_path, request.state_path_len)
	owner, err := newS3Owner(statePath, cfg)
	if err != nil {
		if errors.Is(err, errS3OwnerBusy) {
			return result, StatusBusy
		}
		if errors.Is(err, errS3OwnerIO) {
			return result, StatusFailed
		}
		return result, StatusInvalidArgument
	}
	result.remote = &s3RemoteRequest{cfg: cfg, owner: owner}
	if err := pinDataPaths(&result, []*os.File{owner.directory}); err != nil {
		return result, rejectBridgeCall(&result, StatusInvalidArgument)
	}
	if operation == OpSnapshot {
		workspace := readString(request.workspace_path, request.workspace_path_len)
		if err := pinMetadataWorkspace(&result, workspace); err != nil {
			if errors.Is(err, unix.EWOULDBLOCK) || errors.Is(err, unix.EAGAIN) {
				return result, rejectBridgeCall(&result, StatusBusy)
			}
			return result, rejectBridgeCall(&result, StatusInvalidArgument)
		}
	}
	return result, StatusOk
}

//export restic_bridge_s3_start
func restic_bridge_s3_start(request *C.restic_s3_request, outID *C.uint64_t) C.int32_t {
	if outID == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	*outID = 0
	req, status := validateS3Request(request)
	if status != StatusOk {
		return C.int32_t(status)
	}
	id, status := startBridgeCall(req)
	if status == StatusOk {
		*outID = C.uint64_t(id)
	}
	return C.int32_t(status)
}

//export restic_bridge_s3_snapshot_v1_start
func restic_bridge_s3_snapshot_v1_start(request *C.restic_s3_request, policy *C.char, policyLen C.uint32_t, outID *C.uint64_t) C.int32_t {
	if outID == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	*outID = 0
	if request == nil || request.operation != C.int32_t(OpSnapshot) {
		return C.int32_t(StatusInvalidArgument)
	}
	// Refuse malformed policy before owner/root admission or any worker/network
	// access; the selection cannot change repository or credential authority.
	selection, status := parseCSnapshotSelection(policy, policyLen)
	if status != StatusOk {
		return C.int32_t(status)
	}
	req, status := validateS3Request(request)
	if status != StatusOk {
		return C.int32_t(status)
	}
	req.snapshotSelection = selection
	id, status := startBridgeCall(req)
	if status == StatusOk {
		*outID = C.uint64_t(id)
	}
	return C.int32_t(status)
}
