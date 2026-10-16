//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
)

func TestOwnedPublicLogicalWorkspaceOutcomeDoesNotSeal(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newRootedTestFixture(t, remote)
			f.source += "-retained"
			held, err := os.Open(f.source)
			if err != nil {
				t.Fatal(err)
			}
			defer held.Close()
			other, err := os.Open(f.repo)
			if err != nil {
				t.Fatal(err)
			}
			defer other.Close()
			input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password}
			if remote {
				input.remote = &f.cfg
			}
			var closers atomic.Int64
			testOwnedStage = func(stage string, entry *ownedCall) {
				if stage == "before-publication" {
					ownedControlledFactory(entry, f.peer)
				}
			}
			testOwnedClose = func(_ string, _ io.Closer, actual func() error) error { closers.Add(1); return actual() }
			t.Cleanup(func() { testOwnedStage = nil; testOwnedClose = nil })
			succeed := func() {
				id, status := sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
				if status != StatusOk || id == 0 {
					t.Fatal("healthy public source refused", status, id)
				}
				result, waited, freed := sourceCWaitAndFree(id)
				if result.status != StatusOk || waited != StatusOk || freed != StatusOk {
					t.Fatal("healthy public snapshot failed", result.status, result.diagnostic, waited, freed)
				}
			}
			succeed()
			for _, published := range []bool{false, true} {
				foreign := filepath.Join(f.workspace, "foreign-owner-fixture")
				if err := os.WriteFile(foreign, []byte("synthetic retained foreign workspace bytes"), 0600); err != nil {
					t.Fatal(err)
				}
				before := closers.Load()
				descriptor := int(other.Fd())
				if published {
					descriptor = int(held.Fd())
				}
				id, status := sourceCStart(input, descriptor, []byte(`[]`), 0)
				if published {
					if status != StatusOk || id == 0 {
						t.Fatal("logical workspace public entry did not publish", status, id)
					}
					result, waited, freed := sourceCWaitAndFree(id)
					expected := StatusFailed
					if remote {
						expected = StatusWorkspaceCleanupRequired
					}
					if result.status != expected || result.diagnostic != DiagWorkspaceCleanupRequired || waited != StatusOk || freed != StatusOk {
						t.Fatal("logical workspace terminal classification changed", remote, result.status, result.diagnostic, waited, freed)
					}
				} else if status != StatusWorkspaceCleanupRequired || id != 0 {
					t.Fatal("logical rejected workspace classification changed", status, id)
				}
				ownedCalls.Lock()
				sealed := ownedCalls.sealed
				live := len(ownedCalls.live)
				ownedCalls.Unlock()
				if sealed || live != 0 || closers.Load() <= before {
					t.Fatal("confirmed logical cleanup sealed or retained actual owner", sealed, live, closers.Load()-before)
				}
				if bytes, err := os.ReadFile(foreign); err != nil || string(bytes) != "synthetic retained foreign workspace bytes" {
					t.Fatal("logical cleanup removed foreign bytes", err)
				}
				if err := os.Remove(foreign); err != nil {
					t.Fatal(err)
				}
				succeed()
			}
			// Recognized residue can report an ordinary sync failure even though every
			// actual descriptor close succeeds. This must not become a lifetime fault.
			for _, published := range []bool{false, true} {
				name := filepath.Join(f.workspace, metadataDirectoryPrefix+strings.Repeat("c", 32))
				if err := os.Mkdir(name, 0700); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink("recognized synthetic symlink", filepath.Join(name, strings.Repeat("d", 32))); err != nil {
					t.Fatal(err)
				}
				originalSync := syncDirectory
				var consumed atomic.Bool
				syncDirectory = func(fd int) error {
					if consumed.CompareAndSwap(false, true) {
						return errors.New("synthetic ordinary workspace sync failure")
					}
					return originalSync(fd)
				}
				descriptor := int(other.Fd())
				if published {
					descriptor = int(held.Fd())
				}
				id, status := sourceCStart(input, descriptor, []byte(`[]`), 0)
				if published {
					if status != StatusOk || id == 0 {
						syncDirectory = originalSync
						t.Fatal("recognized workspace worker refused", status, id)
					}
					result, waited, freed := sourceCWaitAndFree(id)
					syncDirectory = originalSync
					expected := StatusFailed
					if remote {
						expected = StatusWorkspaceCleanupRequired
					}
					if result.status != expected || result.diagnostic != DiagWorkspaceCleanupRequired || waited != StatusOk || freed != StatusOk {
						t.Fatal("recognized logical workspace classification changed", result.status, result.diagnostic, waited, freed)
					}
				} else {
					syncDirectory = originalSync
					if status != StatusWorkspaceCleanupRequired || id != 0 {
						t.Fatal("recognized logical rejected workspace classification changed", status, id)
					}
				}
				ownedCalls.Lock()
				sealed, live := ownedCalls.sealed, len(ownedCalls.live)
				ownedCalls.Unlock()
				if !consumed.Load() || sealed || live != 0 {
					t.Fatal("recognized logical sync failure sealed or failed to exercise actual cleanup", consumed.Load(), sealed, live)
				}
				succeed()
			}
		})
	}
}
