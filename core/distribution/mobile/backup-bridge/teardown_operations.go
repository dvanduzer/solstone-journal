// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include "abi_legacy.h"

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
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"sort"
	"sync"

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
	"github.com/restic/restic/internal/ui/progress"
	"golang.org/x/sys/unix"
)

const (
	OpTeardown int32 = 11
)

var (
	teardownMu       sync.Mutex
	teardownAdmitted = make(map[string][]restic.ID)
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
	if count > maxSnapshotCount || uint32(len(ids)) > maxSnapshotCount {
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
	outID *C.uint64_t,
) C.int32_t {
	if outID == nil || options == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	*outID = 0

	if (local == nil && remote == nil) || (local != nil && remote != nil) {
		return C.int32_t(StatusInvalidArgument)
	}
	if !validateTeardownOptions(options) {
		return C.int32_t(StatusInvalidArgument)
	}

	admitted, ok := parseAdmittedSnapshotIDs(ids, count)
	if !ok {
		return C.int32_t(StatusInvalidArgument)
	}

	var req bridgeRequest

	if local != nil {
		if stateFD != -1 {
			return C.int32_t(StatusInvalidArgument)
		}
		if local.operation != C.int32_t(OpTeardown) {
			return C.int32_t(StatusInvalidArgument)
		}
		if local.snapshot_id_len != 0 || local.recovery_password_len != 0 {
			return C.int32_t(StatusInvalidArgument)
		}
		if local.source_path_len != 0 || local.restore_path_len != 0 {
			return C.int32_t(StatusInvalidArgument)
		}

		repoPathBytes := copyCBytes(local.repo_path, local.repo_path_len)
		repoPath := string(repoPathBytes)
		if len(repoPath) == 0 || !filepath.IsAbs(repoPath) {
			return C.int32_t(StatusInvalidArgument)
		}
		fi, err := os.Stat(repoPath)
		if err != nil || !fi.IsDir() {
			return C.int32_t(StatusInvalidArgument)
		}

		req = bridgeRequest{
			operation: OpTeardown,
			password:  copyCBytes(local.password, local.password_len),
			repoPath:  repoPath,
		}
		if err := pinRequestPaths(&req); err != nil {
			zeroBytes(req.password)
			return C.int32_t(StatusInvalidArgument)
		}
	} else {
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
			{remote.endpoint, remote.endpoint_len},
			{remote.region, remote.region_len},
			{remote.bucket, remote.bucket_len},
			{remote.prefix, remote.prefix_len},
			{remote.access_key, remote.access_key_len},
			{remote.secret_key, remote.secret_key_len},
			{remote.session_token, remote.session_token_len},
			{remote.password, remote.password_len},
			{remote.state_path, remote.state_path_len},
		}
		caps := [9]uint32{maxHTTPURLBytes, 64, 63, 512, 256, 1024, 16384, maxPasswordLen, maxPathLen}
		for i, f := range fields {
			if status := validateField(f, caps[i]); status != StatusOk {
				return C.int32_t(status)
			}
		}

		cfg := s3Config{
			endpoint: readString(remote.endpoint, remote.endpoint_len),
			region:   readString(remote.region, remote.region_len),
			bucket:   readString(remote.bucket, remote.bucket_len),
			prefix:   readString(remote.prefix, remote.prefix_len),
			access:   readString(remote.access_key, remote.access_key_len),
			secret:   readBytes(remote.secret_key, remote.secret_key_len),
			token:    readString(remote.session_token, remote.session_token_len),
		}

		statePath := readString(remote.state_path, remote.state_path_len)
		owner, err := newS3OwnerExpected(statePath, cfg, nil, stateIdentity)
		if err != nil {
			if errors.Is(err, errS3OwnerBusy) {
				return C.int32_t(StatusBusy)
			}
			if errors.Is(err, errS3OwnerIO) {
				return C.int32_t(StatusFailed)
			}
			return C.int32_t(StatusInvalidArgument)
		}
		if !owner.isUnsealed() {
			_ = owner.Close()
			return C.int32_t(StatusBusy)
		}

		req.operation = OpTeardown
		req.password = copyCBytes(remote.password, remote.password_len)
		req.remote = &s3RemoteRequest{cfg: cfg, owner: owner}

		if err := owner.beginTeardown(cfg, req.password); err != nil {
			_ = owner.Close()
			zeroBytes(req.password)
			if errors.Is(err, errS3OwnerBusy) {
				return C.int32_t(StatusBusy)
			}
			if errors.Is(err, errS3OwnerPending) || errors.Is(err, errS3OwnerIO) {
				return C.int32_t(StatusFailed)
			}
			return C.int32_t(StatusInvalidArgument)
		}

		if err := pinDataPaths(&req, []*os.File{owner.directory}); err != nil {
			return C.int32_t(rejectBridgeCall(&req, StatusInvalidArgument))
		}
	}

	var nonce [16]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		return C.int32_t(rejectBridgeCall(&req, StatusFailed))
	}
	token := hex.EncodeToString(nonce[:])
	req.sourceName = token

	teardownMu.Lock()
	teardownAdmitted[token] = admitted
	teardownMu.Unlock()

	id, status := startBridgeCall(req)
	if status != StatusOk {
		teardownMu.Lock()
		delete(teardownAdmitted, token)
		teardownMu.Unlock()
		return C.int32_t(status)
	}

	*outID = C.uint64_t(id)
	return C.int32_t(StatusOk)
}

//export restic_bridge_teardown_v1_wait
func restic_bridge_teardown_v1_wait(id C.uint64_t, out *C.restic_result) C.int32_t {
	return restic_bridge_wait(id, out)
}

//export restic_bridge_s3_retire_v1
func restic_bridge_s3_retire_v1(stateFD C.int) C.int32_t {
	fd := int(stateFD)
	if err := unix.Flock(fd, unix.LOCK_EX|unix.LOCK_NB); err != nil {
		if errors.Is(err, unix.EWOULDBLOCK) || errors.Is(err, unix.EAGAIN) {
			return C.int32_t(StatusBusy)
		}
		return C.int32_t(StatusFailed)
	}

	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return C.int32_t(StatusFailed)
	}
	if (stat.Mode&unix.S_IFMT) != unix.S_IFDIR || int(stat.Uid) != os.Geteuid() {
		return C.int32_t(StatusFailed)
	}

	fileFD, err := unix.Openat(fd, s3OwnerStateName, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC|unix.O_NONBLOCK, 0)
	if errors.Is(err, unix.ENOENT) {
		if err := syncDirectory(fd); err != nil {
			return C.int32_t(StatusFailed)
		}
		return C.int32_t(StatusOk)
	}
	if err != nil {
		return C.int32_t(StatusFailed)
	}
	file := os.NewFile(uintptr(fileFD), "S3 owner state")
	defer file.Close()
	raw, err := io.ReadAll(io.LimitReader(file, maxS3OwnerBytes+1))
	defer zeroBytes(raw)
	if err != nil || len(raw) > maxS3OwnerBytes {
		return C.int32_t(StatusFailed)
	}
	var record s3OwnerRecord
	if err := json.Unmarshal(raw, &record); err != nil {
		return C.int32_t(StatusFailed)
	}
	if record.Active != nil {
		return C.int32_t(StatusFailed)
	}
	_ = file.Close()

	if err := unix.Unlinkat(fd, s3OwnerStateName, 0); err != nil && !errors.Is(err, unix.ENOENT) {
		return C.int32_t(StatusFailed)
	}
	if err := syncDirectory(fd); err != nil {
		return C.int32_t(StatusFailed)
	}
	return C.int32_t(StatusOk)
}

func runTeardownWorker(h *callHandle, req bridgeRequest) {
	req.owned.retain(&req)
	defer func() {
		if recover() == nil {
			return
		}
		h.cancel()
		if req.owned != nil {
			req.owned.markFault()
			req.owned.blockIfFault(&req)
		}
		h.mu.Lock()
		h.state = handleRunning
		h.mu.Unlock()
	}()

	teardownMu.Lock()
	admitted, ok := teardownAdmitted[req.sourceName]
	if ok {
		delete(teardownAdmitted, req.sourceName)
	}
	teardownMu.Unlock()

	result := bridgeResult{status: StatusFailed, diagnostic: DiagInternal}
	wrote := false

	if ok {
		ctx := withTeardownScope(h.ctx)
		result, wrote = executeTeardown(ctx, h, &req, admitted)
	}

	if req.remote != nil {
		owner := req.remote.owner
		req.owned.blockIfFault(&req)
		drainErr := owner.drainPrimary()
		req.owned.blockIfFault(&req)
		var completeErr error
		if result.status == StatusOk && drainErr == nil {
			_, completeErr = owner.complete()
			req.owned.blockIfFault(&req)
		}
		cleanupErr := closeS3RequestPaths(&req)
		req.owned.blockIfFault(&req)

		if result.status == StatusOk && (drainErr != nil || completeErr != nil || cleanupErr != nil) {
			result = bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}
		}
	} else {
		req.owned.blockIfFault(&req)
		cleanupErr := closeRequestPaths(&req)
		req.owned.blockIfFault(&req)
		if result.status == StatusOk && cleanupErr != nil {
			result = bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}
		}
	}

	finishCall(h, result, wrote)
}

func executeTeardown(ctx context.Context, h *callHandle, req *bridgeRequest, admitted []restic.ID) (bridgeResult, bool) {
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

	popts := repository.PruneOptions{}
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
