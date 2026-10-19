// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"encoding/json"
	"sort"
	"time"
	"unicode/utf8"

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/restic"
)

const maxResultMetadata = 4 * 1024 * 1024
const metadataSchema = "solstone.backup-metadata.v1"

func repositoryMetadata(config restic.Config) ([]byte, int32) {
	if !lowerHexID(config.ID) {
		return nil, StatusFailed
	}
	encoded, err := json.Marshal(struct {
		Schema     string        `json:"schema"`
		Kind       string        `json:"kind"`
		Repository restic.Config `json:"repository"`
	}{metadataSchema, "repository-config", config})
	if err != nil || len(encoded) > maxResultMetadata {
		return nil, StatusInternal
	}
	return encoded, StatusOk
}

func lowerHexID(id string) bool {
	if len(id) != idHexLen {
		return false
	}
	for _, b := range []byte(id) {
		if !(b >= '0' && b <= '9' || b >= 'a' && b <= 'f') {
			return false
		}
	}
	return true
}

type snapshotMetadataRow struct {
	ID       string          `json:"id"`
	Snapshot json.RawMessage `json:"snapshot"`
}

// The collector is called under the existing snapshot-list mutex. Rows are
// encoded while each actual SDK snapshot is held, without another repository
// read. An optional-output refusal never truncates or changes the legacy IDs.
type snapshotMetadataCollector struct {
	rows   []snapshotMetadataRow
	bytes  int
	status int32
}

func (c *snapshotMetadataCollector) add(id restic.ID, snapshot *data.Snapshot) {
	if c.status != StatusOk {
		return
	}
	if snapshot == nil || !lowerHexID(id.String()) {
		c.fail(StatusFailed)
		return
	}
	textBytes := 0
	for _, values := range [][]string{snapshot.Paths, snapshot.Tags} {
		if len(values) > maxSnapshotCount {
			c.fail(StatusCapExceeded)
			return
		}
		for _, value := range values {
			if !utf8.ValidString(value) {
				c.fail(StatusFailed)
				return
			}
			if len(value) > maxResultMetadata-textBytes {
				c.fail(StatusCapExceeded)
				return
			}
			textBytes += len(value)
		}
	}
	// Bound worst-case JSON escaping before serialization (six bytes per input
	// byte). The fixed margin covers summary/time/keys and the actual ID.
	if textBytes > (maxResultMetadata-4096)/6 {
		c.fail(StatusCapExceeded)
		return
	}
	encoded, err := json.Marshal(struct {
		Time    time.Time             `json:"time"`
		Paths   []string              `json:"paths"`
		Tags    []string              `json:"tags,omitempty"`
		Summary *data.SnapshotSummary `json:"summary,omitempty"`
	}{snapshot.Time, snapshot.Paths, snapshot.Tags, snapshot.Summary})
	if err != nil {
		c.fail(StatusFailed)
		return
	}
	// Account for the wrapper/ID/comma and final envelope before retaining a row.
	rowBytes := len(encoded) + idHexLen + 64
	if len(c.rows) >= maxSnapshotCount || rowBytes > maxResultMetadata-256-c.bytes {
		c.fail(StatusCapExceeded)
		return
	}
	c.rows = append(c.rows, snapshotMetadataRow{ID: id.String(), Snapshot: encoded})
	c.bytes += rowBytes
}

func (c *snapshotMetadataCollector) fail(status int32) {
	c.rows = nil
	c.bytes = 0
	c.status = status
}

func (c *snapshotMetadataCollector) finish() ([]byte, int32) {
	if c.status != StatusOk {
		return nil, c.status
	}
	sort.Slice(c.rows, func(i, j int) bool { return c.rows[i].ID < c.rows[j].ID })
	if c.rows == nil {
		c.rows = []snapshotMetadataRow{}
	}
	encoded, err := json.Marshal(struct {
		Schema    string                `json:"schema"`
		Kind      string                `json:"kind"`
		Snapshots []snapshotMetadataRow `json:"snapshots"`
	}{metadataSchema, "snapshot-catalog", c.rows})
	if err != nil {
		return nil, StatusInternal
	}
	if len(encoded) > maxResultMetadata {
		return nil, StatusCapExceeded
	}
	return encoded, StatusOk
}

// withResultMetadata serializes result reads against actual free. It exposes
// only a successful terminal result, after worker cleanup has completed. The
// callback is bridge-owned code, never supplied by an external caller.
func withResultMetadata(id uint64, copyResult func([]byte) int32) int32 {
	handlesMu.Lock()
	h := handles[id]
	handlesMu.Unlock()
	if h == nil {
		return StatusInvalidHandle
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.state == handleFreed {
		return StatusInvalidHandle
	}
	select {
	case <-h.doneCh:
	default:
		return StatusBusy
	}
	if h.result.status != StatusOk {
		if !(h.result.status == StatusRemotePending && h.result.diagnostic == DiagRemotePending && h.result.metadataStatus == StatusOk && len(h.result.metadata) > 0 && len(h.result.metadata) <= maxResultMetadata) {
			return h.result.status
		}
		return copyResult(h.result.metadata)
	}
	if h.result.metadataStatus != StatusOk {
		return h.result.metadataStatus
	}
	if len(h.result.metadata) == 0 || len(h.result.metadata) > maxResultMetadata {
		return StatusInvalidArgument
	}
	return copyResult(h.result.metadata)
}
