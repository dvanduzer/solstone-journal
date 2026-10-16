// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestSourceIdentityBorrowedCaptureAndAdmissibleMismatch(t *testing.T) {
	base := t.TempDir()
	original := filepath.Join(base, "journal")
	other := filepath.Join(base, "other")
	for _, p := range []string{original, other} {
		if err := os.Mkdir(p, 0755); err != nil {
			t.Fatal(err)
		}
	}
	file, err := os.Open(original)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	expected, status := readBorrowedSourceIdentity(int(file.Fd()))
	if status != StatusOk {
		t.Fatal(status)
	}
	root, err := os.OpenRoot(original)
	if err != nil {
		t.Fatal(err)
	}
	defer root.Close()
	req := bridgeRequest{operation: OpSnapshot, sourceRoot: root}
	if verifyCapturedSource(&req, expected) != StatusOk {
		t.Fatal("matching ordinary-permission source refused")
	}
	wrong, err := os.Open(other)
	if err != nil {
		t.Fatal(err)
	}
	defer wrong.Close()
	mismatch, status := readBorrowedSourceIdentity(int(wrong.Fd()))
	if status != StatusOk {
		t.Fatal("otherwise-admissible mismatch did not reach identity gate")
	}
	if verifyCapturedSource(&req, mismatch) != StatusInvalidArgument {
		t.Fatal("different directory became source authority")
	}
	if _, err := file.Stat(); err != nil {
		t.Fatal("borrowed descriptor was closed", err)
	}
}

func TestSourceIdentityCapturedRootDefeatsPathSwapAndReturn(t *testing.T) {
	base := t.TempDir()
	source := filepath.Join(base, "journal")
	retained := source + "-retained"
	replacement := source + "-replacement"
	if err := os.Mkdir(source, 0700); err != nil {
		t.Fatal(err)
	}
	held, err := os.Open(source)
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	expected, status := readBorrowedSourceIdentity(int(held.Fd()))
	if status != StatusOk {
		t.Fatal(status)
	}
	if err := os.Rename(source, retained); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(source, 0700); err != nil {
		t.Fatal(err)
	}
	pinned, err := openPinnedDirectory(source)
	if err != nil {
		t.Fatal(err)
	}
	defer pinned.Close()
	captured, err := adoptRoot(source, pinned)
	if err != nil {
		t.Fatal(err)
	}
	defer captured.Close()
	a, err := pinned.Stat()
	if err != nil {
		t.Fatal(err)
	}
	b, err := captured.Stat(".")
	if err != nil || !os.SameFile(a, b) {
		t.Fatal("replacement captures were not proven")
	}
	if err := os.Rename(source, replacement); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(retained, source); err != nil {
		t.Fatal(err)
	}
	current, err := os.Stat(source)
	if err != nil {
		t.Fatal(err)
	}
	original, err := held.Stat()
	if err != nil || !os.SameFile(current, original) {
		t.Fatal("path did not return to original")
	}
	if verifyCapturedSource(&bridgeRequest{operation: OpSnapshot, sourcePath: source, sourceRoot: captured}, expected) != StatusInvalidArgument {
		t.Fatal("pathname ABA admitted captured replacement")
	}
}

func TestSourceIdentityRefusesFileClosedAndNegativeDescriptors(t *testing.T) {
	file, err := os.CreateTemp(t.TempDir(), "regular")
	if err != nil {
		t.Fatal(err)
	}
	if _, status := readBorrowedSourceIdentity(int(file.Fd())); status != StatusInvalidArgument {
		t.Fatal("regular file admitted")
	}
	fd := int(file.Fd())
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	for _, fd := range []int{fd, -1} {
		if _, status := readBorrowedSourceIdentity(fd); status != StatusInvalidArgument {
			t.Fatal("closed/negative descriptor admitted")
		}
	}
}
