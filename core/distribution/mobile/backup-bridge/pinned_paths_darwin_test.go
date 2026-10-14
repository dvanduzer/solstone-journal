//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"os"
	"path/filepath"
	"testing"

	"golang.org/x/sys/unix"
)

func TestRestoreCleanupClearsDarwinImmutableFlags(t *testing.T) {
	root := t.TempDir()
	req, stagePath := pinnedRestoreFixture(t, root)
	child := filepath.Join(stagePath, "child")
	if err := os.WriteFile(child, []byte("sensitive"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := unix.Chflags(child, unix.UF_IMMUTABLE); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("child", filepath.Join(stagePath, "child-link")); err != nil {
		t.Fatal(err)
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Lstat(stagePath); !os.IsNotExist(err) {
		t.Fatalf("flagged restore staging retained: %v", err)
	}
}
