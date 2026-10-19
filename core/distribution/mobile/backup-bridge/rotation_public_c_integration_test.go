//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func rotationCFixture(t *testing.T) (sourceCInput, *s3MemoryPeer, string, string) {
	t.Helper()
	cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	daily, recovery := []byte("public-c-daily"), []byte("public-c-recovery-distinct")
	call := func(op int32, password []byte) bridgeResult {
		req := bridgeRequest{operation: op, password: append([]byte(nil), password...)}
		if op == OpInit || op == OpAddRecoveryKey {
			req.recoveryPassword = append([]byte(nil), recovery...)
		}
		id, handle := s3TestStartOperation(t, cfg, state, "", peer, req)
		return s3TestWaitOperation(t, id, handle)
	}
	initialized, added := call(OpInit, daily), call(OpAddRecoveryKey, daily)
	if initialized.status != StatusOk || added.status != StatusOk || !validS3ID(added.keyID) || initialized.keyID == added.keyID {
		t.Fatal("encrypted fixture initialization failed", initialized.status, added.status)
	}
	if opened := call(OpOpen, recovery); opened.status != StatusOk || opened.keyID != added.keyID {
		t.Fatal("independent recovery control failed", opened.status, opened.diagnostic)
	}
	return sourceCInput{remote: &cfg, state: state, password: daily, recovery: recovery}, peer, initialized.keyID, added.keyID
}

func rotationCTrack(t *testing.T, id uint64) func() (bridgeResult, int32, int32) {
	t.Helper()
	if id == 0 {
		return nil
	}
	handlesMu.Lock()
	handle := handles[id]
	handlesMu.Unlock()
	if handle == nil {
		t.Fatal("C entry returned an unknown handle", id)
	}
	finished := false
	t.Cleanup(func() {
		if !finished {
			_ = sourceCCancel(id)
			// Keep hooks and fixtures alive until actual native drain. The
			// enclosing Go test deadline contains a worker that cannot drain.
			<-handle.doneCh
			freed := sourceCFree(id)
			finished = freed == StatusOk
			if !finished {
				t.Error("C handle failure cleanup did not free after drain", freed)
			}
		}
	})
	return func() (bridgeResult, int32, int32) {
		select {
		case <-handle.doneCh:
		case <-time.After(45 * time.Second):
			t.Fatal("actual C removal did not drain before the test deadline")
		}
		result, waited, freed := sourceCWaitAndFree(id)
		finished = waited == StatusOk && freed == StatusOk
		return result, waited, freed
	}
}

func TestRotationPublicCTypedRemovalAuthorityAndSuccess(t *testing.T) {
	input, peer, dailyID, recoveryID := rotationCFixture(t)
	held, err := os.Open(input.state)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = held.Close() })
	statePath := filepath.Join(input.state, s3OwnerStateName)
	before, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatal(err)
	}
	var current *ownedCall
	var workers, requests atomic.Int32
	var deletesMu sync.Mutex
	var keyDeletes []string
	testOwnedStage = func(stage string, entry *ownedCall) {
		if stage == "copied" {
			current = entry
		}
	}
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			workers.Add(1)
			current.mu.Lock()
			current.request.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
					requests.Add(1)
					if req.Method == http.MethodDelete && strings.Contains(req.URL.Path, "/keys/") {
						deletesMu.Lock()
						keyDeletes = append(keyDeletes, req.URL.Path)
						deletesMu.Unlock()
					}
					return peer.RoundTrip(req)
				})
				return &s3Backend{cfg: cfg, transport: transport, ctx: owned, cancel: cancel}, nil
			}
			current.mu.Unlock()
		}
		return nil
	}
	t.Cleanup(func() { testOwnedStage = nil; testGate = nil })
	id, status := rotationCS3Remove(input, int(held.Fd()), recoveryID, 0)
	_ = rotationCTrack(t, id)
	if id != 0 || status != StatusInvalidArgument || workers.Load() != 0 || requests.Load() != 0 {
		t.Fatal("unprivileged append-only removal reached work", id, status, workers.Load(), requests.Load())
	}
	after, err := os.ReadFile(statePath)
	if err != nil || !bytes.Equal(before, after) {
		t.Fatal("maintenance refusal changed durable state", err)
	}
	id, status = rotationCS3Remove(input, int(held.Fd()), recoveryID, 1)
	join := rotationCTrack(t, id)
	if id == 0 || status != StatusOk {
		t.Fatal("typed maintenance entry refused", id, status)
	}
	result, waited, freed := join()
	deletesMu.Lock()
	deletes := append([]string(nil), keyDeletes...)
	deletesMu.Unlock()
	expectedDelete := "/" + input.remote.bucket + "/" + input.remote.prefix + "/keys/" + recoveryID
	if result.status != StatusOk || waited != StatusOk || freed != StatusOk || workers.Load() != 1 || len(deletes) != 1 || deletes[0] != expectedDelete {
		t.Fatal("typed C removal did not delete exactly the selected key", result.status, result.diagnostic, waited, freed, workers.Load(), deletes)
	}
	testOwnedStage, testGate = nil, nil
	call := func(password []byte) bridgeResult {
		req := bridgeRequest{operation: OpOpen, password: append([]byte(nil), password...)}
		id, handle := s3TestStartOperation(t, *input.remote, input.state, "", peer, req)
		return s3TestWaitOperation(t, id, handle)
	}
	if opened := call(input.recovery); opened.status == StatusOk || opened.diagnostic != DiagAuthFailed {
		t.Fatal("removed recovery still authenticates or refusal was not authentication", opened.status, opened.diagnostic)
	}
	if opened := call(input.password); opened.status != StatusOk || opened.keyID != dailyID {
		t.Fatal("daily access did not survive exact removal", opened.status, opened.diagnostic)
	}
}

func TestRotationPublicCStateReplacementRefusesBeforePersistence(t *testing.T) {
	input, peer, _, recoveryID := rotationCFixture(t)
	owner := s3OwnerTestOpen(t, input.state, *input.remote)
	if err := owner.begin(OpRemoveRecoveryKey, false, *input.remote, input.password); err != nil {
		t.Fatal(err)
	}
	if err := owner.noteSave(""); err != nil {
		t.Fatal(err)
	}
	s3OwnerTestClose(t, owner)
	held, err := os.Open(input.state)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = held.Close() })
	before, err := os.ReadFile(filepath.Join(input.state, s3OwnerStateName))
	if err != nil {
		t.Fatal(err)
	}
	saved := input.state + "-original"
	// This directory already satisfies the platform's private-storage policy.
	replacement := s3OwnerTestDirectory(t)
	var swapped bool
	var swapErr error
	var workers atomic.Int32
	var current *ownedCall
	testOwnedStage = func(stage string, entry *ownedCall) {
		if stage == "copied" {
			current = entry
		}
		if stage == "copied" && !swapped {
			swapped = true
			if swapErr = os.Rename(input.state, saved); swapErr == nil {
				swapErr = os.Rename(replacement, input.state)
			}
		}
	}
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			workers.Add(1)
			current.mu.Lock()
			current.request.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				return &s3Backend{cfg: cfg, transport: peer, ctx: owned, cancel: cancel}, nil
			}
			current.mu.Unlock()
		}
		return nil
	}
	t.Cleanup(func() { testOwnedStage = nil; testGate = nil })
	id, status := rotationCS3Remove(input, int(held.Fd()), recoveryID, 1)
	_ = rotationCTrack(t, id)
	if !swapped || swapErr != nil || id != 0 || status != StatusInvalidArgument || workers.Load() != 0 {
		t.Fatal("replacement state directory was adopted", swapped, swapErr, id, status, workers.Load())
	}
	after, err := os.ReadFile(filepath.Join(saved, s3OwnerStateName))
	if err != nil || !bytes.Equal(before, after) {
		t.Fatal("original uncertainty custody changed", err)
	}
	if _, err := os.Lstat(filepath.Join(input.state, s3OwnerStateName)); !os.IsNotExist(err) {
		t.Fatal("replacement received a native secret record", err)
	}
	// Its own descriptor admits real work, so policy cannot explain the refusal.
	admissible, err := os.Open(input.state)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = admissible.Close() })
	id, status = rotationCS3Remove(input, int(admissible.Fd()), recoveryID, 1)
	join := rotationCTrack(t, id)
	if id == 0 || status != StatusOk {
		t.Fatal("otherwise admissible replacement control refused", id, status)
	}
	result, waited, freed := join()
	if result.status != StatusOk || waited != StatusOk || freed != StatusOk || workers.Load() != 1 {
		t.Fatal("replacement control did not perform actual work", result.status, result.diagnostic, waited, freed, workers.Load())
	}
}
