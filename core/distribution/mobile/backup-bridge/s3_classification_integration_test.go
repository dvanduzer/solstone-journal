//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
)

func TestS3OperationsKeyListingFailureIsNotARejectedPassword(t *testing.T) {
	for _, fault := range []struct {
		name     string
		code     int
		body     string
		expected string
	}{
		{"unavailable", 503, "provider-fixture-unavailable", DiagIoFailed},
		{"denied", 403, "<Error><Code>AccessDenied</Code><Message>provider-fixture</Message></Error>", DiagAuthFailed},
		{"key-unreadable", 503, "provider-fixture-unavailable", DiagIoFailed},
	} {
		t.Run(fault.name, func(t *testing.T) {
			cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
			peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
			var failing atomic.Bool
			transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
				listing := req.URL.Query().Get("list-type") != ""
				if failing.Load() && req.Method == http.MethodGet {
					if fault.name == "key-unreadable" && !listing && strings.Contains(req.URL.Path, "/keys/") {
						return s3TestResponse(fault.code, fault.body, nil), nil
					}
					if fault.name != "key-unreadable" && strings.Contains(req.URL.Query().Get("prefix"), "/keys") {
						return s3TestResponse(fault.code, fault.body, nil), nil
					}
				}
				return peer.RoundTrip(req)
			})
			id, handle := s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpInit, password: []byte("fixture-password")})
			if result := s3TestWaitOperation(t, id, handle); result.status != StatusOk {
				t.Fatal("init failed", result.status, result.diagnostic)
			}
			failing.Store(true)
			id, handle = s3TestStartOperation(t, cfg, state, "", transport, bridgeRequest{operation: OpOpen, password: []byte("fixture-password")})
			if result := s3TestWaitOperation(t, id, handle); result.status != StatusFailed || result.diagnostic != fault.expected {
				t.Fatal("key listing failure misclassified", result.status, result.diagnostic)
			}
		})
	}
}

func TestRootedRestoreCorruptDataIsNeverReportedAsPasswordOrMissing(t *testing.T) {
	f := newRootedTestFixture(t, true)
	f.peer.mu.Lock()
	corrupted := 0
	for key, value := range f.peer.objects {
		if strings.Contains(key, "/data/") && len(value) > 64 {
			for index := 32; index < len(value); index += 97 {
				value[index] ^= 0xff
			}
			corrupted++
		}
	}
	f.peer.mu.Unlock()
	if corrupted == 0 {
		t.Fatal("fixture has no data packs")
	}
	target := filepath.Join(f.base, "corrupt-target")
	if err := os.Mkdir(target, 0700); err != nil {
		t.Fatal(err)
	}
	id, h := rootedTestStart(t, f, rootedTestRequest(t, f, target, f.second, f.recovery))
	r, _, _ := rootedTestWait(t, id, h)
	if r.status == StatusOk || r.diagnostic == DiagAuthFailed || r.diagnostic == DiagRepoMissing || r.diagnostic == DiagLocked {
		t.Fatal("corrupt repository restore misreported", r.status, r.diagnostic)
	}
}

func TestRootedRestoreFromAnEmptyPrefixIsReportedMissing(t *testing.T) {
	f := newRootedTestFixture(t, true)
	empty := *f
	empty.cfg.prefix = f.cfg.prefix + "-empty"
	empty.state = s3OwnerTestDirectory(t)
	target := filepath.Join(f.base, "empty-target")
	if err := os.Mkdir(target, 0700); err != nil {
		t.Fatal(err)
	}
	id, h := rootedTestStart(t, &empty, rootedTestRequest(t, &empty, target, f.second, f.recovery))
	if r, _, _ := rootedTestWait(t, id, h); r.status != StatusFailed || r.diagnostic != DiagRepoMissing {
		t.Fatal("restore from an empty prefix misreported", r.status, r.diagnostic)
	}
}

func TestS3MistypedPasswordsNeverExhaustTheDestination(t *testing.T) {
	cfg, state := s3TestConfig(), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	run := func(operation int32, password string) bridgeResult {
		id, handle := s3TestStartOperation(t, cfg, state, "", peer, bridgeRequest{operation: operation, password: []byte(password)})
		return s3TestWaitOperation(t, id, handle)
	}
	if r := run(OpInit, "fixture-password"); r.status != StatusOk {
		t.Fatal("init", r.status, r.diagnostic)
	}
	for attempt := 0; attempt < 2*maxS3OwnerPasswords; attempt++ {
		if r := run(OpOpen, fmt.Sprintf("mistyped-%02d", attempt)); r.diagnostic != DiagAuthFailed {
			t.Fatal("mistyped password", attempt, r.status, r.diagnostic)
		}
	}
	if r := run(OpOpen, "fixture-password"); r.status != StatusOk {
		t.Fatal("destination exhausted by mistyped passwords", r.status, r.diagnostic)
	}
}
