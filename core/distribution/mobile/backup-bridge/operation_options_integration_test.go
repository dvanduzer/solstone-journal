//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/restic/restic/internal/restic"
)

func newOptionsCheckFixture(t *testing.T) bridgeRequest {
	t.Helper()
	base := t.TempDir()
	repo, source := filepath.Join(base, "repo"), filepath.Join(base, "journal")
	for _, path := range []string{repo, source} {
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(source, "owner.txt"), []byte("real encrypted owner record"), 0600); err != nil {
		t.Fatal(err)
	}
	req := bridgeRequest{operation: OpInit, repoPath: repo, password: []byte("options-check-synthetic-password")}
	if result, err := executeInit(context.Background(), req, func() {}); err != nil || result.status != StatusOk {
		t.Fatal("init", result, err)
	}
	req.operation, req.sourcePath = OpSnapshot, source
	if result, err := executeSnapshot(context.Background(), req, func() {}); err != nil || result.status != StatusOk {
		t.Fatal("snapshot", result, err)
	}
	req.operation, req.sourcePath = OpCheck, ""
	return req
}

func TestOperationOptionsCheckActualLockContextLossNeverSucceeds(t *testing.T) {
	req := newOptionsCheckFixture(t)
	// Count the actual repository's pack IDs to select a provably empty group.
	present := make(map[uint32]bool)
	err := filepath.Walk(filepath.Join(req.repoPath, "data"), func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if !info.IsDir() && len(info.Name()) == 64 {
			id, err := restic.ParseID(info.Name())
			if err != nil {
				return err
			}
			present[uint32(id[0])+1] = true
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	var empty uint32
	for group := uint32(1); group <= 256; group++ {
		if !present[group] {
			empty = group
			break
		}
	}
	if empty == 0 || len(present) == 0 {
		t.Fatal("fixture has no proven empty/nonempty groups")
	}
	for _, mode := range []struct {
		name    string
		options *operationOptions
	}{
		{"legacy-full", nil},
		{"structure", &operationOptions{checkMode: checkReadStructure}},
		{"all", &operationOptions{checkMode: checkReadAll}},
		{"group", &operationOptions{checkMode: checkReadGroup, group: 1, groups: 1}},
		{"empty-group", &operationOptions{checkMode: checkReadGroup, group: empty, groups: 256}},
	} {
		t.Run(mode.name, func(t *testing.T) {
			req.executionOptions = mode.options
			phases := []string{"snapshots", "index", "packs", "before-structure", "structure", "complete"}
			if mode.options == nil || mode.options.checkMode != checkReadStructure {
				phases = append(phases, "before-read-packs", "read-packs")
			}
			for _, phase := range phases {
				t.Run(phase, func(t *testing.T) {
					parent := context.Background()
					called := false
					joined := false
					cleared := func() { testCheckPhase = nil }
					t.Cleanup(cleared)
					testCheckPhase = func(actual string, acquired context.Context, unlock func()) {
						if (phase == "before-structure" && actual == "structure") || (phase == "before-read-packs" && actual == "read-packs") {
							joined = true
						}
						if actual == phase {
							called = true
							unlock() // actual SDK cancellation and refresh goroutine join
							if acquired.Err() == nil || parent.Err() != nil {
								t.Error("did not isolate acquired lock loss")
							}
						}
					}
					result, err := executeCheck(parent, req)
					cleared()
					if !called || err == nil || result.status == StatusOk || result.diagnostic != DiagLocked || parent.Err() != nil {
						t.Fatal("lost lock published success or changed parent", called, result, err)
					}
					if (phase == "before-structure" || phase == "before-read-packs") && !joined {
						t.Fatal("pre-worker refusal substituted for actual checker channel/counter completion")
					}
				})
			}
			if result, err := executeCheck(context.Background(), req); err != nil || result.status != StatusOk {
				t.Fatal("valid same-process retry after actual lock drain failed", result, err)
			}
		})
	}
}
