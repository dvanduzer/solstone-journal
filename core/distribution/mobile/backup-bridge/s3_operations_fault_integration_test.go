//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/restic/restic/internal/repository"
)

func TestS3OperationsLostConfigResponseAndLateServerCommitStayPending(t *testing.T) {
	cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	var requests atomic.Int64
	var delayed []byte
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		requests.Add(1)
		if req.Method == http.MethodPut && strings.HasSuffix(req.URL.Path, "/config") {
			var err error
			delayed, err = io.ReadAll(req.Body)
			if err != nil {
				return nil, err
			}
			return nil, errS3IO
		}
		return peer.RoundTrip(req)
	})
	daily, recovery := []byte("late-config-daily-fixture"), []byte("late-config-recovery-fixture")
	id, handle := s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpInit, password: append([]byte(nil), daily...), recoveryPassword: append([]byte(nil), recovery...)})
	result := s3TestWaitOperation(t, id, handle)
	if result.status != StatusRemotePending || !validS3ID(result.keyID) || len(delayed) == 0 {
		t.Fatal("lost config outcome or known key ID was discarded")
	}
	before := requests.Load()
	id, handle = s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpS3Inspect})
	if inspect := s3TestWaitOperation(t, id, handle); inspect.status != StatusRemotePending || requests.Load() != before {
		t.Fatal("inspection reset pending or reached network")
	}
	// This is the controlled server's action after the entire client has drained,
	// not a detached client writer. A native/provider late commit is not inferred.
	peer.mu.Lock()
	peer.objects[cfg.prefix+"/config"] = append([]byte(nil), delayed...)
	_, keyExists := peer.objects[cfg.prefix+"/keys/"+result.keyID]
	peer.mu.Unlock()
	zeroBytes(delayed)
	if !keyExists {
		t.Fatal("known acknowledged key disappeared")
	}
	owner := s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || !owner.record.Active.MayHaveWritten || len(owner.record.Passwords) != 2 || !bytes.Equal(owner.record.Passwords[0], daily) || !bytes.Equal(owner.record.Passwords[1], recovery) {
		t.Fatal("lost-response key custody missing")
	}
	s3OwnerTestClose(t, owner)
	// Independent diagnostic read proves the eventual ciphertext is recoverable.
	// It does not bypass pending admission in the production callable path.
	be := s3TestBackend(t, cfg, peer)
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if key, err := safeScanKey(context.Background(), repo, string(daily)); err != nil || key.String() != result.keyID {
		t.Fatal("late committed encrypted repository is unrecoverable")
	}
	for _, operation := range []int32{OpS3Inspect, OpOpen} {
		password := []byte(nil)
		if operation == OpOpen {
			password = append([]byte(nil), daily...)
		}
		id, handle = s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: operation, password: password})
		if refused := s3TestWaitOperation(t, id, handle); refused.status != StatusRemotePending || requests.Load() != before {
			t.Fatal("late server commit cleared pending admission")
		}
	}
}

func TestS3OperationsCancellationOwnsActualPutUntilReturn(t *testing.T) {
	cfg, state, workspace := s3TestConfig(), s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	daily := []byte("cancelled-remote-daily-fixture")
	id, handle := s3TestStartOperation(t, cfg, state, "", peer, bridgeRequest{operation: OpInit, password: append([]byte(nil), daily...)})
	if result := s3TestWaitOperation(t, id, handle); result.status != StatusOk {
		t.Fatal("init failed")
	}
	source, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	payload := make([]byte, 512*1024)
	if _, err := rand.Read(payload); err != nil || os.WriteFile(filepath.Join(source, "journal.bin"), payload, 0600) != nil {
		t.Fatal("fixture failed")
	}
	entered, cancelled, release := make(chan struct{}), make(chan struct{}), make(chan struct{})
	var once, cancelOnce sync.Once
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method == http.MethodPut && strings.Contains(req.URL.Path, "/data/") {
			reply, err := peer.RoundTrip(req)
			if err != nil {
				return nil, err
			}
			_ = reply.Body.Close()
			once.Do(func() { close(entered) })
			select {
			case <-req.Context().Done():
				cancelOnce.Do(func() { close(cancelled) })
			case <-release:
				return nil, errS3IO
			}
			<-release
			return nil, req.Context().Err()
		}
		return peer.RoundTrip(req)
	})
	id, handle = s3TestStartOperation(t, cfg, state, workspace, transport, bridgeRequest{operation: OpSnapshot, password: append([]byte(nil), daily...), sourcePath: source})
	var releaseOnce sync.Once
	defer func() { releaseOnce.Do(func() { close(release) }); awaitS3Test(t, handle.doneCh) }()
	awaitS3Test(t, entered)
	if cancelCall(id) != StatusOk || freeCall(id) != StatusBusy {
		t.Fatal("active operation was released")
	}
	awaitS3Test(t, cancelled)
	assertS3Pending(t, handle.doneCh)
	if other, err := newS3Owner(s3OwnerTestDirectory(t), cfg); err != errS3OwnerBusy || other != nil {
		if other != nil {
			_ = other.Close()
		}
		t.Fatal("replacement entered during actual cancelled PUT")
	}
	releaseOnce.Do(func() { close(release) })
	result := s3TestWaitOperation(t, id, handle)
	if result.status != StatusRemotePending || result.snapshotID != "" {
		t.Fatal("lost cancelled PUT was reported clean or published snapshot")
	}
	actual, err := os.ReadFile(filepath.Join(source, "journal.bin"))
	if err != nil || !bytes.Equal(actual, payload) {
		t.Fatal("interruption changed source")
	}
	owner := s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || !owner.record.Active.MayHaveWritten {
		t.Fatal("cancelled write uncertainty was not retained")
	}
}
