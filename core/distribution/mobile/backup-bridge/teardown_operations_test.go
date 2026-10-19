// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
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

// A terminal ordinary inventory record provides actual non-destructive origin.
func teardownTestOwner(t *testing.T, root string, cfg s3Config, repositoryID string, ids []string) (*s3Owner, *s3TeardownAdmission) {
	t.Helper()
	owner := s3OwnerTestOpen(t, root, cfg)
	if err := owner.begin(OpListSnapshots, false, cfg, []byte("fixture-password")); err != nil {
		t.Fatal(err)
	}
	if err := owner.drainPrimary(); err != nil {
		t.Fatal(err)
	}
	if _, err := owner.complete(); err != nil {
		t.Fatal(err)
	}
	s3OwnerTestClose(t, owner)
	owner = s3OwnerTestOpen(t, root, cfg)
	ob, file, err := observeTeardownState(int(owner.directory.Fd()), owner.destination, "")
	if file != nil {
		defer file.Close()
	}
	if err != nil {
		t.Fatal(err)
	}
	ad := &s3TeardownAdmission{ConfigSHA: "sha256:" + strings.Repeat("a", 64), RepositoryID: repositoryID, Origin: ob.ID, Expected: ob}
	if _, err := ad.scope(owner.destination, ids); err != nil {
		t.Fatal(err)
	}
	return owner, ad
}

func TestTeardownObservedRetirement(t *testing.T) {
	cfg := s3TestConfig()
	state := s3OwnerTestDirectory(t)
	// This unit control covers exact-record identity using ordinary completed
	// custody. Successful opcode-11 retirement requires actual prune and drain,
	// exercised separately through the public C integration path.
	owner, _ := teardownTestOwner(t, state, cfg, strings.Repeat("b", 64), nil)
	s3OwnerTestClose(t, owner)
	parent, err := os.Open(state)
	if err != nil {
		t.Fatal(err)
	}
	defer parent.Close()
	held, ob, err := lockTeardownState(int(parent.Fd()), cfgDestination(t, cfg), "")
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	if other, _, err := lockTeardownState(int(parent.Fd()), cfgDestination(t, cfg), ""); !errors.Is(err, errS3OwnerBusy) {
		if other != nil {
			other.Close()
		}
		t.Fatal("exclusive lock not retained", err)
	}
	statePath := filepath.Join(state, s3OwnerStateName)
	raw, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatal(err)
	}
	moved := filepath.Join(t.TempDir(), "original-state")
	if err := os.Rename(statePath, moved); err != nil {
		t.Fatal(err)
	}
	if status := retireObservedTeardownState(int(held.Fd()), cfgDestination(t, cfg), ob); status == StatusOk {
		t.Fatal("missing original accepted")
	}
	if err := os.WriteFile(statePath, raw, 0600); err != nil {
		t.Fatal(err)
	}
	if status := retireObservedTeardownState(int(held.Fd()), cfgDestination(t, cfg), ob); status == StatusOk {
		t.Fatal("same-bytes replacement deleted")
	}
	if _, err := os.Stat(statePath); err != nil {
		t.Fatal("replacement did not survive", err)
	}
	if err := os.Remove(statePath); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(moved, statePath); err != nil {
		t.Fatal(err)
	}
	wrong := ob
	wrong.ID = strings.Repeat("f", 32)
	if status := retireObservedTeardownState(int(held.Fd()), cfgDestination(t, cfg), wrong); status == StatusOk {
		t.Fatal("wrong completion accepted")
	}
	if status := retireObservedTeardownState(int(held.Fd()), cfgDestination(t, cfg), ob); status != StatusOk {
		t.Fatal("original did not retire", status)
	}
	if status := retireObservedTeardownState(int(held.Fd()), cfgDestination(t, cfg), ob); status == StatusOk {
		t.Fatal("absence fabricated a receipt")
	}
	if err := unix.Flock(int(parent.Fd()), unix.LOCK_EX|unix.LOCK_NB); err == nil {
		t.Fatal("retirement released caller's lock")
	}
}

func cfgDestination(t *testing.T, cfg s3Config) s3Destination {
	t.Helper()
	destination, err := s3DestinationFor(cfg)
	if err != nil {
		t.Fatal(err)
	}
	return destination
}
