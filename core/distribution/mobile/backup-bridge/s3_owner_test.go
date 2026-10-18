// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func s3OwnerTestDirectory(t *testing.T) string {
	t.Helper()
	root, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil || os.Chmod(root, 0700) != nil {
		t.Fatal("private fixture directory failed")
	}
	file, err := os.Open(root)
	if err != nil {
		t.Fatal("private fixture open failed")
	}
	defer file.Close()
	if s3PrepareStateRootPolicy(int(file.Fd())) != nil {
		t.Fatal("fixture root policy failed")
	}
	return root
}

func s3OwnerTestOpen(t *testing.T, path string, cfg s3Config) *s3Owner {
	t.Helper()
	owner, err := newS3Owner(path, cfg)
	if err != nil {
		t.Fatal("valid owner refused", err)
	}
	t.Cleanup(func() {
		if err := owner.drainPrimary(); err != nil {
			t.Error("owner primary drain failed")
		}
		if err := owner.Close(); err != nil {
			t.Error("owner close failed")
		}
	})
	return owner
}

func s3OwnerTestClose(t *testing.T, owner *s3Owner) {
	t.Helper()
	if owner.drainPrimary() != nil || owner.Close() != nil {
		t.Fatal("fixture owner drain/close failed")
	}
}

func TestS3OwnerRetainsCredentialsAndKnownCompletion(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	daily, recovery := []byte("fixture-daily"), []byte("fixture-recovery")
	if owner.begin(OpInit, cfg, daily, recovery) != nil {
		t.Fatal("intent refused")
	}
	if !errors.Is(owner.Close(), errS3OwnerBusy) {
		t.Fatal("owner released before work drain")
	}
	if owner.noteSave("") != nil || owner.noteSave(s3TestID(1)) != nil {
		t.Fatal("write-ahead admission refused")
	}
	if published, err := owner.complete(); published || err == nil {
		t.Fatal("completed before actual primary drain")
	}
	if owner.drainPrimary() != nil {
		t.Fatal("drain failed")
	}
	if published, err := owner.complete(); published || !errors.Is(err, errS3OwnerPending) {
		t.Fatal("ignored unacknowledged lock")
	}
	owner.lockRemoved(s3TestID(1), nil)
	if published, err := owner.complete(); !published || err != nil {
		t.Fatal("known completion failed")
	}
	s3OwnerTestClose(t, owner)
	reopened := s3OwnerTestOpen(t, root, cfg)
	if reopened.record.Active != nil || reopened.record.Complete == nil || reopened.record.Complete.Operation != OpInit ||
		len(reopened.record.Passwords) != 2 || !bytes.Equal(reopened.record.Passwords[0], daily) || !bytes.Equal(reopened.record.Passwords[1], recovery) ||
		len(reopened.record.Credentials) != 1 || reopened.record.Credentials[0].Secret != cfg.secret {
		t.Fatal("completion or recovery custody changed")
	}
	info, err := os.Stat(filepath.Join(root, s3OwnerStateName))
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatal("state file mode changed")
	}
}

func TestS3OwnerPendingSurvivesReopenAndCleanupAcknowledgement(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if owner.begin(OpAddRecoveryKey, cfg, []byte("old-daily"), []byte("new-recovery")) != nil || owner.noteSave(s3TestID(2)) != nil {
		t.Fatal("intent failed")
	}
	owner.saveReturned(errors.New("private-provider-secret-and-path"))
	owner.lockRemoved(s3TestID(2), nil)
	if owner.drainPrimary() != nil {
		t.Fatal("drain failed")
	}
	if published, err := owner.complete(); published || !errors.Is(err, errS3OwnerPending) {
		t.Fatal("failed Save became completed")
	}
	s3OwnerTestClose(t, owner)
	reopened := s3OwnerTestOpen(t, root, cfg)
	if reopened.record.Active == nil || !reopened.record.Active.MayHaveWritten {
		t.Fatal("interruption custody missing")
	}
	var foundOldDaily, foundNewRecovery bool
	for _, p := range reopened.record.Passwords {
		if bytes.Equal(p, []byte("old-daily")) {
			foundOldDaily = true
		} else if bytes.Equal(p, []byte("new-recovery")) {
			foundNewRecovery = true
		} else {
			t.Fatalf("unexpected password in record: %s", string(p))
		}
	}
	if len(reopened.record.Passwords) != 2 || !foundOldDaily || !foundNewRecovery {
		t.Fatal("interruption custody missing")
	}
	before, err := os.ReadFile(filepath.Join(root, s3OwnerStateName))
	if err != nil {
		t.Fatal(err)
	}
	if err := reopened.begin(OpSnapshot, cfg, []byte("old-daily")); !errors.Is(err, errS3OwnerPending) {
		t.Fatal("restart reset pending state")
	}
	after, err := os.ReadFile(filepath.Join(root, s3OwnerStateName))
	if err != nil || !bytes.Equal(before, after) {
		t.Fatal("pending refusal changed state")
	}
}

func TestS3OwnerNoEgressIntentCanBeReplaced(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if owner.begin(OpOpen, cfg, []byte("retained-daily")) != nil {
		t.Fatal("intent failed")
	}
	firstID := owner.record.Active.ID
	s3OwnerTestClose(t, owner)
	reopened := s3OwnerTestOpen(t, root, cfg)
	if reopened.begin(OpOpen, cfg, []byte("retained-daily")) != nil || reopened.record.Active.ID == firstID || reopened.record.Active.MayHaveWritten {
		t.Fatal("no-egress intent could not be safely replaced")
	}
}

func TestS3OwnerCanonicalDestinationExcludesOtherDirectoryAndCredentials(t *testing.T) {
	cfg := s3TestConfig()
	root1, root2 := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root1, cfg)
	replacement := cfg
	replacement.endpoint = "https://STORAGE.example:443/"
	replacement.access, replacement.secret = "replacement-access", "replacement-secret"
	if other, err := newS3Owner(root2, replacement); !errors.Is(err, errS3OwnerBusy) || other != nil {
		if other != nil {
			_ = other.Close()
		}
		t.Fatal("same destination bypassed reservation")
	}
	independent := cfg
	independent.prefix += "-independent"
	other := s3OwnerTestOpen(t, root2, independent)
	s3OwnerTestClose(t, other)
	s3OwnerTestClose(t, owner)
	reopened := s3OwnerTestOpen(t, root2, replacement)
	if reopened == nil {
		t.Fatal("released destination did not become usable")
	}
}

func TestS3OwnerTerminalRenameAndSyncFailureRecovery(t *testing.T) {
	for _, stage := range []string{"file-sync", "before-rename", "directory-sync"} {
		t.Run(stage, func(t *testing.T) {
			cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
			owner := s3OwnerTestOpen(t, root, cfg)
			if owner.begin(OpSnapshot, cfg, []byte("retained-key")) != nil || owner.noteSave("") != nil || owner.drainPrimary() != nil {
				t.Fatal("prepared operation failed")
			}
			owner.fault = func(point string) error {
				if point == stage {
					return errors.New("injected state fault")
				}
				return nil
			}
			published, err := owner.complete()
			if err == nil || published != (stage == "directory-sync") {
				t.Fatal("wrong state publication distinction")
			}
			s3OwnerTestClose(t, owner)
			if stage == "directory-sync" {
				reopened := s3OwnerTestOpen(t, root, cfg)
				if reopened.record.Complete == nil || reopened.record.Active != nil || len(reopened.record.Passwords) != 1 {
					t.Fatal("visible completed record lost known outcomes or keys")
				}
			} else {
				// A failed pre-publication temp is retained, never silently swept.
				if other, err := newS3Owner(root, cfg); !errors.Is(err, errS3OwnerInvalid) || other != nil {
					if other != nil {
						_ = other.Close()
					}
					t.Fatal("uncommitted residue was silently discarded")
				}
				raw, err := os.ReadFile(filepath.Join(root, s3OwnerStateName))
				if err != nil {
					t.Fatal(err)
				}
				var retained s3OwnerRecord
				if json.Unmarshal(raw, &retained) != nil || retained.Active == nil || !retained.Active.MayHaveWritten {
					t.Fatal("old active record was not retained")
				}
			}
		})
	}
}

func TestS3OwnerPersistenceFaultNeverAdmitsSave(t *testing.T) {
	for _, stage := range []string{"before-secret-write", "file-sync", "before-rename", "directory-sync"} {
		t.Run(stage, func(t *testing.T) {
			cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
			owner := s3OwnerTestOpen(t, root, cfg)
			if owner.begin(OpInit, cfg, []byte("retained-key")) != nil {
				t.Fatal("intent failed")
			}
			owner.fault = func(point string) error {
				if point == stage {
					return errors.New("private failure text")
				}
				return nil
			}
			if err := owner.noteSave(s3TestID(3)); err == nil || strings.Contains(err.Error(), "private failure text") {
				t.Fatal("fault admitted Save or leaked error")
			}
			if !owner.stateFailed {
				t.Fatal("failed write-ahead state was reusable")
			}
			if err := owner.noteSave(s3TestID(4)); err == nil {
				t.Fatal("later admission ignored state failure")
			}
		})
	}
}

func TestS3OwnerStateSchemaAndFileRefusals(t *testing.T) {
	cfg, source := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, source, cfg)
	if owner.begin(OpInit, cfg, []byte("fixture-password")) != nil {
		t.Fatal("fixture state failed")
	}
	s3OwnerTestClose(t, owner)
	valid, err := os.ReadFile(filepath.Join(source, s3OwnerStateName))
	if err != nil {
		t.Fatal(err)
	}
	cases := map[string][]byte{
		"missing-write-bit": bytes.Replace(valid, []byte(`"may_have_written":false,`), nil, 1),
		"null-write-bit":    bytes.Replace(valid, []byte(`"may_have_written":false`), []byte(`"may_have_written":null`), 1),
		"duplicate":         bytes.Replace(valid, []byte(`"version":1`), []byte(`"version":1,"version":1`), 1),
		"unknown":           bytes.Replace(valid, []byte(`"version":1`), []byte(`"version":1,"unknown":false`), 1),
		"trailing":          append(append([]byte(nil), valid...), []byte(` {}`)...),
		"oversize":          bytes.Repeat([]byte("x"), maxS3OwnerBytes+1),
		"destination":       bytes.Replace(valid, []byte(`"owner-bucket"`), []byte(`"other-bucket"`), 1),
	}
	for name, raw := range cases {
		t.Run(name, func(t *testing.T) {
			root := s3OwnerTestDirectory(t)
			path := filepath.Join(root, s3OwnerStateName)
			if os.WriteFile(path, raw, 0600) != nil {
				t.Fatal("fixture write failed")
			}
			file, err := os.Open(path)
			if err != nil {
				t.Fatal(err)
			}
			if s3StateFilePolicy(int(file.Fd()), true) != nil {
				_ = file.Close()
				t.Fatal("fixture policy failed")
			}
			_ = file.Close()
			if other, err := newS3Owner(root, cfg); !errors.Is(err, errS3OwnerInvalid) || other != nil {
				if other != nil {
					_ = other.Close()
				}
				t.Fatal("malformed state admitted")
			}
			observed, err := os.ReadFile(path)
			if err != nil || !bytes.Equal(observed, raw) {
				t.Fatal("refusal altered custody")
			}
		})
	}
	for _, kind := range []string{"mode", "symlink", "hardlink", "foreign", "directory"} {
		t.Run(kind, func(t *testing.T) {
			root := s3OwnerTestDirectory(t)
			path := filepath.Join(root, s3OwnerStateName)
			switch kind {
			case "mode":
				if os.WriteFile(path, valid, 0644) != nil {
					t.Fatal("fixture failed")
				}
			case "symlink":
				if os.Symlink(filepath.Join(source, s3OwnerStateName), path) != nil {
					t.Fatal("fixture failed")
				}
			case "hardlink":
				if os.Link(filepath.Join(source, s3OwnerStateName), path) != nil {
					t.Fatal("fixture failed")
				}
				defer os.Remove(path)
			case "foreign":
				if os.WriteFile(filepath.Join(root, "foreign"), []byte("preserve"), 0600) != nil {
					t.Fatal("fixture failed")
				}
			case "directory":
				if os.Mkdir(path, 0700) != nil {
					t.Fatal("fixture failed")
				}
			}
			if other, err := newS3Owner(root, cfg); !errors.Is(err, errS3OwnerInvalid) || other != nil {
				if other != nil {
					_ = other.Close()
				}
				t.Fatal("unsafe state admitted")
			}
		})
	}
}

func TestS3OwnerAtomicExchangePreservesUnexpectedEntries(t *testing.T) {
	for _, replacement := range []string{"source", "target"} {
		t.Run(replacement, func(t *testing.T) {
			cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
			owner := s3OwnerTestOpen(t, root, cfg)
			if owner.begin(OpSnapshot, cfg, []byte("fixture-password")) != nil {
				t.Fatal("setup failed")
			}
			original, err := os.ReadFile(filepath.Join(root, s3OwnerStateName))
			if err != nil {
				t.Fatal(err)
			}
			foreign := []byte("unexpected independent custody entry")
			owner.fault = func(stage string) error {
				if stage != "atomic-replace" {
					return nil
				}
				name := s3OwnerStateName
				if replacement == "source" {
					entries, err := os.ReadDir(root)
					if err != nil {
						return err
					}
					for _, entry := range entries {
						if strings.HasPrefix(entry.Name(), s3OwnerTempPrefix) {
							name = entry.Name()
						}
					}
					if name == s3OwnerStateName {
						return errS3OwnerIO
					}
				}
				if err := os.Remove(filepath.Join(root, name)); err != nil {
					return err
				}
				return os.WriteFile(filepath.Join(root, name), foreign, 0600)
			}
			if owner.noteSave("") == nil {
				t.Fatal("unexpected exchange participant was admitted")
			}
			entries, err := os.ReadDir(root)
			if err != nil {
				t.Fatal(err)
			}
			foundForeign, foundOriginal, foundKeys := false, false, false
			for _, entry := range entries {
				raw, err := os.ReadFile(filepath.Join(root, entry.Name()))
				if err != nil {
					t.Fatal(err)
				}
				foundForeign = foundForeign || bytes.Equal(raw, foreign)
				foundOriginal = foundOriginal || bytes.Equal(raw, original)
				var retained s3OwnerRecord
				if json.Unmarshal(raw, &retained) == nil && len(retained.Passwords) == 1 && bytes.Equal(retained.Passwords[0], []byte("fixture-password")) {
					foundKeys = true
				}
			}
			if !foundForeign || !foundKeys || replacement == "source" && !foundOriginal {
				t.Fatal("exchange refusal discarded unexpected or owned custody")
			}
		})
	}
}

func TestS3OwnerInterruptedAddReconciles(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if err := owner.begin(OpAddRecoveryKey, cfg, []byte("daily-1"), []byte("candidate-1")); err != nil {
		t.Fatal("begin add failed", err)
	}
	if err := owner.noteSave(s3TestID(2)); err != nil {
		t.Fatal("noteSave add failed", err)
	}
	keyID := s3TestID(44)
	if err := owner.recordKeyID(keyID); err != nil {
		t.Fatal("recordKeyID failed", err)
	}
	s3OwnerTestClose(t, owner)

	// Reopen, begin(OpListRecoveryKeys), drain, complete
	reopened := s3OwnerTestOpen(t, root, cfg)
	if err := reopened.begin(OpListRecoveryKeys, cfg, []byte("daily-1")); err != nil {
		t.Fatalf("begin list failed: %v", err)
	}
	reopened.lockRemoved(s3TestID(2), nil)
	if err := reopened.drainPrimary(); err != nil {
		t.Fatalf("drain failed: %v", err)
	}
	published, err := reopened.complete()
	if !published || err != nil {
		t.Fatalf("complete list failed: %v", err)
	}
	s3OwnerTestClose(t, reopened)
	reopened = s3OwnerTestOpen(t, root, cfg)

	// Then begin(OpAddRecoveryKey) with candidate-2
	if err := reopened.begin(OpAddRecoveryKey, cfg, []byte("daily-1"), []byte("candidate-2")); err != nil {
		t.Fatalf("begin add retry failed: %v", err)
	}
	if reopened.record.Active == nil || reopened.record.Active.Operation != OpAddRecoveryKey {
		t.Fatal("active intent is not OpAddRecoveryKey")
	}
	if !reopened.record.Active.MayHaveWritten {
		t.Fatal("MayHaveWritten must still be true")
	}
	if !bytes.Equal(reopened.record.Active.RecoveryPassword, []byte("candidate-1")) {
		t.Fatalf("recovery password must still be candidate-1, got %q", reopened.record.Active.RecoveryPassword)
	}
	if reopened.record.Active.KeyID != keyID {
		t.Fatalf("key id changed: expected %s, got %s", keyID, reopened.record.Active.KeyID)
	}
	foundCandidate2 := false
	foundCandidate1 := false
	foundDaily1 := false
	for _, p := range reopened.record.Passwords {
		if bytes.Equal(p, []byte("candidate-2")) {
			foundCandidate2 = true
		}
		if bytes.Equal(p, []byte("candidate-1")) {
			foundCandidate1 = true
		}
		if bytes.Equal(p, []byte("daily-1")) {
			foundDaily1 = true
		}
	}
	if foundCandidate2 {
		t.Fatal("candidate-2 must be absent from record.Passwords")
	}
	if !foundCandidate1 || !foundDaily1 {
		t.Fatalf("daily-1 and candidate-1 must stay in Passwords: %v", reopened.record.Passwords)
	}
	s3OwnerTestClose(t, reopened)

	// A separate loop of 17 successful add completions, each begin then complete with a distinct
	// candidate and no prior uncertainty, leaves len(Passwords) <= maxS3OwnerPasswords and the
	// next begin does not return errResourceLimit.
	loopRoot := s3OwnerTestDirectory(t)
	for i := 0; i < 17; i++ {
		loopOwner := s3OwnerTestOpen(t, loopRoot, cfg)
		cand := []byte(fmt.Sprintf("loop-candidate-%d", i))
		if err := loopOwner.begin(OpAddRecoveryKey, cfg, []byte("daily-1"), cand); err != nil {
			t.Fatalf("loop begin add %d failed: %v", i, err)
		}
		if err := loopOwner.drainPrimary(); err != nil {
			t.Fatalf("loop drain %d failed: %v", i, err)
		}
		pub, err := loopOwner.complete()
		if !pub || err != nil {
			t.Fatalf("loop complete %d failed: %v", i, err)
		}
		s3OwnerTestClose(t, loopOwner)
	}
	checkOwner := s3OwnerTestOpen(t, loopRoot, cfg)
	if len(checkOwner.record.Passwords) > maxS3OwnerPasswords {
		t.Fatalf("passwords count %d exceeded max %d", len(checkOwner.record.Passwords), maxS3OwnerPasswords)
	}
	if err := checkOwner.begin(OpAddRecoveryKey, cfg, []byte("daily-1"), []byte("final-candidate")); err != nil {
		t.Fatalf("begin after 17 adds returned error: %v", err)
	}
	s3OwnerTestClose(t, checkOwner)
}

func TestS3OwnerInterruptedRemoveReconciles(t *testing.T) {
	if adoptsUncertainty(OpListRecoveryKeys) || adoptsUncertainty(OpRemoveRecoveryKey) {
		t.Fatal("adoptsUncertainty must be false for list and remove")
	}

	cfg := s3TestConfig()

	// Interrupted OpAddRecoveryKey remains pending for snapshot, but list inherits and preserves add intent
	addRoot := s3OwnerTestDirectory(t)
	addOwner := s3OwnerTestOpen(t, addRoot, cfg)
	if err := addOwner.begin(OpAddRecoveryKey, cfg, []byte("daily-1"), []byte("candidate-1")); err != nil {
		t.Fatalf("begin add failed: %v", err)
	}
	if err := addOwner.noteSave(s3TestID(1)); err != nil {
		t.Fatalf("noteSave add failed: %v", err)
	}
	keyID := s3TestID(44)
	if err := addOwner.recordKeyID(keyID); err != nil {
		t.Fatalf("recordKeyID failed: %v", err)
	}
	s3OwnerTestClose(t, addOwner)

	reopenedAdd := s3OwnerTestOpen(t, addRoot, cfg)
	if !errors.Is(reopenedAdd.begin(OpSnapshot, cfg), errS3OwnerPending) {
		t.Fatal("begin snapshot while add is MayHaveWritten must return errS3OwnerPending")
	}
	if reopenedAdd.record.Active == nil || reopenedAdd.record.Active.Operation != OpAddRecoveryKey {
		t.Fatal("active operation must still be OpAddRecoveryKey")
	}
	if err := reopenedAdd.begin(OpListRecoveryKeys, cfg, []byte("daily-1")); err != nil {
		t.Fatalf("begin list while add is MayHaveWritten failed: %v", err)
	}
	reopenedAdd.lockRemoved(s3TestID(1), nil)
	if err := reopenedAdd.drainPrimary(); err != nil {
		t.Fatalf("drainPrimary failed: %v", err)
	}
	published, err := reopenedAdd.complete()
	if !published || err != nil {
		t.Fatalf("complete failed: %v", err)
	}
	if reopenedAdd.record.Active == nil || reopenedAdd.record.Active.Operation != OpAddRecoveryKey {
		t.Fatal("active operation must still be OpAddRecoveryKey after complete")
	}
	if !reopenedAdd.record.Active.MayHaveWritten {
		t.Fatal("MayHaveWritten must still be true")
	}
	if reopenedAdd.record.Complete != nil {
		t.Fatal("Complete must be nil")
	}
	s3OwnerTestClose(t, reopenedAdd)
	reopenedAdd = s3OwnerTestOpen(t, root, cfg)
	if err := reopenedAdd.begin(OpAddRecoveryKey, cfg, []byte("daily-1"), []byte("candidate-2")); err != nil {
		t.Fatalf("begin OpAddRecoveryKey after list failed: %v", err)
	}
	if reopenedAdd.record.Active == nil || reopenedAdd.record.Active.Operation != OpAddRecoveryKey {
		t.Fatal("active intent is not OpAddRecoveryKey")
	}
	if !reopenedAdd.record.Active.MayHaveWritten {
		t.Fatal("MayHaveWritten must still be true")
	}
	if !bytes.Equal(reopenedAdd.record.Active.RecoveryPassword, []byte("candidate-1")) {
		t.Fatalf("recovery password must still be candidate-1, got %q", reopenedAdd.record.Active.RecoveryPassword)
	}
	if reopenedAdd.record.Active.KeyID != keyID {
		t.Fatalf("key id changed: expected %s, got %s", keyID, reopenedAdd.record.Active.KeyID)
	}
	foundCandidate2 := false
	foundCandidate1 := false
	foundDaily1 := false
	for _, p := range reopenedAdd.record.Passwords {
		if bytes.Equal(p, []byte("candidate-2")) {
			foundCandidate2 = true
		}
		if bytes.Equal(p, []byte("candidate-1")) {
			foundCandidate1 = true
		}
		if bytes.Equal(p, []byte("daily-1")) {
			foundDaily1 = true
		}
	}
	if foundCandidate2 {
		t.Fatal("candidate-2 must be absent from record.Passwords")
	}
	if !foundCandidate1 || !foundDaily1 {
		t.Fatalf("daily-1 and candidate-1 must stay in Passwords: %v", reopenedAdd.record.Passwords)
	}
	s3OwnerTestClose(t, reopenedAdd)

	// noteSave an OpRemoveRecoveryKey, reopen, assert OpSnapshot returns errS3OwnerPending
	// and active operation is still remove, then assert begin(OpListRecoveryKeys) returns nil
	// and inherits the lock
	root := s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if err := owner.begin(OpRemoveRecoveryKey, cfg, []byte("daily-1")); err != nil {
		t.Fatalf("begin remove failed: %v", err)
	}
	if err := owner.noteSave(s3TestID(2)); err != nil {
		t.Fatalf("noteSave remove failed: %v", err)
	}
	s3OwnerTestClose(t, owner)

	reopened := s3OwnerTestOpen(t, root, cfg)
	if !errors.Is(reopened.begin(OpSnapshot, cfg), errS3OwnerPending) {
		t.Fatal("begin OpSnapshot while remove is MayHaveWritten must return errS3OwnerPending")
	}
	if reopened.record.Active == nil || reopened.record.Active.Operation != OpRemoveRecoveryKey {
		t.Fatal("active operation must still be OpRemoveRecoveryKey")
	}
	if err := reopened.begin(OpListRecoveryKeys, cfg, []byte("daily-1")); err != nil {
		t.Fatalf("begin list following remove failed: %v", err)
	}
	if len(reopened.record.Active.Locks) != 1 || reopened.record.Active.Locks[0] != s3TestID(2) {
		t.Fatal("inherited lock missing")
	}
	s3OwnerTestClose(t, reopened)
}

func TestS3OwnerReplacedDirectoryIdentityRefused(t *testing.T) {
	cfg := s3TestConfig()
	base := s3OwnerTestDirectory(t)
	stateDir := filepath.Join(base, "state")
	if err := os.Mkdir(stateDir, 0700); err != nil {
		t.Fatal(err)
	}
	dirFile, err := os.Open(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	defer dirFile.Close()
	if err := s3PrepareStateRootPolicy(int(dirFile.Fd())); err != nil {
		t.Fatal(err)
	}
	oldIdentity, status := readBorrowedDirectoryIdentity(int(dirFile.Fd()))
	if status != StatusOk {
		t.Fatalf("readBorrowedDirectoryIdentity failed: %d", status)
	}

	// Replace that path with a new directory
	oldPath := filepath.Join(base, "state.old")
	if err := os.Rename(stateDir, oldPath); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(stateDir, 0700); err != nil {
		t.Fatal(err)
	}
	newDir, err := os.Open(stateDir)
	if err != nil {
		t.Fatal(err)
	}
	defer newDir.Close()
	if err := s3PrepareStateRootPolicy(int(newDir.Fd())); err != nil {
		t.Fatal(err)
	}
	liveIdentity, status := readBorrowedDirectoryIdentity(int(newDir.Fd()))
	if status != StatusOk {
		t.Fatalf("readBorrowedDirectoryIdentity for live failed: %d", status)
	}

	// First case: newS3OwnerExpected with old identity returns errS3OwnerInvalid and does not adopt replacement
	entry1, status := reserveOwnedCall()
	if status != StatusOk {
		t.Fatal("reserveOwnedCall failed", status)
	}
	owner1, err := newS3OwnerExpected(stateDir, cfg, entry1, oldIdentity)
	if !errors.Is(err, errS3OwnerInvalid) {
		t.Fatalf("expected errS3OwnerInvalid with old identity, got owner=%v err=%v", owner1, err)
	}

	// Second case: with live directory's own identity opens
	entry2, status := reserveOwnedCall()
	if status != StatusOk {
		t.Fatal("reserveOwnedCall failed", status)
	}
	owner2, err := newS3OwnerExpected(stateDir, cfg, entry2, liveIdentity)
	if err != nil {
		t.Fatalf("newS3OwnerExpected with live identity failed: %v", err)
	}
	s3OwnerTestClose(t, owner2)
}

func TestClassifyKeyOpen(t *testing.T) {
	ctx := context.Background()

	// password error: not current, not fatal
	current, err := classifyKeyOpen(ctx, false, errors.New("wrong password"), nil)
	if current || err != nil {
		t.Fatalf("expected wrong password to be not current and not fatal, got current=%v err=%v", current, err)
	}

	// ciphertext error: not current, not fatal
	current, err = classifyKeyOpen(ctx, false, errors.New("ciphertext verification failed"), nil)
	if current || err != nil {
		t.Fatalf("expected ciphertext error to be not current and not fatal, got current=%v err=%v", current, err)
	}

	// cancel: fatal
	current, err = classifyKeyOpen(ctx, false, context.Canceled, nil)
	if current || !errors.Is(err, context.Canceled) {
		t.Fatalf("expected context.Canceled to be fatal, got current=%v err=%v", current, err)
	}
	canceledCtx, cancel := context.WithCancel(ctx)
	cancel()
	current, err = classifyKeyOpen(canceledCtx, false, errors.New("wrong password"), nil)
	if current || !errors.Is(err, context.Canceled) {
		t.Fatalf("expected canceled context to be fatal, got current=%v err=%v", current, err)
	}

	// deadline: fatal
	current, err = classifyKeyOpen(ctx, false, context.DeadlineExceeded, nil)
	if current || !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded to be fatal, got current=%v err=%v", current, err)
	}

	// panic: fatal
	current, err = classifyKeyOpen(ctx, false, nil, "slice bounds out of range")
	if current || err == nil || !strings.Contains(err.Error(), "panic: slice bounds out of range") {
		t.Fatalf("expected panic to be fatal, got current=%v err=%v", current, err)
	}

	// plain I/O error: fatal
	ioErr := errors.New("connection reset by peer")
	current, err = classifyKeyOpen(ctx, false, ioErr, nil)
	if current || !errors.Is(err, ioErr) {
		t.Fatalf("expected plain I/O error to be fatal, got current=%v err=%v", current, err)
	}

	// nil error with key: current
	current, err = classifyKeyOpen(ctx, true, nil, nil)
	if !current || err != nil {
		t.Fatalf("expected nil error with key to be current and not fatal, got current=%v err=%v", current, err)
	}
}
