//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
)

// Compare the new operation output with a separate SDK load of the encrypted
// repository and exact persisted snapshot, rather than a prescribed fixture ID,
// fabricated timestamp/path or a metadata-output round trip alone.
func TestResultMetadataMatchesIndependentSDKReadback(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath, sourcePath := filepath.Join(root, "repo"), filepath.Join(root, "source")
	for _, path := range []string{repoPath, sourcePath} {
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "owner.txt"), []byte("metadata parity"), 0600); err != nil {
		t.Fatal(err)
	}
	password := []byte("metadata-sdk-parity-fixture")
	req := bridgeRequest{operation: OpInit, password: password, repoPath: repoPath}
	if result, err := executeInit(ctx, req, func() {}); err != nil || result.status != StatusOk {
		t.Fatal("init failed", err)
	}
	req.operation = OpOpen
	opened, err := executeOpen(ctx, req)
	if err != nil || opened.status != StatusOk || opened.metadataStatus != StatusOk {
		t.Fatal("open failed", err)
	}
	snapshotReq := bridgeRequest{operation: OpSnapshot, password: password, repoPath: repoPath, sourcePath: sourcePath}
	snapshotResult, err := executeSnapshot(ctx, snapshotReq, func() {})
	if err != nil || snapshotResult.status != StatusOk {
		t.Fatal("snapshot failed", err)
	}
	req.operation = OpListSnapshots
	listed, err := executeListSnapshots(ctx, req)
	if err != nil || listed.status != StatusOk || listed.metadataStatus != StatusOk || listed.snapshotCount != 1 {
		t.Fatal("list failed", err)
	}

	readReq := bridgeRequest{operation: OpOpen, password: password, repoPath: repoPath}
	if err := pinRequestPaths(&readReq); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&readReq)
	be, err := operationBackend(ctx, readReq, false)
	if err != nil {
		t.Fatal(err)
	}
	defer be.Close()
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := safeScanKey(ctx, repo, string(password)); err != nil {
		t.Fatal(err)
	}
	var config struct {
		Repository restic.Config `json:"repository"`
	}
	if json.Unmarshal(opened.metadata, &config) != nil || config.Repository != repo.Config() {
		t.Fatal("config differs from independent authenticated SDK load")
	}
	id, err := restic.ParseID(snapshotResult.snapshotID)
	if err != nil {
		t.Fatal(err)
	}
	want, err := data.LoadSnapshot(ctx, repo, id)
	if err != nil {
		t.Fatal(err)
	}
	var catalog struct {
		Snapshots []snapshotMetadataRow `json:"snapshots"`
	}
	if json.Unmarshal(listed.metadata, &catalog) != nil || len(catalog.Snapshots) != 1 || catalog.Snapshots[0].ID != id.String() {
		t.Fatal("catalog has no exact persisted snapshot")
	}
	var got data.Snapshot
	if json.Unmarshal(catalog.Snapshots[0].Snapshot, &got) != nil || !got.Time.Equal(want.Time) ||
		!reflect.DeepEqual(got.Paths, want.Paths) || !reflect.DeepEqual(got.Tags, want.Tags) || !reflect.DeepEqual(got.Summary, want.Summary) {
		t.Fatal("metadata differs from independent persisted snapshot")
	}
}
