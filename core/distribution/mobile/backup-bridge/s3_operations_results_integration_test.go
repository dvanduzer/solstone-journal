//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
)

func TestS3OperationsActualResultsRetainPublicationAndCleanup(t *testing.T) {
	for _, fault := range []string{"restore-pending", "restore-state-uncertain", "snapshot-pending-workspace"} {
		t.Run(fault, func(t *testing.T) {
			cfg, state, workspace := s3TestConfig(), s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
			peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
			base, err := filepath.EvalSymlinks(t.TempDir())
			if err != nil {
				t.Fatal(err)
			}
			source, destination := filepath.Join(base, "source"), filepath.Join(base, "restored")
			payload := []byte("owned synthetic result-precedence fixture")
			if os.Mkdir(source, 0700) != nil || os.WriteFile(filepath.Join(source, "record"), payload, 0600) != nil {
				t.Fatal("source fixture failed")
			}
			password := []byte("result-precedence-fixture-password")
			call := func(operation int32) bridgeResult {
				req := bridgeRequest{operation: operation, password: append([]byte(nil), password...)}
				if operation == OpSnapshot {
					req.sourcePath = source
				}
				id, h := s3TestStartOperation(t, cfg, state, workspace, peer, req)
				return s3TestWaitOperation(t, id, h)
			}
			if got := call(OpInit); got.status != StatusOk {
				t.Fatal("init failed", got.status, got.diagnostic)
			}
			snapshot := call(OpSnapshot)
			if snapshot.status != StatusOk || !validS3ID(snapshot.snapshotID) {
				t.Fatal("snapshot failed", snapshot.status, snapshot.diagnostic)
			}
			owner, err := newS3Owner(state, cfg)
			if err != nil {
				t.Fatal(err)
			}
			var snapshotAttempt atomic.Bool
			transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
				if fault == "restore-pending" && req.Method == http.MethodDelete {
					return nil, errS3IO
				}
				reply, err := peer.RoundTrip(req)
				if fault == "snapshot-pending-workspace" && req.Method == http.MethodPut && strings.Contains(req.URL.Path, "/snapshots/") {
					// An unexpected entry appearing after admission must survive
					// cleanup and remain explicit alongside the lost Save response.
					if os.WriteFile(filepath.Join(workspace, "foreign-fixture"), []byte("preserve this owned test witness"), 0600) == nil {
						snapshotAttempt.Store(true)
					}
					if reply != nil {
						_ = reply.Body.Close()
					}
					return nil, errS3IO
				}
				return reply, err
			})
			req := bridgeRequest{operation: OpRestore, password: append([]byte(nil), password...), sourcePath: source, restorePath: destination, snapshotID: snapshot.snapshotID}
			if fault == "restore-state-uncertain" {
				owner.fault = func(point string) error {
					if point == "directory-sync" && owner.record.Complete != nil {
						return errors.New("controlled final directory sync refusal")
					}
					return nil
				}
			}
			if fault == "snapshot-pending-workspace" {
				req.operation, req.restorePath, req.snapshotID = OpSnapshot, "", ""
				if os.WriteFile(filepath.Join(source, "changed"), []byte("changed"), 0600) != nil {
					t.Fatal("source change failed")
				}
			}
			id, h := s3TestStartOwnedOperation(t, cfg, owner, workspace, transport, req)
			result := s3TestWaitOperation(t, id, h)
			switch fault {
			case "restore-pending":
				if result.status != StatusRemotePending || result.diagnostic != DiagRemotePendingRestore {
					t.Fatal("committed restore erased by pending", result.status, result.diagnostic)
				}
			case "restore-state-uncertain":
				if result.status != StatusCommittedUncertain || result.diagnostic != DiagStateCommitUncertainRestore {
					t.Fatal("committed restore erased by state sync", result.status, result.diagnostic)
				}
			case "snapshot-pending-workspace":
				if !snapshotAttempt.Load() || !validS3ID(result.snapshotID) || result.status != StatusRemotePending || result.diagnostic != DiagRemotePendingWorkspace {
					t.Fatal("pending/scratch/attempt ID combination lost", result.status, result.diagnostic)
				}
				if data, err := os.ReadFile(filepath.Join(workspace, "foreign-fixture")); err != nil || string(data) != "preserve this owned test witness" {
					t.Fatal("foreign scratch witness was removed")
				}
			}
			if fault != "snapshot-pending-workspace" {
				data, err := os.ReadFile(filepath.Join(destination, "source/record"))
				if err != nil || !bytes.Equal(data, payload) {
					t.Fatal("claimed publication lacks actual bytes")
				}
			}
			reopened := s3OwnerTestOpen(t, state, cfg)
			if fault == "restore-state-uncertain" {
				if reopened.record.Complete == nil || reopened.record.Active != nil {
					t.Fatal("visible completion restart rule changed")
				}
			} else if reopened.record.Active == nil || !reopened.record.Active.MayHaveWritten {
				t.Fatal("pending result lost durable pending state")
			}
		})
	}
}
