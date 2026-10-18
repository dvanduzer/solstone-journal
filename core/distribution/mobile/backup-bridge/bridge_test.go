// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
)

func TestActualShapeValidation(t *testing.T) {
	valid := requestShape{operation: OpInit, fields: [6]requestField{{4, true}, {0, false}, {8, true}}}
	if got := validateShape(valid); got != StatusOk {
		t.Fatalf("valid request refused: %d", got)
	}
	caps := [6]uint32{maxPasswordLen, maxPasswordLen, maxPathLen, maxPathLen, maxPathLen, idHexLen}
	for i, cap := range caps {
		test := valid
		test.fields[i] = requestField{cap + 1, true}
		if got := validateShape(test); got != StatusCapExceeded {
			t.Fatalf("field %d overflow accepted: %d", i, got)
		}
		test = valid
		test.fields[i] = requestField{1, false}
		if got := validateShape(test); got != StatusInvalidArgument {
			t.Fatalf("field %d NULL accepted: %d", i, got)
		}
	}
	for _, op := range []int32{0, OpCheck + 1} {
		test := valid
		test.operation = op
		if validateShape(test) != StatusInvalidArgument {
			t.Fatal("unknown operation accepted")
		}
	}
	for _, op := range []int32{OpAddRecoveryKey, OpSnapshot, OpRestore} {
		test := valid
		test.operation = op
		if validateShape(test) != StatusInvalidArgument {
			t.Fatalf("missing operation input accepted: %d", op)
		}
	}
	for _, op := range []int32{OpListRecoveryKeys, OpRemoveRecoveryKey} {
		test := valid
		test.operation = op
		if got := validateShape(test); got != StatusOk {
			t.Fatalf("valid operation %d refused: %d", op, got)
		}
	}
}

func TestSyntacticPathAndOverlapValidation(t *testing.T) {
	for _, path := range []string{"", "relative", "/a/../b", "//a", "s3://bucket/path", "/a\x00b", "/" + strings.Repeat("a", maxPathLen)} {
		if sanitizePath(path) == nil {
			t.Fatalf("invalid path accepted: %q", path)
		}
	}
	if sanitizePath("/owner/repository") != nil {
		t.Fatal("canonical path refused")
	}
	for _, pair := range [][2]string{{"/a", "/a"}, {"/a", "/a/child"}, {"/a/child", "/a"}, {"/a", "/a/..repo"}, {"/a/..restore", "/a"}} {
		if !pathsOverlap(pair[0], pair[1]) {
			t.Fatal("overlap missed")
		}
	}
	if pathsOverlap("/a", "/ab") {
		t.Fatal("unrelated paths overlap")
	}
}

func TestSnapshotListRefusesOverflow(t *testing.T) {
	for _, count := range []int{maxSnapshotCount, maxSnapshotCount + 1} {
		ids := make([]string, count)
		for i := range ids {
			ids[i] = fmt.Sprintf("%064x", i)
		}
		result := clipSnapshotIDs(ids)
		if count == maxSnapshotCount {
			if result.status != StatusOk || int(result.snapshotCount) != count {
				t.Fatal("complete list refused")
			}
		} else if result.status != StatusCapExceeded || result.snapshotCount != 0 || len(result.snapshotIDs) != 0 {
			t.Fatal("overflow truncated into success")
		}
	}
}

func syntheticHandle(t *testing.T) *callHandle {
	t.Helper()
	id, status := allocateID(&nextID)
	if status != StatusOk {
		t.Fatal("ID allocation failed")
	}
	ctx, cancel := context.WithCancel(context.Background())
	h := &callHandle{id: id, ctx: ctx, cancel: cancel, doneCh: make(chan struct{}), state: handleRunning, passwords: [][]byte{[]byte("fixture-password")}}
	handlesMu.Lock()
	handles[id] = h
	handlesMu.Unlock()
	return h
}

func TestCancelPreventsLateSuccessAndRetainsRunningWorker(t *testing.T) {
	h := syntheticHandle(t)
	if cancelCall(h.id) != StatusOk {
		t.Fatal("cancel refused")
	}
	if freeCall(h.id) != StatusBusy {
		t.Fatal("live worker released")
	}
	finishCall(h, bridgeResult{status: StatusOk, snapshotID: strings.Repeat("a", 64)}, true)
	<-h.doneCh
	if h.result.status != StatusCancelledUncertain {
		t.Fatal("late success accepted after cancellation")
	}
	if h.result.keyID != "" || h.result.snapshotID != "" || h.result.snapshotCount != 0 || len(h.result.snapshotIDs) != 0 {
		t.Fatal("cancelled publication retained operation payload")
	}
	for _, b := range h.passwords[0] {
		if b != 0 {
			t.Fatal("owned credential bytes retained")
		}
	}
	if freeCall(h.id) != StatusOk || freeCall(h.id) != StatusInvalidHandle {
		t.Fatal("free lifetime incorrect")
	}
}

func TestCancelAfterPublicationKeepsTerminalResult(t *testing.T) {
	h := syntheticHandle(t)
	finishCall(h, bridgeResult{status: StatusOk}, false)
	if cancelCall(h.id) != StatusOk || h.result.status != StatusOk {
		t.Fatal("terminal result rewritten")
	}
	if freeCall(h.id) != StatusOk {
		t.Fatal("completed worker not released")
	}
}

func TestConcurrentCancellationAndPublication(t *testing.T) {
	for i := 0; i < 64; i++ {
		h := syntheticHandle(t)
		var wg sync.WaitGroup
		wg.Add(2)
		go func() { defer wg.Done(); cancelCall(h.id) }()
		go func() { defer wg.Done(); finishCall(h, bridgeResult{status: StatusOk}, true) }()
		wg.Wait()
		<-h.doneCh
		if h.result.status != StatusOk && h.result.status != StatusCancelledUncertain {
			t.Fatal("invalid terminal result")
		}
		if freeCall(h.id) != StatusOk {
			t.Fatal("completion not drainable")
		}
	}
}

func TestWorkerPanicCompletesAndClearsOwnedBytes(t *testing.T) {
	testGate = func(string, context.Context) error { panic("fixture panic") }
	defer func() { testGate = nil }()
	id, status := startBridgeCall(bridgeRequest{operation: OpOpen, password: []byte("fixture-password")})
	if status != StatusOk {
		t.Fatal("start failed")
	}
	handlesMu.Lock()
	h := handles[id]
	handlesMu.Unlock()
	<-h.doneCh
	if h.result.status != StatusInternal {
		t.Fatal("panic escaped refusal boundary")
	}
	if freeCall(id) != StatusOk {
		t.Fatal("panic retained live handle")
	}
}

func TestAdmissionBoundsRealHandles(t *testing.T) {
	release := make(chan struct{})
	testGate = func(string, context.Context) error { <-release; return context.Canceled }
	var ids []uint64
	defer func() {
		close(release)
		for _, id := range ids {
			handlesMu.Lock()
			h := handles[id]
			handlesMu.Unlock()
			<-h.doneCh
			freeCall(id)
		}
		testGate = nil
	}()
	for i := 0; i < 4; i++ {
		id, status := startBridgeCall(bridgeRequest{operation: OpOpen, password: []byte("fixture")})
		if status != StatusOk {
			t.Fatal("bounded worker admission failed")
		}
		ids = append(ids, id)
	}
	if id, status := startBridgeCall(bridgeRequest{operation: OpOpen, password: []byte("fixture")}); id != 0 || status != StatusBusy {
		t.Fatal("unbounded workers admitted")
	}
	for _, id := range ids {
		if cancelCall(id) != StatusOk || freeCall(id) != StatusBusy {
			t.Fatal("cancellation detached a worker")
		}
	}
}

func TestIDExhaustionCannotAlias(t *testing.T) {
	var counter atomic.Uint64
	counter.Store(^uint64(0) - 1)
	if id, status := allocateID(&counter); id != ^uint64(0) || status != StatusOk {
		t.Fatal("last ID invalid")
	}
	for i := 0; i < 2; i++ {
		if id, status := allocateID(&counter); id != 0 || status != StatusCapExceeded {
			t.Fatal("ID namespace wrapped")
		}
	}
}

func TestKeyResourceValidationPreservesNormalParameters(t *testing.T) {
	key := map[string]any{"kdf": "scrypt", "N": 32768, "r": 8, "p": 4, "salt": make([]byte, 64), "data": make([]byte, 64)}
	encode := func() []byte {
		data, err := json.Marshal(key)
		if err != nil {
			t.Fatal(err)
		}
		return data
	}
	if err := validateKeyResources(encode()); err != nil {
		t.Fatal(err)
	}
	for _, field := range []string{"N", "r", "p"} {
		original := key[field]
		key[field] = 1 << 40
		if validateKeyResources(encode()) == nil {
			t.Fatalf("excessive %s accepted", field)
		}
		key[field] = original
	}
	key["N"], key["r"], key["p"] = 2, 1, 8388608
	if validateKeyResources(encode()) == nil {
		t.Fatal("parallelism allocation exceeding memory cap accepted")
	}
	key["N"], key["r"], key["p"] = 1<<20, 1, 1
	if validateKeyResources(encode()) == nil {
		t.Fatal("combined scrypt allocations exceeding memory cap accepted")
	}
	key["N"], key["r"], key["p"] = 32768, 8, 4
	key["data"] = []byte{1}
	if validateKeyResources(encode()) == nil {
		t.Fatal("short key accepted")
	}
}

func TestDiagnosticsNeverReturnCredentialText(t *testing.T) {
	request := bridgeRequest{password: []byte("synthetic-secret")}
	diagnostic := scrubError(fmt.Errorf("upstream synthetic-secret"), request)
	if diagnostic != DiagAuthFailed || strings.Contains(diagnostic, "synthetic-secret") {
		t.Fatal("credential-bearing diagnostic")
	}
}

func TestPinnedDirectorySurvivesCallerPathReplacement(t *testing.T) {
	root := t.TempDir()
	original := filepath.Join(root, "source")
	moved := filepath.Join(root, "moved")
	if err := os.Mkdir(original, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(original, "owner.txt"), []byte("retained"), 0600); err != nil {
		t.Fatal(err)
	}
	pinned, err := openPinnedDirectory(original)
	if err != nil {
		t.Fatal(err)
	}
	defer pinned.Close()

	adoptedRoot, err := adoptRoot(original, pinned)
	if err != nil {
		t.Fatal(err)
	}
	defer adoptedRoot.Close()

	if err := os.Rename(original, moved); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(root, original); err != nil {
		t.Fatal(err)
	}

	data, err := adoptedRoot.ReadFile("owner.txt")
	if err != nil || string(data) != "retained" {
		t.Fatalf("adopted root redirected after replacement: %q %v", data, err)
	}
}

func TestPinnedDirectoryOverlapUsesIdentityAndAncestry(t *testing.T) {
	root := t.TempDir()
	child := filepath.Join(root, "child")
	other := t.TempDir()
	if err := os.Mkdir(child, 0700); err != nil {
		t.Fatal(err)
	}
	rootFD, err := openPinnedDirectory(root)
	if err != nil {
		t.Fatal(err)
	}
	defer rootFD.Close()
	childFD, err := openPinnedDirectory(child)
	if err != nil {
		t.Fatal(err)
	}
	defer childFD.Close()
	otherFD, err := openPinnedDirectory(other)
	if err != nil {
		t.Fatal(err)
	}
	defer otherFD.Close()
	if overlap, err := pinnedDirectoriesOverlap(rootFD, childFD); err != nil || !overlap {
		t.Fatalf("directory ancestry missed: overlap=%t err=%v", overlap, err)
	}
	if overlap, err := pinnedDirectoriesOverlap(rootFD, otherFD); err != nil || overlap {
		t.Fatalf("unrelated directory overlapped: overlap=%t err=%v", overlap, err)
	}
}
