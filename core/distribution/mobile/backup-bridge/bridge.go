// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>

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
*/
import "C"

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"unsafe"
)

// Handle states.
const (
	handleRunning int32 = 0
	handleStopped int32 = 1
	handleFreed   int32 = 2
)

// Internal call handle tracking worker lifecycle and owned memory.
type callHandle struct {
	id                   uint64
	ctx                  context.Context
	cancel               context.CancelFunc
	doneCh               chan struct{}
	result               bridgeResult
	passwords            [][]byte
	state                int32
	cancelRequested      bool
	publicationCommitted bool
	mu                   sync.Mutex
}

var (
	handlesMu sync.Mutex
	handles   = make(map[uint64]*callHandle)
	nextID    atomic.Uint64
)

func init() {
	// IDs start at 1; 0 is permanently invalid.
	nextID.Store(0)
}

// testGate hook for deterministic unit and integration test synchronization.
// Nil in production.
var testGate func(stage string, ctx context.Context) error

func zeroBytes(b []byte) {
	for i := range b {
		b[i] = 0
	}
}

func copyCBytes(ptr *C.char, length C.uint32_t) []byte {
	if ptr == nil || length == 0 {
		return nil
	}
	buf := make([]byte, int(length))
	src := unsafe.Slice((*byte)(unsafe.Pointer(ptr)), int(length))
	copy(buf, src)
	return buf
}

func copyCString(dst []C.char, s string) {
	if len(dst) == 0 {
		return
	}
	for i := range dst {
		dst[i] = 0
	}
	max := len(dst) - 1
	if len(s) < max {
		max = len(s)
	}
	for i := 0; i < max; i++ {
		dst[i] = C.char(s[i])
	}
	dst[max] = 0
}

// validateRequest validates C request lengths and path invariants.
func validateRequest(req *C.restic_request) (bridgeRequest, int32) {
	if req == nil {
		return bridgeRequest{}, StatusInvalidArgument
	}

	op := int32(req.operation)
	if op < OpInit || op > OpCheck {
		return bridgeRequest{}, StatusInvalidArgument
	}

	shape := requestShape{
		operation: op,
		fields: [6]requestField{
			{uint32(req.password_len), req.password != nil},
			{uint32(req.recovery_password_len), req.recovery_password != nil},
			{uint32(req.repo_path_len), req.repo_path != nil},
			{uint32(req.source_path_len), req.source_path != nil},
			{uint32(req.restore_path_len), req.restore_path != nil},
			{uint32(req.snapshot_id_len), req.snapshot_id != nil},
		},
	}
	if status := validateShape(shape); status != StatusOk {
		return bridgeRequest{}, status
	}

	repoBytes := copyCBytes(req.repo_path, req.repo_path_len)
	repoPath := string(repoBytes)
	if err := sanitizePath(repoPath); err != nil {
		return bridgeRequest{}, StatusInvalidArgument
	}
	if err := checkNoSymlinks(repoPath); err != nil {
		return bridgeRequest{}, StatusInvalidArgument
	}

	// Callable paths must already exist. This lets the bridge pin each directory
	// by descriptor before the asynchronous worker starts, so later renames or
	// symlink substitutions cannot redirect repository access.
	if err := validateExistingDir(repoPath); err != nil {
		return bridgeRequest{}, StatusInvalidArgument
	}

	var sourcePath string
	if op == OpSnapshot || op == OpRestore {
		srcBytes := copyCBytes(req.source_path, req.source_path_len)
		sourcePath = string(srcBytes)
		if err := validateExistingDir(sourcePath); err != nil {
			return bridgeRequest{}, StatusInvalidArgument
		}
		if pathsOverlap(repoPath, sourcePath) {
			return bridgeRequest{}, StatusInvalidArgument
		}
	}

	var restorePath string
	if op == OpRestore {
		rstBytes := copyCBytes(req.restore_path, req.restore_path_len)
		restorePath = string(rstBytes)
		if err := validateRestoreDest(restorePath); err != nil {
			return bridgeRequest{}, StatusInvalidArgument
		}
		if pathsOverlap(repoPath, restorePath) || pathsOverlap(sourcePath, restorePath) {
			return bridgeRequest{}, StatusInvalidArgument
		}
	}

	var snapshotID string
	if op == OpRestore {
		snBytes := copyCBytes(req.snapshot_id, req.snapshot_id_len)
		snapshotID = string(snBytes)
	}

	breq := bridgeRequest{
		operation:        op,
		password:         copyCBytes(req.password, req.password_len),
		recoveryPassword: copyCBytes(req.recovery_password, req.recovery_password_len),
		repoPath:         repoPath,
		sourcePath:       sourcePath,
		restorePath:      restorePath,
		snapshotID:       snapshotID,
	}
	if err := pinRequestPaths(&breq); err != nil {
		closeRequestPaths(&breq)
		return bridgeRequest{}, StatusInvalidArgument
	}

	return breq, StatusOk
}

func allocateID(counter *atomic.Uint64) (uint64, int32) {
	for {
		previous := counter.Load()
		if previous == ^uint64(0) {
			return 0, StatusCapExceeded
		}
		if counter.CompareAndSwap(previous, previous+1) {
			return previous + 1, StatusOk
		}
	}
}

// startBridgeCall allocates a handle, initializes context, and launches the worker.
func startBridgeCall(req bridgeRequest) (uint64, int32) {
	id, status := allocateID(&nextID)
	if status != StatusOk {
		closeRequestPaths(&req)
		zeroBytes(req.password)
		zeroBytes(req.recoveryPassword)
		return 0, status
	}

	ctx, cancel := context.WithCancel(context.Background())
	h := &callHandle{
		id:        id,
		ctx:       ctx,
		cancel:    cancel,
		doneCh:    make(chan struct{}),
		state:     handleRunning,
		passwords: [][]byte{req.password, req.recoveryPassword},
	}

	handlesMu.Lock()
	running := 0
	for _, active := range handles {
		active.mu.Lock()
		if active.state == handleRunning {
			running++
		}
		active.mu.Unlock()
	}
	if running >= 4 || len(handles) >= 16 {
		handlesMu.Unlock()
		cancel()
		closeRequestPaths(&req)
		zeroBytes(req.password)
		zeroBytes(req.recoveryPassword)
		return 0, StatusBusy
	}
	handles[id] = h
	handlesMu.Unlock()

	go runWorker(h, req)

	return id, StatusOk
}

// runWorker executes the requested operation under panic and cancellation protection.
// finishCall publishes once, after native cleanup, in the same critical section as cancellation.
func finishCall(h *callHandle, result bridgeResult, wrote bool) {
	for _, password := range h.passwords {
		zeroBytes(password)
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.cancelRequested || h.ctx.Err() != nil {
		result = bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}
		if wrote {
			result = bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}
		}
	}
	h.result = result
	h.state = handleStopped
	h.cancel()
	close(h.doneCh)
}

// publishRestoreForHandle is the cancellation/publication linearization point.
// Whichever takes h.mu first wins: cancellation prevents publication, while a
// completed rename prevents a later cancellation from rewriting the result.
func publishRestoreForHandle(h *callHandle, req *bridgeRequest) error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.cancelRequested || h.ctx.Err() != nil {
		return context.Canceled
	}
	err := promoteRestoreStage(h.ctx, req)
	if err == nil || errors.Is(err, errRestoreCommittedUncertain) {
		h.publicationCommitted = true
	}
	return err
}

func runWorker(h *callHandle, req bridgeRequest) {
	wrote := false
	result := bridgeResult{status: StatusInternal, diagnostic: DiagInternal}
	defer func() {
		cleanupErr := closeRequestPaths(&req)
		if recover() != nil {
			result = bridgeResult{status: StatusInternal, diagnostic: DiagInternal}
		} else if cleanupErr != nil {
			if result.status == StatusOk && req.operation == OpRestore {
				// Publication may already be visible even though the final staging
				// census or parent sync could not be confirmed.
				result = bridgeResult{status: StatusCommittedUncertain, diagnostic: DiagCommittedUncertain}
			} else if result.status != StatusCommittedUncertain {
				result = bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}
			}
		}
		finishCall(h, result, wrote)
	}()
	if testGate != nil {
		if err := testGate("before-restic", h.ctx); err != nil {
			result = bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}
			return
		}
	}
	if h.ctx.Err() != nil {
		result = bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}
		return
	}
	markWrite := func() { wrote = true }
	switch req.operation {
	case OpInit:
		result, _ = executeInit(h.ctx, req, markWrite)
	case OpOpen:
		result, _ = executeOpen(h.ctx, req)
	case OpAddRecoveryKey:
		result, _ = executeAddRecoveryKey(h.ctx, req, markWrite)
	case OpSnapshot:
		result, _ = executeSnapshot(h.ctx, req, markWrite)
	case OpListSnapshots:
		result, _ = executeListSnapshots(h.ctx, req)
	case OpRestore:
		result, _ = executeRestoreWithPublisher(h.ctx, &req, markWrite, func(_ context.Context, req *bridgeRequest) error {
			return publishRestoreForHandle(h, req)
		})
	case OpCheck:
		result, _ = executeCheck(h.ctx, req)
	default:
		result = bridgeResult{status: StatusInvalidArgument, diagnostic: DiagInvalidArgument}
	}
}

//export restic_bridge_start
func restic_bridge_start(req *C.restic_request, outID *C.uint64_t) C.int32_t {
	if outID == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	*outID = 0
	if req == nil {
		return C.int32_t(StatusInvalidArgument)
	}

	breq, status := validateRequest(req)
	if status != StatusOk {
		return C.int32_t(status)
	}

	id, status := startBridgeCall(breq)
	if status == StatusOk {
		*outID = C.uint64_t(id)
	}
	return C.int32_t(status)
}

func cancelCall(id uint64) int32 {
	handlesMu.Lock()
	h := handles[id]
	handlesMu.Unlock()
	if h == nil {
		return StatusInvalidHandle
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.state == handleFreed {
		return StatusInvalidHandle
	}
	// Cancellation after publication cannot rewrite an already terminal result.
	if h.state == handleRunning && !h.publicationCommitted {
		h.cancelRequested = true
		h.cancel()
	}
	return StatusOk
}

//export restic_bridge_cancel
func restic_bridge_cancel(id C.uint64_t) C.int32_t { return C.int32_t(cancelCall(uint64(id))) }

//export restic_bridge_wait
func restic_bridge_wait(id C.uint64_t, out *C.restic_result) C.int32_t {
	if out == nil {
		return C.int32_t(StatusInvalidArgument)
	}

	handlesMu.Lock()
	h, ok := handles[uint64(id)]
	handlesMu.Unlock()

	if !ok || h == nil {
		return C.int32_t(StatusInvalidHandle)
	}

	h.mu.Lock()
	state := h.state
	h.mu.Unlock()

	if state == handleFreed {
		return C.int32_t(StatusInvalidHandle)
	}

	// Block until worker finishes and cleanup completes
	<-h.doneCh

	h.mu.Lock()
	res := h.result
	h.mu.Unlock()

	// Populate caller-supplied result
	out.status = C.int32_t(res.status)
	out.snapshot_count = C.uint32_t(res.snapshotCount)

	copyCString(out.key_id[:], res.keyID)
	copyCString(out.snapshot_id[:], res.snapshotID)
	copyCString(out.diagnostic[:], res.diagnostic)

	for i := range out.snapshot_ids {
		out.snapshot_ids[i][0] = 0
	}
	if res.status != StatusCapExceeded {
		for i := 0; i < int(res.snapshotCount) && i < maxSnapshotCount; i++ {
			copyCString(out.snapshot_ids[i][:], res.snapshotIDs[i])
		}
	}

	return C.int32_t(StatusOk)
}

func freeCall(id uint64) int32 {
	handlesMu.Lock()
	h, ok := handles[id]
	if !ok || h == nil {
		handlesMu.Unlock()
		return StatusInvalidHandle
	}

	h.mu.Lock()
	state := h.state
	if state == handleRunning {
		h.mu.Unlock()
		handlesMu.Unlock()
		return StatusBusy
	}
	if state == handleFreed {
		h.mu.Unlock()
		handlesMu.Unlock()
		return StatusInvalidHandle
	}

	// Mark freed, zero memory, and remove from map
	h.state = handleFreed
	for _, b := range h.passwords {
		zeroBytes(b)
	}
	delete(handles, id)
	h.mu.Unlock()
	handlesMu.Unlock()

	return StatusOk
}

//export restic_bridge_free
func restic_bridge_free(id C.uint64_t) C.int32_t { return C.int32_t(freeCall(uint64(id))) }

func main() {}
