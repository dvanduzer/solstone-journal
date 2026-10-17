//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"net/http"
	"sync/atomic"
	"testing"
)

func TestS3OperationsOpenDistinguishesMissingRepositoryFromWrongPassword(t *testing.T) {
	cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	var writes atomic.Int32
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method == http.MethodPut || req.Method == http.MethodDelete {
			writes.Add(1)
		}
		return peer.RoundTrip(req)
	})
	open := func(password string) bridgeResult {
		id, handle := s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpOpen, password: []byte(password)})
		return s3TestWaitOperation(t, id, handle)
	}

	if result := open("fixture-password"); result.status != StatusFailed || result.diagnostic != DiagRepoMissing || writes.Load() != 0 {
		t.Fatal("empty prefix was not reported missing", result.status, result.diagnostic, writes.Load())
	}

	id, handle := s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpInit, password: []byte("fixture-password")})
	if result := s3TestWaitOperation(t, id, handle); result.status != StatusOk {
		t.Fatal("init failed", result.status, result.diagnostic)
	}
	before := writes.Load()
	if result := open("another-password"); result.status != StatusFailed || result.diagnostic != DiagAuthFailed || writes.Load() != before {
		t.Fatal("wrong password against an existing repository misclassified or mutated", result.status, result.diagnostic, writes.Load()-before)
	}
	// A successful open takes and releases its repository lock.
	if result := open("fixture-password"); result.status != StatusOk {
		t.Fatal("correct password refused", result.status, result.diagnostic)
	}
}
