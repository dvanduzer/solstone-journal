//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"net/http"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestOwnedPublicRemotePendingRemainsDurableWithoutSeal(t *testing.T) {
	f := newRootedTestFixture(t, true)
	f.source += "-retained"
	held, err := os.Open(f.source)
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	var requests, deletes atomic.Int32
	testOwnedStage = func(stage string, entry *ownedCall) {
		if stage == "before-publication" {
			entry.request.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				return &s3Backend{cfg: cfg, ctx: owned, cancel: cancel, transport: s3TestTransport(func(request *http.Request) (*http.Response, error) {
					requests.Add(1)
					if request.Method == http.MethodDelete && strings.Contains(request.URL.Path, "/locks/") {
						deletes.Add(1)
						return nil, errS3IO
					}
					return f.peer.RoundTrip(request)
				})}, nil
			}
		}
	}
	t.Cleanup(func() { testOwnedStage = nil })
	input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password, remote: &f.cfg}
	id, status := sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
	if status != StatusOk || id == 0 {
		t.Fatal("actual pending control refused admission", status, id)
	}
	result, waited, freed := sourceCWaitAndFree(id)
	if result.status != StatusRemotePending || result.diagnostic != DiagRemotePending || !validS3ID(result.snapshotID) || waited != StatusOk || freed != StatusOk || deletes.Load() < 2 {
		t.Fatal("actual unacknowledged remote lock did not preserve pending snapshot", result.status, result.diagnostic, waited, freed, deletes.Load())
	}
	before := requests.Load()
	id, status = sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
	if status != StatusOk || id == 0 {
		t.Fatal("pending-state worker did not admit local owner", status, id)
	}
	result, waited, freed = sourceCWaitAndFree(id)
	ownedCalls.Lock()
	sealed, live := ownedCalls.sealed, len(ownedCalls.live)
	ownedCalls.Unlock()
	if result.status != StatusRemotePending || waited != StatusOk || freed != StatusOk || requests.Load() != before || sealed || live != 0 {
		t.Fatal("durable remote pending reset, sealed, or reached credentialed work", result.status, result.diagnostic, waited, freed, requests.Load()-before, sealed, live)
	}
}

func TestOwnedPublicCancellationRetainsActualRemoteTransportUntilJoin(t *testing.T) {
	f := newRootedTestFixture(t, true)
	f.source += "-retained"
	held, err := os.Open(f.source)
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	entered, cancelled, release := make(chan struct{}), make(chan struct{}), make(chan struct{})
	var releaseOnce sync.Once
	unblock := func() { releaseOnce.Do(func() { close(release) }) }
	defer unblock()
	var holdOnce atomic.Bool
	testOwnedStage = func(stage string, entry *ownedCall) {
		if stage == "before-publication" {
			entry.request.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				return &s3Backend{cfg: cfg, ctx: owned, cancel: cancel, transport: s3TestTransport(func(request *http.Request) (*http.Response, error) {
					if request.Method == http.MethodGet && strings.HasSuffix(request.URL.Path, "/config") && holdOnce.CompareAndSwap(false, true) {
						close(entered)
						<-request.Context().Done()
						close(cancelled)
						<-release // deliberately non-cooperative transport remains actually owned
					}
					return f.peer.RoundTrip(request)
				})}, nil
			}
		}
	}
	t.Cleanup(func() { testOwnedStage = nil })
	input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password, remote: &f.cfg}
	id, status := sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
	if status != StatusOk || id == 0 {
		t.Fatal("public transport control refused", status, id)
	}
	complete := false
	defer func() {
		unblock()
		if !complete {
			_ = sourceCCancel(id)
			_, _, _ = sourceCWaitAndFree(id)
		}
	}()
	select {
	case <-entered:
	case <-time.After(5 * time.Second):
		t.Fatal("actual credentialed config request did not enter held peer")
	}
	if sourceCCancel(id) != StatusOk {
		t.Fatal("actual cancellation signal refused")
	}
	select {
	case <-cancelled:
	case <-time.After(5 * time.Second):
		t.Fatal("actual request did not observe operation cancellation")
	}
	if sourceCFree(id) != StatusBusy {
		t.Fatal("actual non-cooperative request was reported drained")
	}
	// Same namespace is still reserved while the actual transport remains active.
	other, status := sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
	if status != StatusBusy || other != 0 {
		t.Fatal("replacement admitted before actual transport join", status, other)
	}
	handlesMu.Lock()
	h := handles[id]
	handlesMu.Unlock()
	if h == nil || h.owned == nil || h.owned.failed() {
		t.Fatal("actual healthy pending transport lost owner or became a cleanup fault")
	}
	h.mu.Lock()
	running := h.state == handleRunning
	h.mu.Unlock()
	if !running || h.ctx.Err() == nil {
		t.Fatal("actual owner did not retain cancelled running worker")
	}
	unblock()
	result, waited, freed := sourceCWaitAndFree(id)
	complete = true
	if result.status != StatusCancelled || waited != StatusOk || freed != StatusOk {
		t.Fatal("actual cancelled request did not join/clean up", result.status, result.diagnostic, waited, freed)
	}
	id, status = sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
	if status != StatusOk || id == 0 {
		t.Fatal("same-process retry after actual transport join refused", status, id)
	}
	result, waited, freed = sourceCWaitAndFree(id)
	if result.status != StatusOk || waited != StatusOk || freed != StatusOk || !validS3ID(result.snapshotID) {
		t.Fatal("same-process retry failed after actual join", result.status, result.diagnostic, waited, freed)
	}
}
