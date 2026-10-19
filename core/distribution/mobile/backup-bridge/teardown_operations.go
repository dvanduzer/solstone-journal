// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>

#ifndef SOLSTONE_RESTIC_LEGACY_TYPES
#define SOLSTONE_RESTIC_LEGACY_TYPES
typedef struct {
    int32_t operation;
    const char *password;
    uint32_t password_len;
    const char *recovery_password;
    uint32_t recovery_password_len;
    const char *repo_path;
    uint32_t repo_path_len;
    const char *source_path;
    uint32_t source_path_len;
    const char *restore_path;
    uint32_t restore_path_len;
    const char *snapshot_id;
    uint32_t snapshot_id_len;
} restic_request;

typedef struct {
    int32_t status;
    char key_id[65];
    char snapshot_id[65];
    uint32_t snapshot_count;
    char snapshot_ids[4096][65];
    char diagnostic[256];
} restic_result;
#endif

#ifndef SOLSTONE_RESTIC_OPERATION_OPTIONS_V1
#define SOLSTONE_RESTIC_OPERATION_OPTIONS_V1
typedef struct {
    uint32_t version;
    uint32_t check_read_mode;
    uint64_t retry_lock_ms;
    uint32_t group;
    uint32_t groups;
    uint32_t reserved1;
    uint32_t reserved2;
} restic_operation_options_v1;
#endif

#ifndef SOLSTONE_RESTIC_S3_REQUEST
#define SOLSTONE_RESTIC_S3_REQUEST
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
#endif
*/
import "C"

import (
	"context"
	"errors"
	"os"
	"sort"

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
	"github.com/restic/restic/internal/ui/progress"
)

const (
	OpTeardown int32 = 11
)

func validateTeardownOptions(options *C.restic_operation_options_v1) bool {
	if options == nil {
		return false
	}
	if options.version != 1 && options.version != 2 {
		return false
	}
	if options.reserved1 != 0 || options.reserved2 != 0 {
		return false
	}
	if options.check_read_mode != 0 || options.retry_lock_ms != 0 || options.group != 0 || options.groups != 0 {
		return false
	}
	return true
}

func validateTeardownShape(operation int32, optionsReserved1, optionsReserved2, optionsVersion uint32, snapshotIDLen, recoveryPasswordLen uint32, count uint32, ids []string) int32 {
	if operation != OpTeardown {
		return StatusInvalidArgument
	}
	if optionsVersion != 1 && optionsVersion != 2 {
		return StatusInvalidArgument
	}
	if optionsReserved1 != 0 || optionsReserved2 != 0 {
		return StatusInvalidArgument
	}
	if snapshotIDLen != 0 || recoveryPasswordLen != 0 {
		return StatusInvalidArgument
	}
	if count > maxSnapshotCount || uint32(len(ids)) != count {
		return StatusInvalidArgument
	}
	seen := make(map[string]bool, len(ids))
	for _, id := range ids {
		if len(id) != 64 {
			return StatusInvalidArgument
		}
		for _, ch := range id {
			if !((ch >= '0' && ch <= '9') || (ch >= 'a' && ch <= 'f')) {
				return StatusInvalidArgument
			}
		}
		if seen[id] {
			return StatusInvalidArgument
		}
		seen[id] = true
	}
	return StatusOk
}

func parseAdmittedSnapshotIDs(ids *C.char, count C.uint32_t) ([]restic.ID, bool) {
	if count > maxSnapshotCount {
		return nil, false
	}
	if count == 0 {
		return nil, true
	}
	if ids == nil {
		return nil, false
	}
	idsBytes := copyCBytes(ids, count*64)
	if len(idsBytes) != int(count*64) {
		return nil, false
	}
	seen := make(map[string]bool, count)
	result := make([]restic.ID, 0, count)
	for i := 0; i < int(count); i++ {
		idStr := string(idsBytes[i*64 : (i+1)*64])
		for _, ch := range idStr {
			if !((ch >= '0' && ch <= '9') || (ch >= 'a' && ch <= 'f')) {
				return nil, false
			}
		}
		if seen[idStr] {
			return nil, false
		}
		seen[idStr] = true
		id, err := restic.ParseID(idStr)
		if err != nil {
			return nil, false
		}
		result = append(result, id)
	}
	return result, true
}

//export restic_bridge_teardown_v1_start
func restic_bridge_teardown_v1_start(
	local *C.restic_request,
	remote *C.restic_s3_request,
	options *C.restic_operation_options_v1,
	stateFD C.int,
	ids *C.char,
	count C.uint32_t,
	admissionJSON *C.char,
	admissionLen C.uint32_t,
	outID *C.uint64_t,
) (returned C.int32_t) {
	if outID == nil || options == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	*outID = 0
	entry, status := reserveOwnedCall()
	if status != StatusOk {
		return C.int32_t(status)
	}
	defer func() { returned = C.int32_t(entry.finishAdmission(int32(returned))) }()

	// This admission is only for the supported iOS own-S3 teardown.
	if local != nil || remote == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	if !validateTeardownOptions(options) {
		return C.int32_t(StatusInvalidArgument)
	}

	admitted, ok := parseAdmittedSnapshotIDs(ids, count)
	if !ok {
		return C.int32_t(StatusInvalidArgument)
	}

	if admissionJSON == nil || admissionLen == 0 || admissionLen > 4096 {
		return C.int32_t(StatusInvalidArgument)
	}
	var admission s3TeardownAdmission
	if strictTeardownJSON(copyCBytes(admissionJSON, admissionLen), &admission) != nil {
		return C.int32_t(StatusInvalidArgument)
	}
	req := bridgeRequest{owned: entry}
	entry.retain(&req)

	if remote.version != 1 || remote.operation != C.int32_t(OpTeardown) {
		return C.int32_t(StatusInvalidArgument)
	}
	if remote.snapshot_id_len != 0 || remote.recovery_password_len != 0 {
		return C.int32_t(StatusInvalidArgument)
	}
	if remote.source_path_len != 0 || remote.restore_path_len != 0 || remote.workspace_path_len != 0 {
		return C.int32_t(StatusInvalidArgument)
	}

	stateIdentity, status := readBorrowedDirectoryIdentity(int(stateFD))
	if status != StatusOk {
		return C.int32_t(status)
	}

	fields := [9]requestField{
		{uint32(remote.endpoint_len), remote.endpoint != nil},
		{uint32(remote.region_len), remote.region != nil},
		{uint32(remote.bucket_len), remote.bucket != nil},
		{uint32(remote.prefix_len), remote.prefix != nil},
		{uint32(remote.access_key_len), remote.access_key != nil},
		{uint32(remote.secret_key_len), remote.secret_key != nil},
		{uint32(remote.session_token_len), remote.session_token != nil},
		{uint32(remote.password_len), remote.password != nil},
		{uint32(remote.state_path_len), remote.state_path != nil},
	}
	caps := [9]uint32{maxHTTPURLBytes, 64, 63, 512, 256, 1024, 16384, maxPasswordLen, maxPathLen}
	for i, f := range fields {
		if f.length > caps[i] {
			return C.int32_t(StatusCapExceeded)
		}
		if (f.length > 0 && !f.present) || (i != 6 && f.length == 0) {
			return C.int32_t(StatusInvalidArgument)
		}
	}
	readString := func(pointer *C.char, length C.uint32_t) string {
		owned := copyCBytes(pointer, length)
		defer zeroBytes(owned)
		return string(owned)
	}

	cfg := s3Config{
		endpoint: readString(remote.endpoint, remote.endpoint_len),
		region:   readString(remote.region, remote.region_len),
		bucket:   readString(remote.bucket, remote.bucket_len),
		prefix:   readString(remote.prefix, remote.prefix_len),
		access:   readString(remote.access_key, remote.access_key_len),
		secret:   readString(remote.secret_key, remote.secret_key_len),
		token:    readString(remote.session_token, remote.session_token_len),
	}
	if cfg.validate() != nil {
		return C.int32_t(StatusInvalidArgument)
	}

	destination, _ := s3DestinationFor(cfg)
	admittedStrings := make([]string, 0, len(admitted))
	for _, id := range admitted {
		admittedStrings = append(admittedStrings, id.String())
	}
	if _, err := admission.scope(destination, admittedStrings); err != nil {
		return C.int32_t(StatusInvalidArgument)
	}
	statePath := readString(remote.state_path, remote.state_path_len)
	owner, err := newS3OwnerExpected(statePath, cfg, entry, stateIdentity)
	if err != nil {
		if errors.Is(err, errS3OwnerBusy) {
			return C.int32_t(StatusBusy)
		}
		if errors.Is(err, errS3OwnerIO) {
			return C.int32_t(StatusFailed)
		}
		return C.int32_t(StatusInvalidArgument)
	}
	pendingScope := ""
	if admission.Expected.Operation == OpTeardown {
		pendingScope, _ = admission.scope(destination, admittedStrings)
	}
	actual, stateFile, observeErr := observeTeardownState(int(owner.directory.Fd()), destination, pendingScope)
	if stateFile != nil {
		observeErr = errors.Join(observeErr, stateFile.Close())
	}
	if observeErr != nil || actual != admission.Expected || owner.recheckCustody("") != nil {
		return C.int32_t(StatusInvalidArgument)
	}
	if !owner.isUnsealed() {
		_ = owner.Close()
		return C.int32_t(StatusBusy)
	}

	req.teardownAdmission = &admission
	req.operation = OpTeardown
	req.password = copyCBytes(remote.password, remote.password_len)
	req.remote = &s3RemoteRequest{cfg: cfg, owner: owner}

	if err := pinDataPaths(&req, []*os.File{owner.directory}); err != nil {
		return C.int32_t(rejectBridgeCall(&req, StatusInvalidArgument))
	}
	for _, id := range admitted {
		req.teardownIDs = append(req.teardownIDs, id.String())
	}
	id, status := startBridgeCall(req)
	if status != StatusOk {
		return C.int32_t(status)
	}

	*outID = C.uint64_t(id)
	return C.int32_t(StatusOk)
}

//export restic_bridge_teardown_v1_wait
func restic_bridge_teardown_v1_wait(id C.uint64_t, out *C.restic_result) C.int32_t {
	return restic_bridge_wait(id, out)
}

func executeTeardown(ctx context.Context, h *callHandle, req *bridgeRequest, admitted []restic.ID) (bridgeResult, bool) {
	if req.remote == nil {
		return bridgeResult{status: StatusInvalidArgument, diagnostic: DiagInvalidArgument}, false
	}
	if h.ctx.Err() != nil {
		return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, false
	}

	pw, clearPw := passwordString(req.password)
	defer clearPw()

	be, err := operationBackend(ctx, *req, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}, false
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, false
	}

	_, err = safeScanKey(ctx, repo, pw)
	if err != nil {
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, false
		}
		return keyRefusal(ctx, be, err), false
	}

	if req.teardownAdmission == nil || repo.Config().ID != req.teardownAdmission.RepositoryID {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, false
	}
	unlock, lockCtx, err := lockRepoForRequest(ctx, *req, repo, true)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagLocked}, false
	}
	defer unlock()

	teardownCtx := withTeardownScope(lockCtx)

	if err := repo.LoadIndex(teardownCtx, &progress.NoopPrinter{}); err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, false
	}

	if teardownCtx.Err() != nil {
		return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, false
	}

	wrote := false

	// 1. List restic.SnapshotFile. Forget only admitted ids that are present.
	presentMap := make(map[restic.ID]bool)
	err = repo.List(teardownCtx, restic.SnapshotFile, func(id restic.ID, _ int64) error {
		presentMap[id] = true
		return nil
	})
	if err != nil {
		if teardownCtx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, false
		}
		return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, false
	}

	admittedSet := make(map[restic.ID]bool, len(admitted))
	for _, id := range admitted {
		admittedSet[id] = true
	}
	for id := range presentMap {
		if !admittedSet[id] {
			return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, false
		}
	}
	for _, id := range admitted {
		if !presentMap[id] {
			continue
		}
		if teardownCtx.Err() != nil {
			status := StatusCancelled
			diag := DiagCancelled
			if wrote {
				status = StatusCancelledUncertain
				diag = DiagCancelledUncertain
			}
			return bridgeResult{status: int32(status), diagnostic: diag}, wrote
		}
		wrote = true
		if err := repo.RemoveUnpacked(teardownCtx, restic.WriteableSnapshotFile, id); err != nil {
			return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, wrote
		}
	}

	// 2 & 3. PlanPrune with ignoreSnapshots equal to admitted ids whose files are absent.
	ignoreSnapshots := restic.NewIDSet()
	for _, id := range admitted {
		ignoreSnapshots.Insert(id)
	}

	getUsedBlobs := func(ctx context.Context, r restic.Repository, usedBlobs restic.FindBlobSet) error {
		var snapshotTrees restic.IDs
		err := data.ForAllSnapshots(ctx, r, r, ignoreSnapshots, func(id restic.ID, sn *data.Snapshot, err error) error {
			if err != nil {
				return err
			}
			if sn.Tree == nil {
				return errors.New("missing snapshot tree")
			}
			snapshotTrees = append(snapshotTrees, *sn.Tree)
			return nil
		})
		if err != nil {
			return err
		}
		counter := (&progress.NoopPrinter{}).NewCounter("snapshots")
		return data.FindUsedBlobs(ctx, r, snapshotTrees, usedBlobs, counter)
	}

	if teardownCtx.Err() != nil {
		status := StatusCancelled
		diag := DiagCancelled
		if wrote {
			status = StatusCancelledUncertain
			diag = DiagCancelledUncertain
		}
		return bridgeResult{status: int32(status), diagnostic: diag}, wrote
	}

	popts := repository.PruneOptions{
		// Restic's default five percent of the post-repack total is used/19.
		MaxUnusedBytes: func(used uint64) uint64 { return used / 19 },
		MaxRepackBytes: ^uint64(0),
	}
	plan, err := repository.PlanPrune(teardownCtx, popts, repo, getUsedBlobs, &progress.NoopPrinter{})
	if err != nil {
		if teardownCtx.Err() != nil {
			status := StatusCancelled
			diag := DiagCancelled
			if wrote {
				status = StatusCancelledUncertain
				diag = DiagCancelledUncertain
			}
			return bridgeResult{status: int32(status), diagnostic: diag}, wrote
		}
		return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, wrote
	}

	if teardownCtx.Err() != nil {
		status := StatusCancelled
		diag := DiagCancelled
		if wrote {
			status = StatusCancelledUncertain
			diag = DiagCancelledUncertain
		}
		return bridgeResult{status: int32(status), diagnostic: diag}, wrote
	}

	// 4. plan.Execute
	wrote = true
	if err := plan.Execute(teardownCtx, &progress.NoopPrinter{}); err != nil {
		if teardownCtx.Err() != nil {
			return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, wrote
		}
		return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, wrote
	}

	if req.remote != nil && req.remote.owner != nil && req.remote.owner.hasWriteFailed() {
		return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, wrote
	}

	var remainingIDs []string
	err = repo.List(teardownCtx, restic.SnapshotFile, func(id restic.ID, _ int64) error {
		remainingIDs = append(remainingIDs, id.String())
		return nil
	})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, wrote
	}
	sort.Strings(remainingIDs)
	result := clipSnapshotIDs(remainingIDs)
	if result.status == StatusCapExceeded {
		return bridgeResult{status: StatusFailed, diagnostic: DiagCapExceeded}, wrote
	}

	return result, wrote
}
