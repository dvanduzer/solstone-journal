//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/restic/restic/internal/backend"
	"golang.org/x/sys/unix"
)

func TestS3OperationsIndependentFailedSaveKeepsKnownAttemptID(t *testing.T) {
	cfg := s3TestConfig()
	owner := s3OwnerTestOpen(t, s3OwnerTestDirectory(t), cfg)
	if err := owner.begin(OpInit, cfg, []byte("fixture-password")); err != nil {
		t.Fatal(err)
	}
	base := s3TestBackend(t, cfg, s3TestTransport(func(*http.Request) (*http.Response, error) { return nil, errS3IO }))
	wrapped, err := owner.attachBackend(base)
	if err != nil {
		t.Fatal(err)
	}
	id := s3TestID(33)
	if err := wrapped.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: id}, backend.NewByteReader([]byte("ciphertext"), nil)); err == nil {
		t.Fatal("failure fixture returned success")
	}
	if err := owner.drainPrimary(); err != nil {
		t.Fatal(err)
	}
	if owner.knownKeyID != id {
		t.Fatal("attempted content-addressed key ID was lost with the response")
	}
	if visible, err := owner.complete(); visible || !errors.Is(err, errS3OwnerPending) {
		t.Fatal("unknown Save completed")
	}
}

func TestS3OperationsIndependentActualPublishedRestorePanicRemainsExplicit(t *testing.T) {
	cfg, state, workspace := s3TestConfig(), s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(base, "source")
	if os.Mkdir(source, 0700) != nil || os.WriteFile(filepath.Join(source, "payload"), []byte("synthetic restore fixture"), 0600) != nil {
		t.Fatal("source fixture failed")
	}
	password := []byte("review-operation-password")
	call := func(req bridgeRequest) bridgeResult {
		req.password = append([]byte(nil), password...)
		id, handle := s3TestStartOperation(t, cfg, state, workspace, peer, req)
		return s3TestWaitOperation(t, id, handle)
	}
	if got := call(bridgeRequest{operation: OpInit}); got.status != StatusOk {
		t.Fatal("init fixture failed", got.status, got.diagnostic)
	}
	snapshot := call(bridgeRequest{operation: OpSnapshot, sourcePath: source})
	if snapshot.status != StatusOk {
		t.Fatal("snapshot fixture failed", snapshot.status, snapshot.diagnostic)
	}
	destination := filepath.Join(base, "restored")
	originalSync := syncDirectory
	var raised atomic.Bool
	syncDirectory = func(fd int) error {
		if _, err := os.Stat(destination); err == nil && raised.CompareAndSwap(false, true) {
			panic("controlled panic after actual publication")
		}
		return unix.Fsync(fd)
	}
	defer func() { syncDirectory = originalSync }()
	result := call(bridgeRequest{operation: OpRestore, sourcePath: source, restorePath: destination, snapshotID: snapshot.snapshotID})
	if !raised.Load() {
		t.Fatal("panic fixture never reached actual publication")
	}
	if _, err := os.Stat(filepath.Join(destination, "source/payload")); err != nil {
		t.Fatal("fixture did not actually publish restore")
	}
	if result.status != StatusCommittedUncertain || result.diagnostic != DiagCommittedUncertain {
		t.Fatalf("published restore became generic status=%d diagnostic=%s", result.status, result.diagnostic)
	}
}

func TestS3OperationsIndependentCleanupPanicReachesTerminalAndReleasesOwner(t *testing.T) {
	cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, state, cfg)
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(base, "source")
	if err := os.Mkdir(source, 0700); err != nil {
		t.Fatal(err)
	}
	req := bridgeRequest{operation: OpRestore, password: []byte("fixture-password"), sourcePath: source, restorePath: filepath.Join(base, "destination"), snapshotID: s3TestID(1), remote: &s3RemoteRequest{cfg: cfg, owner: owner}}
	if err := pinDataPaths(&req, []*os.File{owner.directory}); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	h := &callHandle{ctx: ctx, cancel: cancel, doneCh: make(chan struct{}), passwords: [][]byte{req.password}, state: handleRunning}
	originalSync := syncDirectory
	syncDirectory = func(int) error { panic("controlled cleanup sync panic") }
	escaped := false
	func() {
		defer func() { escaped = recover() != nil }()
		runWorker(h, req)
	}()
	syncDirectory = originalSync
	terminal := false
	select {
	case <-h.doneCh:
		terminal = true
	default:
	}
	descriptorsClosed := true
	for _, file := range req.pathFiles {
		if _, err := file.Stat(); err == nil {
			descriptorsClosed = false
		}
	}
	released := false
	if terminal {
		replacement, err := newS3Owner(state, cfg)
		if err == nil {
			released = true
			_ = replacement.Close()
		}
	}
	// The direct call lets the test catch the escaping panic and join/close all
	// fixture resources; an exported asynchronous call has no such catcher.
	_ = closeRequestPaths(&req)
	_ = owner.Close()
	if escaped || !terminal || !descriptorsClosed || !released {
		t.Fatalf("cleanup panic escaped=%t terminal=%t descriptors_closed=%t owner_released=%t", escaped, terminal, descriptorsClosed, released)
	}
}

func TestS3OperationsIndependentCancelledPrimaryAndDistinctCleanupRetainOwnership(t *testing.T) {
	cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	password := []byte("fixture-password")
	initialID, initialHandle := s3TestStartOperation(t, cfg, state, "", peer, bridgeRequest{operation: OpInit, password: append([]byte(nil), password...)})
	if got := s3TestWaitOperation(t, initialID, initialHandle); got.status != StatusOk {
		t.Fatal("init fixture failed")
	}
	primaryEntered, primaryCancelled, cleanupEntered := make(chan struct{}), make(chan struct{}), make(chan struct{})
	releasePrimary, releaseCleanup := make(chan struct{}), make(chan struct{})
	var primaryOnce, cleanupOnce sync.Once
	var deletes atomic.Int32
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method != http.MethodDelete {
			return peer.RoundTrip(req)
		}
		switch deletes.Add(1) {
		case 1:
			close(primaryEntered)
			select {
			case <-req.Context().Done():
				close(primaryCancelled)
			case <-releasePrimary:
				return nil, errS3IO
			}
			<-releasePrimary
			return nil, req.Context().Err()
		case 2:
			close(cleanupEntered)
			<-releaseCleanup
			if req.Context().Err() != nil {
				return nil, req.Context().Err()
			}
			return peer.RoundTrip(req)
		default:
			return nil, errS3Denied
		}
	})
	id, handle := s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpOpen, password: append([]byte(nil), password...)})
	defer func() {
		primaryOnce.Do(func() { close(releasePrimary) })
		cleanupOnce.Do(func() { close(releaseCleanup) })
		awaitS3Test(t, handle.doneCh)
	}()
	awaitS3Test(t, primaryEntered)
	if cancelCall(id) != StatusOk {
		t.Fatal("cancel refused")
	}
	awaitS3Test(t, primaryCancelled)
	assertOwned := func() {
		if freeCall(id) != StatusBusy {
			t.Fatal("live handle released")
		}
		replacement := cfg
		replacement.secret = "replacement-fixture-secret"
		other, err := newS3Owner(s3OwnerTestDirectory(t), replacement)
		if other != nil {
			_ = other.Close()
		}
		if !errors.Is(err, errS3OwnerBusy) {
			t.Fatal("same destination escaped ownership")
		}
	}
	assertOwned()
	primaryOnce.Do(func() { close(releasePrimary) })
	awaitS3Test(t, cleanupEntered)
	assertOwned()
	if cancelCall(id) != StatusOk {
		t.Fatal("second cancel refused")
	}
	cleanupOnce.Do(func() { close(releaseCleanup) })
	result := s3TestWaitOperation(t, id, handle)
	if result.status != StatusCancelled || result.keyID == "" {
		t.Fatal("acknowledged lock cleanup lost honest cancellation or known key ID", result.status, result.diagnostic)
	}
	reopened := s3OwnerTestOpen(t, state, cfg)
	if reopened.record.Active != nil || reopened.record.Complete == nil {
		t.Fatal("fully observed cleanup was not completed")
	}
}
