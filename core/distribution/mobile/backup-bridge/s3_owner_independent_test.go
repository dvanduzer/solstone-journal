// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"net/http"
	"os"
	"path/filepath"
	"testing"

	"github.com/restic/restic/internal/backend"
)

func TestS3OwnerIndependentStateReplacementBeforeRenameIsPreserved(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if err := owner.begin(OpSnapshot, cfg, []byte("fixture-password")); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(root, s3OwnerStateName)
	foreign := []byte("foreign recovery custody")
	owner.fault = func(stage string) error {
		if stage != "before-rename" {
			return nil
		}
		if err := os.Remove(path); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, foreign, 0600); err != nil {
			t.Fatal(err)
		}
		return nil
	}
	err := owner.noteSave("")
	got, readErr := os.ReadFile(path)
	if err == nil || readErr != nil || !bytes.Equal(got, foreign) {
		t.Fatalf("replacement was not refused and preserved: admitted=%t preserved=%t", err == nil, readErr == nil && bytes.Equal(got, foreign))
	}
}

func TestS3OwnerIndependentSubsequentSaveDetectsStateReplacement(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if owner.begin(OpSnapshot, cfg, []byte("fixture-password")) != nil || owner.noteSave("") != nil {
		t.Fatal("setup failed")
	}
	path := filepath.Join(root, s3OwnerStateName)
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("foreign recovery custody"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := owner.noteSave(""); err == nil {
		t.Fatal("subsequent Save was admitted after its durable intent path was replaced")
	}
}

func TestS3OwnerIndependentRecoveredPendingCannotBecomeCompleted(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	first := s3OwnerTestOpen(t, root, cfg)
	if first.begin(OpInit, cfg, []byte("fixture-password")) != nil || first.noteSave("") != nil {
		t.Fatal("setup failed")
	}
	s3OwnerTestClose(t, first)
	recovered := s3OwnerTestOpen(t, root, cfg)
	if recovered.begin(OpInit, cfg, []byte("fixture-password")) == nil {
		t.Fatal("pending admission succeeded")
	}
	if err := recovered.drainPrimary(); err != nil {
		t.Fatal(err)
	}
	if published, err := recovered.complete(); published || err == nil {
		t.Fatal("restart converted a prior unknown remote write into completed state")
	}
}

func TestS3OwnerIndependentAdmittedSavePanicCannotBecomeCompleted(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if owner.begin(OpInit, cfg, []byte("fixture-password")) != nil {
		t.Fatal("setup failed")
	}
	entered := false
	base := s3TestBackend(t, cfg, s3TestTransport(func(*http.Request) (*http.Response, error) {
		entered = true
		panic("controlled panic after remote entry")
	}))
	wrapped, err := owner.attachBackend(base)
	if err != nil {
		t.Fatal(err)
	}
	func() {
		defer func() { _ = recover() }()
		_ = wrapped.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, backend.NewByteReader([]byte("ciphertext"), nil))
	}()
	if !entered {
		t.Fatal("test never reached remote entry")
	}
	if err := owner.drainPrimary(); err != nil {
		t.Fatal(err)
	}
	if published, err := owner.complete(); published || err == nil {
		t.Fatal("admitted Save panic became completed state")
	}
}

func TestS3OwnerIndependentEquivalentIPv6DestinationIsReserved(t *testing.T) {
	cfg := s3TestConfig()
	cfg.endpoint = "https://[2001:db8::1]"
	first := s3OwnerTestOpen(t, s3OwnerTestDirectory(t), cfg)
	_ = first
	cfg.endpoint = "https://[2001:0db8:0:0:0:0:0:1]:443/"
	other, err := newS3Owner(s3OwnerTestDirectory(t), cfg)
	if other != nil {
		_ = other.Close()
	}
	if err == nil {
		t.Fatal("equivalent IPv6 authority bypassed destination reservation")
	}
}
