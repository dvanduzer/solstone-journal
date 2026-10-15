//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"net/http"
	"sync/atomic"
	"testing"
)

func TestS3OperationsInitFailureDistinguishesExistingFromAccessAndIO(t *testing.T) {
	for _, kind := range []string{"denied", "unavailable", "config", "keys", "snapshots"} {
		t.Run(kind, func(t *testing.T) {
			cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
			peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
			var existing string
			if kind == "config" {
				existing = cfg.prefix + "/config"
			} else if kind == "keys" || kind == "snapshots" {
				existing = cfg.prefix + "/" + kind + "/" + s3TestID(83)
			}
			payload := []byte("pre-existing opaque refusal fixture")
			if existing != "" {
				peer.objects[existing] = append([]byte(nil), payload...)
			}
			var writes atomic.Int32
			transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
				if req.Method == http.MethodPut || req.Method == http.MethodDelete {
					writes.Add(1)
				}
				if kind == "denied" {
					return s3TestResponse(403, "<Error><Code>AccessDenied</Code><Message>private-provider-fixture-secret</Message></Error>", nil), nil
				}
				if kind == "unavailable" {
					return s3TestResponse(503, "private-provider-fixture-secret", nil), nil
				}
				return peer.RoundTrip(req)
			})
			id, handle := s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpInit, password: []byte("fixture-password")})
			result := s3TestWaitOperation(t, id, handle)
			expected := DiagRepoExists
			if kind == "denied" {
				expected = DiagAuthFailed
			} else if kind == "unavailable" {
				expected = DiagIoFailed
			}
			if result.status != StatusFailed || result.diagnostic != expected || writes.Load() != 0 {
				t.Fatal("Init refusal misclassified or attempted mutation", result.status, result.diagnostic, writes.Load())
			}
			peer.mu.Lock()
			preserved := len(peer.objects) == 0 && existing == "" || len(peer.objects) == 1 && bytes.Equal(peer.objects[existing], payload)
			peer.mu.Unlock()
			if !preserved {
				t.Fatal("refusal changed existing objects")
			}
		})
	}
}
