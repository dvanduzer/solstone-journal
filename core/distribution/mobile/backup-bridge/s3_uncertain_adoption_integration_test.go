//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// A backup interrupted mid-upload leaves its remote effects uncertain. The next
// backup adopts that custody, removes the interrupted attempt's own lock and
// completes; it is never wedged behind content-addressed uncertainty.
func TestS3InterruptedBackupIsAdoptedByTheNextBackup(t *testing.T) {
	cfg, state, workspace := s3TestConfig(), s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	source := filepath.Join(t.TempDir(), "journal")
	if err := os.Mkdir(source, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "record.txt"), []byte("synthetic record"), 0600); err != nil {
		t.Fatal(err)
	}
	var interrupt atomic.Bool
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if interrupt.Load() {
			if req.Method == http.MethodPut && strings.Contains(req.URL.Path, "/data/") {
				return nil, errS3IO
			}
			if req.Method == http.MethodDelete && strings.Contains(req.URL.Path, "/locks/") {
				return nil, errS3IO
			}
		}
		return peer.RoundTrip(req)
	})
	password := []byte("adoption-daily-fixture")
	run := func(operation int32) bridgeResult {
		req := bridgeRequest{operation: operation, password: append([]byte(nil), password...)}
		if operation == OpSnapshot {
			req.sourcePath = source
		}
		id, handle := s3TestStartOperation(t, cfg, state, workspace, transport, req)
		return s3TestWaitOperation(t, id, handle)
	}
	locks := func() int {
		peer.mu.Lock()
		defer peer.mu.Unlock()
		count := 0
		for key := range peer.objects {
			if strings.Contains(key, "/locks/") {
				count++
			}
		}
		return count
	}
	if r := run(OpInit); r.status != StatusOk {
		t.Fatal("init", r.status, r.diagnostic)
	}
	interrupt.Store(true)
	if r := run(OpSnapshot); r.status == StatusOk {
		t.Fatal("interrupted backup reported success")
	}
	if locks() != 1 {
		t.Fatal("interrupted attempt should leave exactly its own lock", locks())
	}
	owner := s3OwnerTestOpen(t, state, cfg)
	pending := owner.record.Active != nil && owner.record.Active.MayHaveWritten && len(owner.record.Active.Locks) == 1
	s3OwnerTestClose(t, owner)
	if !pending {
		t.Fatal("interrupted attempt's uncertain custody was not recorded")
	}
	interrupt.Store(false)
	r := run(OpSnapshot)
	if r.status != StatusOk || !validS3ID(r.snapshotID) {
		t.Fatal("next backup did not adopt and complete", r.status, r.diagnostic)
	}
	if locks() != 0 {
		t.Fatal("adopted lock was not removed", locks())
	}
	owner = s3OwnerTestOpen(t, state, cfg)
	settled := owner.record.Active == nil && owner.record.Complete != nil
	s3OwnerTestClose(t, owner)
	if !settled {
		t.Fatal("adopting backup did not settle owner custody")
	}
	if listed := run(OpListSnapshots); listed.status != StatusOk || listed.snapshotCount != 1 {
		t.Fatal("unexpected snapshot catalog after adoption", listed.status, listed.diagnostic, listed.snapshotCount)
	}
}

// An interrupted check holds an exclusive lock. The next backup removes it
// before its own work instead of waiting out its lock retry against it.
func TestS3InterruptedCheckIsAdoptedByABackupWithoutWaiting(t *testing.T) {
	f := newRootedTestFixture(t, true)
	var interrupt atomic.Bool
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if interrupt.Load() && req.Method == http.MethodDelete && strings.Contains(req.URL.Path, "/locks/") {
			return nil, errS3IO
		}
		return f.peer.RoundTrip(req)
	})
	interrupt.Store(true)
	id, handle := s3TestStartOperation(t, f.cfg, f.state, f.workspace, transport, bridgeRequest{operation: OpCheck, password: append([]byte(nil), f.password...)})
	if r := s3TestWaitOperation(t, id, handle); r.status != StatusRemotePending {
		t.Fatal("interrupted check not pending", r.status, r.diagnostic)
	}
	interrupt.Store(false)
	source := f.source + "-retained"
	started := time.Now()
	id, handle = s3TestStartOperation(t, f.cfg, f.state, f.workspace, transport, bridgeRequest{operation: OpSnapshot, password: append([]byte(nil), f.password...), sourcePath: source, executionOptions: &operationOptions{retryLock: time.Minute}})
	r := s3TestWaitOperation(t, id, handle)
	if r.status != StatusOk || time.Since(started) > 30*time.Second {
		t.Fatal("backup stalled on the adopted exclusive lock", r.status, r.diagnostic, time.Since(started))
	}
}
