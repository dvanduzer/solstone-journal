//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"net/http"
	"strings"
	"sync"
	"testing"

	"github.com/restic/restic/internal/backend"
)

// An append-only destination makes every non-lock write create-only and
// refuses to replace an existing object; locks stay ordinary writes.
func TestS3AppendOnlyWritesAreCreateOnlyExceptLocks(t *testing.T) {
	cfg := s3TestConfig()
	cfg.appendOnly = true
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	var mu sync.Mutex
	conditional := map[string]bool{}
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method == http.MethodPut {
			mu.Lock()
			conditional[req.URL.Path] = req.Header.Get("If-None-Match") == "*"
			mu.Unlock()
		}
		return peer.RoundTrip(req)
	})
	be := s3TestBackend(t, cfg, transport)
	data := backend.Handle{Type: backend.PackFile, Name: s3TestID(41)}
	lock := backend.Handle{Type: backend.LockFile, Name: s3TestID(42)}
	save := func(handle backend.Handle, body string) error {
		return be.Save(be.ctx, handle, backend.NewByteReader([]byte(body), nil))
	}
	if err := save(data, "first ciphertext"); err != nil {
		t.Fatal("first create-only write refused", err)
	}
	if err := save(data, "replacement"); !errors.Is(err, errS3ObjectExists) {
		t.Fatal("append-only write replaced an existing object", err)
	}
	if err := save(lock, "lock one"); err != nil {
		t.Fatal("lock write refused", err)
	}
	if err := save(lock, "lock two"); err != nil {
		t.Fatal("lock refresh refused", err)
	}
	mu.Lock()
	defer mu.Unlock()
	for path, created := range conditional {
		if strings.Contains(path, "/locks/") == created {
			t.Fatal("create-only condition on the wrong object kind", path, created)
		}
	}
	peer.mu.Lock()
	defer peer.mu.Unlock()
	for key, value := range peer.objects {
		if strings.Contains(key, "/data/") && string(value) != "first ciphertext" {
			t.Fatal("existing object changed", key)
		}
	}
}

// Without append-only, an owner's own storage keeps ordinary writes.
func TestS3OwnerStorageWritesStayUnconditional(t *testing.T) {
	cfg := s3TestConfig()
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	var conditional bool
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		conditional = conditional || req.Header.Get("If-None-Match") != ""
		return peer.RoundTrip(req)
	})
	be := s3TestBackend(t, cfg, transport)
	handle := backend.Handle{Type: backend.PackFile, Name: s3TestID(43)}
	for _, body := range []string{"one", "two"} {
		if err := be.Save(be.ctx, handle, backend.NewByteReader([]byte(body), nil)); err != nil {
			t.Fatal(err)
		}
	}
	if conditional {
		t.Fatal("owner storage write carried a create-only condition")
	}
}

func TestOperationOptionsVersionTwoCarriesAppendOnlyOnlyWhenValid(t *testing.T) {
	if parsed, status := parseOperationOptions(OpSnapshot, 2, 0, 0, 0, 0, 1, 0); status != StatusOk || !parsed.appendOnly {
		t.Fatal("version 2 append-only refused", status)
	}
	for _, c := range []struct{ version, reserved1, reserved2 uint32 }{{1, 1, 0}, {2, 2, 0}, {2, 1, 1}, {3, 0, 0}, {0, 0, 0}} {
		if _, status := parseOperationOptions(OpSnapshot, c.version, 0, 0, 0, 0, c.reserved1, c.reserved2); status == StatusOk {
			t.Fatal("invalid options accepted", c)
		}
	}
}

func TestS3AppendOnlyRemoveExactKeyAllowed(t *testing.T) {
	cfg := s3TestConfig()
	cfg.appendOnly = true
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	transport := s3TestTransport(peer.RoundTrip)
	be := s3TestBackend(t, cfg, transport)
	key1ID := s3TestID(44)
	key2ID := s3TestID(45)
	key1Handle := backend.Handle{Type: backend.KeyFile, Name: key1ID}
	key2Handle := backend.Handle{Type: backend.KeyFile, Name: key2ID}
	if err := be.Save(be.ctx, key1Handle, backend.NewByteReader([]byte("key 1 data"), nil)); err != nil {
		t.Fatal("save key 1 failed", err)
	}
	if err := be.Save(be.ctx, key2Handle, backend.NewByteReader([]byte("key 2 data"), nil)); err != nil {
		t.Fatal("save key 2 failed", err)
	}
	// Direct Remove refuses non-lock
	if err := be.Remove(be.ctx, key1Handle); !errors.Is(err, errS3Denied) {
		t.Fatal("expected Remove to deny key file", err)
	}
	// Without maintenance, key object remains and error is errS3Denied
	if err := be.removeExactKeyObject(be.ctx, key1ID, false); !errors.Is(err, errS3Denied) {
		t.Fatal("expected removeExactKeyObject without maintenance to return errS3Denied", err)
	}
	if err := be.Load(be.ctx, key1Handle, 0, 0, func(rd io.Reader) error { return nil }); err != nil {
		t.Fatal("key 1 object missing after denied remove", err)
	}
	// With maintenance, that exact id is deleted
	if err := be.removeExactKeyObject(be.ctx, key1ID, true); err != nil {
		t.Fatal("removeExactKeyObject with maintenance failed", err)
	}
	if err := be.Load(be.ctx, key1Handle, 0, 0, func(rd io.Reader) error { return nil }); err == nil {
		t.Fatal("key 1 object still present after maintenance remove")
	}
	// Second key remains
	if err := be.Load(be.ctx, key2Handle, 0, 0, func(rd io.Reader) error { return nil }); err != nil {
		t.Fatal("key 2 object missing after key 1 maintenance remove", err)
	}
	// Direct Remove still denies a key file
	if err := be.Remove(be.ctx, key2Handle); !errors.Is(err, errS3Denied) {
		t.Fatal("expected Remove to deny key file", err)
	}
}
