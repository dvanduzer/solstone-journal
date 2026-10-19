// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/restic/restic/internal/backend"
)

func TestS3OwnerAdapterWriteAheadAndExactLockAuthority(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if err := owner.begin(OpOpen, false, cfg, []byte("fixture-password")); err != nil {
		t.Fatal(err)
	}
	id := s3TestID(71)
	calls, ordered := 0, true
	base := s3TestBackend(t, cfg, s3TestTransport(func(req *http.Request) (*http.Response, error) {
		calls++
		raw, err := os.ReadFile(filepath.Join(root, s3OwnerStateName))
		var record s3OwnerRecord
		if err != nil || json.Unmarshal(raw, &record) != nil || record.Active == nil || !record.Active.MayHaveWritten || len(record.Active.Locks) != 1 || record.Active.Locks[0] != id {
			ordered = false
		}
		status := http.StatusOK
		if req.Method == http.MethodDelete {
			status = http.StatusNoContent
		}
		return &http.Response{StatusCode: status, Header: make(http.Header), Body: io.NopCloser(strings.NewReader("")), Request: req}, nil
	}))
	wrapped, err := owner.attachBackend(base)
	if err != nil {
		t.Fatal(err)
	}
	if err := wrapped.Save(context.Background(), backend.Handle{Type: backend.LockFile, Name: id}, backend.NewByteReader([]byte("opaque lock fixture"), nil)); err != nil {
		t.Fatal(err)
	}
	if calls != 1 || !ordered {
		t.Fatal("actual Save preceded durable intent/lock identity")
	}
	for _, handle := range []backend.Handle{{Type: backend.LockFile, Name: s3TestID(72)}, {Type: backend.KeyFile, Name: id}, {Type: backend.ConfigFile}} {
		if err := wrapped.Remove(context.Background(), handle); !errors.Is(err, errS3Denied) {
			t.Fatal("foreign/non-lock removal admitted", handle.Type)
		}
	}
	if calls != 1 {
		t.Fatal("refused removal reached transport")
	}
	if err := wrapped.Remove(context.Background(), backend.Handle{Type: backend.LockFile, Name: id}); err != nil || calls != 2 {
		t.Fatal("actual owned lock removal failed")
	}
	if err := owner.drainPrimary(); err != nil {
		t.Fatal(err)
	}
	if visible, err := owner.complete(); !visible || err != nil {
		t.Fatal("acknowledged owned operation did not complete", err)
	}
}

func TestS3OwnerCredentialPasswordAndLockCapsPreserveRecord(t *testing.T) {
	for _, kind := range []string{"credentials", "passwords", "locks"} {
		t.Run(kind, func(t *testing.T) {
			cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
			owner := s3OwnerTestOpen(t, root, cfg)
			if err := owner.begin(OpOpen, false, cfg, []byte("fixture-password")); err != nil {
				t.Fatal(err)
			}
			var refused func() error
			switch kind {
			case "credentials":
				for index := 1; index < maxS3OwnerCredentials; index++ {
					replacement := cfg
					replacement.secret = fmt.Sprintf("retained-fixture-secret-%d", index)
					if err := owner.begin(OpOpen, false, replacement, []byte("fixture-password")); err != nil {
						t.Fatal("valid credential cap refused", err)
					}
				}
				replacement := cfg
				replacement.secret = "over-cap-secret"
				refused = func() error { return owner.begin(OpOpen, false, replacement, []byte("fixture-password")) }
			case "passwords":
				passwords := [][]byte{[]byte("fixture-password")}
				for index := 1; index < maxS3OwnerPasswords; index++ {
					passwords = append(passwords, []byte(fmt.Sprintf("fixture-password-%d", index)))
				}
				if err := owner.begin(OpOpen, false, cfg, passwords...); err != nil {
					t.Fatal("valid password cap refused", err)
				}
				refused = func() error { return owner.begin(OpOpen, false, cfg, []byte("over-cap-password")) }
			case "locks":
				for index := 0; index < maxS3OwnerLocks; index++ {
					if err := owner.noteSave(s3TestID(index + 1)); err != nil {
						t.Fatal("valid lock cap refused", err)
					}
				}
				refused = func() error { return owner.noteSave(s3TestID(99)) }
			}
			before, err := os.ReadFile(filepath.Join(root, s3OwnerStateName))
			if err != nil {
				t.Fatal(err)
			}
			if err := refused(); !errors.Is(err, errResourceLimit) {
				t.Fatal("cap did not refuse", err)
			}
			after, err := os.ReadFile(filepath.Join(root, s3OwnerStateName))
			if err != nil || string(before) != string(after) {
				t.Fatal("cap refusal changed retained record")
			}
		})
	}
}
