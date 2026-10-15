// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/restic"
)

func TestRepositoryMetadataReportsActualConfig(t *testing.T) {
	cfg := restic.Config{ID: strings.Repeat("a", 64), Version: 2, ChunkerPolynomial: 0xabcdef}
	payload, status := repositoryMetadata(cfg)
	var got struct {
		Schema     string        `json:"schema"`
		Kind       string        `json:"kind"`
		Repository restic.Config `json:"repository"`
	}
	if status != StatusOk || json.Unmarshal(payload, &got) != nil || got.Schema != metadataSchema || got.Kind != "repository-config" || got.Repository != cfg {
		t.Fatal("metadata did not preserve the actual config")
	}
	for _, invalid := range []string{"", strings.Repeat("A", 64), strings.Repeat("a", 63)} {
		cfg.ID = invalid
		if payload, status := repositoryMetadata(cfg); status == StatusOk || payload != nil {
			t.Fatal("invalid repository identity became metadata authority")
		}
	}
}

func TestSnapshotMetadataPreservesActualFieldsAndSorting(t *testing.T) {
	first, _ := restic.ParseID(strings.Repeat("a", 64))
	second, _ := restic.ParseID(strings.Repeat("b", 64))
	instant := time.Date(2026, 1, 2, 3, 4, 5, 123456789, time.FixedZone("fixture", 3600))
	sn := &data.Snapshot{Time: instant, Paths: []string{"/owner/naïve\npath"}, Tags: []string{"solstone-archive"}, Summary: &data.SnapshotSummary{TotalFilesProcessed: 3, TotalBytesProcessed: 42}}
	var collector snapshotMetadataCollector
	collector.add(second, sn)
	collector.add(first, &data.Snapshot{Time: instant, Paths: []string{"/journal"}})
	payload, status := collector.finish()
	var got struct {
		Schema    string `json:"schema"`
		Kind      string `json:"kind"`
		Snapshots []struct {
			ID       string        `json:"id"`
			Snapshot data.Snapshot `json:"snapshot"`
		} `json:"snapshots"`
	}
	if status != StatusOk || json.Unmarshal(payload, &got) != nil || got.Schema != metadataSchema || got.Kind != "snapshot-catalog" || len(got.Snapshots) != 2 {
		t.Fatal("catalog shape refused")
	}
	if got.Snapshots[0].ID != first.String() || got.Snapshots[1].ID != second.String() || !got.Snapshots[1].Snapshot.Time.Equal(instant) ||
		got.Snapshots[1].Snapshot.Paths[0] != sn.Paths[0] || got.Snapshots[1].Snapshot.Tags[0] != sn.Tags[0] ||
		got.Snapshots[1].Snapshot.Summary.TotalFilesProcessed != 3 || got.Snapshots[1].Snapshot.Summary.TotalBytesProcessed != 42 || got.Snapshots[0].Snapshot.Summary != nil {
		t.Fatal("catalog fields were invented, lost or misordered")
	}
	var empty snapshotMetadataCollector
	payload, status = empty.finish()
	if status != StatusOk || !bytes.Contains(payload, []byte(`"snapshots":[]`)) {
		t.Fatal("empty catalog is not an actual empty array")
	}
}

func TestSnapshotMetadataRefusesLossyOrOversizedOutput(t *testing.T) {
	id, _ := restic.ParseID(strings.Repeat("c", 64))
	for _, snapshot := range []*data.Snapshot{
		nil,
		{Paths: []string{string([]byte{0xff})}},
		{Tags: []string{string([]byte{0xff})}},
		{Paths: []string{strings.Repeat("x", maxResultMetadata)}},
		{Time: time.Date(10000, 1, 1, 0, 0, 0, 0, time.UTC)},
	} {
		var collector snapshotMetadataCollector
		collector.add(id, snapshot)
		if payload, status := collector.finish(); status == StatusOk || payload != nil || len(collector.rows) != 0 {
			t.Fatal("lossy or unbounded metadata accepted")
		}
	}
	collector := snapshotMetadataCollector{bytes: maxResultMetadata - 256}
	collector.add(id, &data.Snapshot{})
	collector.add(id, &data.Snapshot{Paths: []string{"ignored after refusal"}})
	if payload, status := collector.finish(); status != StatusCapExceeded || payload != nil || collector.rows != nil {
		t.Fatal("overflow retained a partial catalog")
	}
}

func TestResultMetadataRequiresActualSuccessfulRetirement(t *testing.T) {
	id := nextID.Add(1)
	h := &callHandle{id: id, state: handleRunning, doneCh: make(chan struct{}), result: bridgeResult{metadata: []byte(`{"actual":true}`)}}
	handlesMu.Lock()
	handles[id] = h
	handlesMu.Unlock()
	defer func() { handlesMu.Lock(); delete(handles, id); handlesMu.Unlock() }()
	called := false
	copyResult := func(payload []byte) int32 {
		called = true
		if string(payload) != `{"actual":true}` {
			t.Fatal("payload changed")
		}
		return StatusOk
	}
	if withResultMetadata(id, copyResult) != StatusBusy || called {
		t.Fatal("running worker published metadata")
	}
	h.mu.Lock()
	h.state = handleStopped
	close(h.doneCh)
	h.result.status = StatusCancelled
	h.mu.Unlock()
	if withResultMetadata(id, copyResult) != StatusCancelled || called {
		t.Fatal("cancelled result published successful metadata")
	}
	h.mu.Lock()
	h.result.status = StatusOk
	h.result.metadataStatus = StatusCapExceeded
	h.mu.Unlock()
	if withResultMetadata(id, copyResult) != StatusCapExceeded || called {
		t.Fatal("partial metadata published")
	}
	h.mu.Lock()
	h.result.metadataStatus = StatusOk
	h.mu.Unlock()
	if withResultMetadata(id, copyResult) != StatusOk || !called {
		t.Fatal("actual successful result unavailable")
	}
	if freeCall(id) != StatusOk || withResultMetadata(id, copyResult) != StatusInvalidHandle || h.result.metadata != nil {
		t.Fatal("freed handle retained metadata authority")
	}
}
