//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
)

func TestS3TeardownOperationsIntegration(t *testing.T) {
	ctx := context.Background()
	cfg := s3TestConfig()
	peer1 := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}

	// Build a tiny repository that has a data pack and no snapshot object.
	be := s3TestBackend(t, cfg, peer1)
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal("repo new failed", err)
	}
	password := "teardown-fixture-password"
	err = func() error {
		addKeyMu.Lock()
		defer addKeyMu.Unlock()
		return repo.Init(ctx, restic.StableRepoVersion, password, nil)
	}()
	if err != nil {
		t.Fatal("encrypted S3 init failed", err)
	}

	source := t.TempDir()
	if err := os.WriteFile(filepath.Join(source, "file.txt"), []byte("payload for teardown pack"), 0600); err != nil {
		t.Fatal(err)
	}
	sourceRoot, err := os.OpenRoot(source)
	if err != nil {
		t.Fatal(err)
	}
	defer sourceRoot.Close()
	arch := archiver.New(repo, newRootFS(sourceRoot, "source", nil), archiver.Options{})
	arch.Error = func(_ string, err error) error { return err }
	_, snapshotID, _, err := arch.Snapshot(ctx, []string{"source"}, archiver.SnapshotOptions{Hostname: "solstone", Time: time.Now()})
	if err != nil {
		t.Fatal("snapshot creation failed", err)
	}

	// Remove snapshot object so there is a data pack and no snapshot object.
	peer1.mu.Lock()
	snapshotKey := cfg.prefix + "/snapshots/" + snapshotID.String()
	if _, ok := peer1.objects[snapshotKey]; !ok {
		peer1.mu.Unlock()
		t.Fatal("snapshot key not found in peer objects")
	}
	delete(peer1.objects, snapshotKey)
	hasPack := false
	for k := range peer1.objects {
		if strings.Contains(k, "/data/") {
			hasPack = true
			break
		}
	}
	peer1.mu.Unlock()
	if !hasPack {
		t.Fatal("expected pack file in tiny repository")
	}

	// Save objects snapshot for the second attempt fresh peer.
	savedObjects := make(map[string][]byte)
	peer1.mu.Lock()
	for k, v := range peer1.objects {
		savedObjects[k] = append([]byte(nil), v...)
	}
	peer1.mu.Unlock()

	// First attempt:
	// Open owner with s3OwnerTestOpen and beginTeardown so attachBackend accepts it.
	state1 := s3OwnerTestDirectory(t)
	owner1 := s3OwnerTestOpen(t, state1, cfg)
	if err := owner1.beginTeardown(cfg, []byte(password)); err != nil {
		t.Fatal("beginTeardown failed on owner1", err)
	}

	// The factory's transport returns HTTP 500 for DELETE of a data pack and lets lock DELETE succeed.
	transport1 := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method == http.MethodDelete && strings.Contains(req.URL.Path, "/data/") {
			return s3TestResponse(http.StatusInternalServerError, "failed pack delete", nil), nil
		}
		return peer1.RoundTrip(req)
	})

	req1 := &bridgeRequest{
		operation: OpTeardown,
		password:  append([]byte(nil), []byte(password)...),
		remote: &s3RemoteRequest{
			cfg:   cfg,
			owner: owner1,
			factory: func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				return &s3Backend{cfg: cfg, transport: transport1, ctx: owned, cancel: cancel}, nil
			},
		},
	}

	ctx1, cancel1 := context.WithCancel(context.Background())
	defer cancel1()
	h1 := &callHandle{ctx: ctx1, cancel: cancel1, doneCh: make(chan struct{}), passwords: [][]byte{req1.password}, state: handleRunning}

	teardownCtx1 := withTeardownScope(ctx1)
	res1, _ := executeTeardown(teardownCtx1, h1, req1, []restic.ID{snapshotID})

	// Assert the result status is not StatusOk, hasWriteFailed is true, Active is non-nil, and state.json still exists.
	// An empty snapshot list must not be success.
	if res1.status == StatusOk {
		t.Fatal("expected teardown with failing pack DELETE not to return StatusOk")
	}
	if !owner1.hasWriteFailed() {
		t.Fatal("expected writeFailed to be true on owner1")
	}
	owner1.mu.Lock()
	active1 := owner1.record.Active
	owner1.mu.Unlock()
	if active1 == nil {
		t.Fatal("expected Active to be non-nil on owner1 after failed teardown")
	}
	if _, err := os.Stat(filepath.Join(state1, s3OwnerStateName)); err != nil {
		t.Fatal("expected state.json to still exist after failed teardown")
	}

	s3OwnerTestClose(t, owner1)

	// Second attempt:
	// A fresh owner and peer whose pack DELETEs succeed.
	state2 := s3OwnerTestDirectory(t)
	owner2 := s3OwnerTestOpen(t, state2, cfg)
	if err := owner2.beginTeardown(cfg, []byte(password)); err != nil {
		t.Fatal("beginTeardown failed on owner2", err)
	}

	peer2 := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	for k, v := range savedObjects {
		peer2.objects[k] = append([]byte(nil), v...)
	}

	req2 := &bridgeRequest{
		operation: OpTeardown,
		password:  append([]byte(nil), []byte(password)...),
		remote: &s3RemoteRequest{
			cfg:   cfg,
			owner: owner2,
			factory: func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				return &s3Backend{cfg: cfg, transport: peer2, ctx: owned, cancel: cancel}, nil
			},
		},
	}

	ctx2, cancel2 := context.WithCancel(context.Background())
	defer cancel2()
	h2 := &callHandle{ctx: ctx2, cancel: cancel2, doneCh: make(chan struct{}), passwords: [][]byte{req2.password}, state: handleRunning}

	teardownCtx2 := withTeardownScope(ctx2)
	res2, _ := executeTeardown(teardownCtx2, h2, req2, []restic.ID{snapshotID})

	// The same admitted prune returns StatusOk only when hasWriteFailed is false and
	// the returned snapshot ids contain none of the admitted ids.
	if res2.status != StatusOk {
		t.Fatalf("expected teardown with succeeding pack DELETE to return StatusOk, got status %d diagnostic %s", res2.status, res2.diagnostic)
	}
	if owner2.hasWriteFailed() {
		t.Fatal("expected writeFailed to be false on owner2")
	}
	for _, id := range res2.snapshotIDs {
		if id == snapshotID.String() {
			t.Fatalf("returned snapshot IDs contains admitted id %s", id)
		}
	}

	s3OwnerTestClose(t, owner2)
}
