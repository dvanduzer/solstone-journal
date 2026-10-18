// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build linux || darwin || ios

package main

import (
	"context"
	"errors"
	"github.com/restic/restic/internal/backend"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

type permissionChangingReader struct {
	backend.RewindReader
	directory string
	changed   bool
}

func (r *permissionChangingReader) Read(buf []byte) (int, error) {
	if !r.changed {
		r.changed = true
		if err := os.Chmod(r.directory, 0300); err != nil {
			return 0, err
		}
	}
	return r.RewindReader.Read(buf)
}

func TestSaveReportsPostRenameDirectoryOpenFailure(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("requires ordinary filesystem permission checks")
	}
	directory := t.TempDir()
	root, err := os.OpenRoot(directory)
	if err != nil {
		t.Fatal(err)
	}
	defer root.Close()
	be, err := createRootBackend(root)
	if err != nil {
		t.Fatal(err)
	}
	h := backend.Handle{Type: backend.ConfigFile}
	rd := &permissionChangingReader{RewindReader: backend.NewByteReader([]byte("published but not synced"), nil), directory: directory}
	defer os.Chmod(directory, 0700)
	err = be.Save(context.Background(), h, rd)
	if !errors.Is(err, os.ErrPermission) {
		t.Fatalf("save hid post-rename directory error: %v", err)
	}
	if err := os.Chmod(directory, 0700); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(filepath.Join(directory, "config"))
	if err != nil || string(data) != "published but not synced" {
		t.Fatalf("did not exercise post-rename failure: %q %v", data, err)
	}
}

func TestLocalKeyListRefusesMalformedNames(t *testing.T) {
	dir := t.TempDir()
	password := []byte("valid-test-daily-password")
	ctx := context.Background()

	initRes, err := executeInit(ctx, bridgeRequest{operation: OpInit, password: password, repoPath: dir}, func() {})
	if err != nil || initRes.status != StatusOk {
		t.Fatalf("executeInit failed: status=%d err=%v", initRes.status, err)
	}

	keysDir := filepath.Join(dir, "keys")
	malformedPath := filepath.Join(keysDir, "truncated-name")
	if err := os.WriteFile(malformedPath, []byte("malformed-key-data"), 0600); err != nil {
		t.Fatal(err)
	}

	req := bridgeRequest{
		operation: OpListRecoveryKeys,
		repoPath:  dir,
		password:  password,
	}

	res, _ := executeListRecoveryKeys(ctx, req)
	if res.status == StatusOk {
		t.Fatalf("expected non-ok status, got %d", res.status)
	}
	if len(res.metadata) != 0 {
		t.Fatalf("expected empty metadata, got %q", res.metadata)
	}
	if res.keyID != "" {
		t.Fatalf("expected empty keyID, got %q", res.keyID)
	}

	if err := os.Remove(malformedPath); err != nil {
		t.Fatal(err)
	}

	res2, err := executeListRecoveryKeys(ctx, req)
	if err != nil || res2.status != StatusOk {
		t.Fatalf("executeListRecoveryKeys failed after removal: status=%d err=%v", res2.status, err)
	}
	if res2.keyID == "" {
		t.Fatal("expected real key id, got empty")
	}
	if len(res2.metadata) == 0 {
		t.Fatal("expected metadata containing real key")
	}

	root, err := os.OpenRoot(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer root.Close()
	be, err := newRootBackend(root)
	if err != nil {
		t.Fatal(err)
	}
	if err := validateBackendKeyNames(ctx, be); err != nil {
		t.Fatalf("validateBackendKeyNames failed after removing malformed key: %v", err)
	}
}
