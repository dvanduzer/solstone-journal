//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"net/http"
	"path"
	"strings"
	"sync"
	"testing"

	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
	"github.com/restic/restic/internal/ui/progress"
)

func TestOperationOptionsActualSelectedReadsAndCorruptionTwins(t *testing.T) {
	f := newRootedTestFixture(t, true)
	be := s3TestBackend(t, f.cfg, f.peer)
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := safeScanKey(context.Background(), repo, string(f.password)); err != nil {
		t.Fatal(err)
	}
	if err := repo.LoadIndex(context.Background(), &progress.NoopPrinter{}); err != nil {
		t.Fatal(err)
	}
	types := make(map[restic.ID]map[restic.BlobType]bool)
	if err := repo.ListBlobs(context.Background(), func(blob restic.PackedBlob) {
		if types[blob.PackID] == nil {
			types[blob.PackID] = make(map[restic.BlobType]bool)
		}
		types[blob.PackID][blob.Type] = true
	}); err != nil {
		t.Fatal(err)
	}
	if err := be.Close(); err != nil {
		t.Fatal(err)
	}
	var dataID, treeID restic.ID
	var dataKey, treeKey string
	present := make(map[uint32]bool)
	f.peer.mu.Lock()
	for key := range f.peer.objects {
		if !strings.Contains(key, "/data/") {
			continue
		}
		id, err := restic.ParseID(path.Base(key))
		if err != nil {
			t.Fatal(err)
		}
		present[uint32(id[0])+1] = true
		if types[id][restic.DataBlob] && !types[id][restic.TreeBlob] {
			dataID, dataKey = id, key
		}
		if types[id][restic.TreeBlob] {
			treeID, treeKey = id, key
		}
	}
	f.peer.mu.Unlock()
	if dataKey == "" || treeKey == "" {
		t.Fatal("fixture did not prove separate data-only and structural tree packs")
	}
	var empty uint32
	for group := uint32(1); group <= 256; group++ {
		if !present[group] {
			empty = group
			break
		}
	}
	if empty == 0 {
		t.Fatal("fixture lacks proven empty group")
	}
	type observed struct{ structure, contents map[restic.ID]int }
	call := func(options *operationOptions) (bridgeResult, observed) {
		t.Helper()
		var mu sync.Mutex
		readContents := false
		got := observed{make(map[restic.ID]int), make(map[restic.ID]int)}
		testCheckPhase = func(phase string, _ context.Context, _ func()) {
			mu.Lock()
			defer mu.Unlock()
			switch phase {
			case "structure", "before-read-packs":
				readContents = true
			case "read-packs", "complete":
				readContents = false
			}
		}
		transport := s3TestTransport(func(request *http.Request) (*http.Response, error) {
			if request.Method == http.MethodGet && strings.Contains(request.URL.Path, "/data/") {
				id, err := restic.ParseID(path.Base(request.URL.Path))
				if err != nil {
					return nil, err
				}
				mu.Lock()
				if readContents {
					got.contents[id]++
				} else {
					got.structure[id]++
				}
				mu.Unlock()
			}
			return f.peer.RoundTrip(request)
		})
		req := bridgeRequest{operation: OpCheck, password: append([]byte(nil), f.password...), executionOptions: options}
		id, handle := s3TestStartOperation(t, f.cfg, f.state, f.workspace, transport, req)
		result := s3TestWaitOperation(t, id, handle)
		testCheckPhase = nil
		return result, got
	}
	t.Cleanup(func() { testCheckPhase = nil })
	structure := &operationOptions{checkMode: checkReadStructure}
	all := &operationOptions{checkMode: checkReadAll}
	selected := &operationOptions{checkMode: checkReadGroup, group: uint32(dataID[0]) + 1, groups: 256}
	unselected := &operationOptions{checkMode: checkReadGroup, group: empty, groups: 256}
	for _, mode := range []struct {
		name    string
		options *operationOptions
	}{
		{"structure", structure}, {"full", all}, {"selected", selected}, {"empty", unselected}, {"legacy-full", nil},
	} {
		t.Run(mode.name, func(t *testing.T) {
			result, reads := call(mode.options)
			if result.status != StatusOk {
				t.Fatal("valid check refused", result.status, result.diagnostic)
			}
			if reads.structure[treeID] == 0 || reads.structure[dataID] != 0 {
				t.Fatal("structural traversal did not read exactly required pack type", reads)
			}
			for id := range types {
				want := mode.options == nil || mode.options.checkMode == checkReadAll || (mode.options.checkMode == checkReadGroup && uint32(id[0])+1 == mode.options.group)
				if (reads.contents[id] > 0) != want {
					t.Fatal("actual full pack-content reads disagree with requested selection", id, want, reads)
				}
			}
		})
	}
	corrupt := func(key string) func() {
		f.peer.mu.Lock()
		original := append([]byte(nil), f.peer.objects[key]...)
		changed := append([]byte(nil), original...)
		if len(changed) == 0 {
			t.Fatal("empty encrypted pack")
		}
		changed[0] ^= 0x80
		f.peer.objects[key] = changed
		f.peer.mu.Unlock()
		return func() { f.peer.mu.Lock(); f.peer.objects[key] = original; f.peer.mu.Unlock() }
	}
	repairData := corrupt(dataKey)
	for _, options := range []*operationOptions{structure, unselected} {
		result, reads := call(options)
		if result.status != StatusOk || reads.contents[dataID] != 0 {
			t.Fatal("unselected payload was read or misreported", result.status, reads)
		}
	}
	for _, options := range []*operationOptions{selected, all, nil} {
		result, reads := call(options)
		if result.status == StatusOk || reads.contents[dataID] == 0 {
			t.Fatal("selected corrupt payload escaped actual verification", result.status, reads)
		}
	}
	repairData()
	repairTree := corrupt(treeKey)
	for _, options := range []*operationOptions{structure, unselected, all, nil} {
		result, reads := call(options)
		if result.status == StatusOk || reads.structure[treeID] == 0 {
			t.Fatal("required tree corruption outside selected group escaped structural validation", result.status, reads)
		}
	}
	repairTree()
	if result, _ := call(all); result.status != StatusOk {
		t.Fatal("same-process repair/recheck failed", result.status)
	}
}
