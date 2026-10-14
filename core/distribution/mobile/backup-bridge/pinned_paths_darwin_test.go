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

func TestDarwinAdoptedRootReadsReplacedDirectory(t *testing.T) {
	root := t.TempDir()
	original := filepath.Join(root, "source")
	moved := filepath.Join(root, "moved")
	if err := os.Mkdir(original, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(original, "owner.txt"), []byte("retained"), 0600); err != nil {
		t.Fatal(err)
	}
	pinned, err := openPinnedDirectory(original)
	if err != nil {
		t.Fatal(err)
	}
	defer pinned.Close()

	adoptedRoot, err := adoptRoot(original, pinned)
	if err != nil {
		t.Fatal(err)
	}
	defer adoptedRoot.Close()

	if err := os.Rename(original, moved); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(original, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(original, "replacement.txt"), []byte("wrong"), 0600); err != nil {
		t.Fatal(err)
	}

	data, err := adoptedRoot.ReadFile("owner.txt")
	if err != nil || string(data) != "retained" {
		t.Fatalf("adopted root redirected after directory replacement: %q %v", data, err)
	}
	if _, err := adoptedRoot.Stat("replacement.txt"); !os.IsNotExist(err) {
		t.Fatal("replacement file visible through adopted root")
	}
}
