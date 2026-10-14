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
