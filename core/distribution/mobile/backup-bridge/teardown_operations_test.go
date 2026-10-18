// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import "C"

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"golang.org/x/sys/unix"
)

func TestTeardownShapeValidation(t *testing.T) {
	validID := strings.Repeat("a", 64)

	// Valid shape passes
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 0, 0, 1, []string{validID}); status != StatusOk {
		t.Fatalf("expected StatusOk, got %d", status)
	}

	// Rejects append-only (reserved1 == 1)
	if status := validateTeardownShape(OpTeardown, 1, 0, 1, 0, 0, 1, []string{validID}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for reserved1==1, got %d", status)
	}

	// Rejects reserved2 != 0
	if status := validateTeardownShape(OpTeardown, 0, 1, 1, 0, 0, 1, []string{validID}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for reserved2==1, got %d", status)
	}

	// Rejects non-empty snapshot_id
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 64, 0, 1, []string{validID}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for non-empty snapshot_id, got %d", status)
	}

	// Rejects non-empty recovery_password
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 0, 16, 1, []string{validID}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for non-empty recovery_password, got %d", status)
	}

	// Rejects operation other than 11
	for _, badOp := range []int32{OpInit, OpOpen, OpSnapshot, OpRestore, OpCheck, 10, 12} {
		if status := validateTeardownShape(badOp, 0, 0, 1, 0, 0, 1, []string{validID}); status != StatusInvalidArgument {
			t.Fatalf("expected StatusInvalidArgument for op %d, got %d", badOp, status)
		}
	}

	// Rejects bad id: wrong length
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 0, 0, 1, []string{"abc"}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for short id, got %d", status)
	}

	// Rejects bad id: uppercase hex
	badHexID := strings.Repeat("A", 64)
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 0, 0, 1, []string{badHexID}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for uppercase hex id, got %d", status)
	}

	// Rejects bad id: non-hex
	badCharID := strings.Repeat("z", 64)
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 0, 0, 1, []string{badCharID}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for non-hex id, got %d", status)
	}

	// Rejects duplicate id
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 0, 0, 2, []string{validID, validID}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for duplicate id, got %d", status)
	}

	// Rejects count > maxSnapshotCount
	if status := validateTeardownShape(OpTeardown, 0, 0, 1, 0, 0, maxSnapshotCount+1, []string{}); status != StatusInvalidArgument {
		t.Fatalf("expected StatusInvalidArgument for count > maxSnapshotCount, got %d", status)
	}

	// parseOperationOptions rejects operation 11
	if _, status := parseOperationOptions(OpTeardown, 1, 0, 0, 0, 0, 0, 0); status != StatusInvalidArgument {
		t.Fatalf("expected parseOperationOptions to reject op 11, got %d", status)
	}

	// validateS3ShapeForRoot rejects operation 11
	var emptyFields [14]requestField
	if status := validateS3ShapeForRoot(1, OpTeardown, emptyFields, false); status != StatusInvalidArgument {
		t.Fatalf("expected validateS3ShapeForRoot to reject op 11, got %d", status)
	}
	if status := validateS3ShapeForRoot(1, OpTeardown, emptyFields, true); status != StatusInvalidArgument {
		t.Fatalf("expected validateS3ShapeForRoot (rooted) to reject op 11, got %d", status)
	}
}

func TestS3RetireV1(t *testing.T) {
	tempDir := t.TempDir()
	if err := os.Chmod(tempDir, 0700); err != nil {
		t.Fatal(err)
	}
	statePath := filepath.Join(tempDir, s3OwnerStateName)
	record := s3OwnerRecord{
		Version:     1,
		Destination: s3Destination{},
		Active:      nil,
	}
	raw, err := json.Marshal(record)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(statePath, raw, 0600); err != nil {
		t.Fatal(err)
	}

	f1, err := os.Open(tempDir)
	if err != nil {
		t.Fatal(err)
	}
	defer f1.Close()

	f2, err := os.Open(tempDir)
	if err != nil {
		t.Fatal(err)
	}

	if err := unix.Flock(int(f1.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		t.Fatal(err)
	}

	status := restic_bridge_s3_retire_v1(C.int(f2.Fd()))
	if int32(status) != StatusBusy {
		t.Fatalf("expected StatusBusy while locked, got %d", status)
	}
	if _, err := os.Stat(statePath); err != nil {
		t.Fatalf("expected state.json to remain while busy, got: %v", err)
	}

	if err := unix.Flock(int(f1.Fd()), unix.LOCK_UN); err != nil {
		t.Fatal(err)
	}
	_ = f1.Close()

	status = restic_bridge_s3_retire_v1(C.int(f2.Fd()))
	if int32(status) != StatusOk {
		t.Fatalf("expected StatusOk after lock released, got %d", status)
	}
	if _, err := os.Stat(statePath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("expected state.json to be gone, got: %v", err)
	}
	var stat unix.Stat_t
	if err := unix.Fstat(int(f2.Fd()), &stat); err != nil {
		t.Fatalf("expected second fd to still Fstat, got: %v", err)
	}
	if err := f2.Close(); err != nil {
		t.Fatal(err)
	}

	activeRecord := s3OwnerRecord{
		Version:     1,
		Destination: s3Destination{},
		Active: &s3OwnerIntent{
			ID:        "active-intent-id",
			Operation: OpTeardown,
		},
	}
	activeRaw, err := json.Marshal(activeRecord)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(statePath, activeRaw, 0600); err != nil {
		t.Fatal(err)
	}

	f3, err := os.Open(tempDir)
	if err != nil {
		t.Fatal(err)
	}
	defer f3.Close()

	status = restic_bridge_s3_retire_v1(C.int(f3.Fd()))
	if int32(status) != StatusFailed {
		t.Fatalf("expected StatusFailed when Active non-nil, got %d", status)
	}
	if _, err := os.Stat(statePath); err != nil {
		t.Fatalf("expected state.json to remain when Active non-nil, got: %v", err)
	}
}
