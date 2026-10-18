// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"encoding/json"
	"errors"
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
	if reopened.record.Active == nil || !reopened.record.Active.MayHaveWritten || len(reopened.record.Passwords) != 2 {
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

func TestS3OwnerTeardownLifecycleAndUncertainty(t *testing.T) {
	// adoptsUncertainty(OpTeardown) is false
	if adoptsUncertainty(OpTeardown) {
		t.Fatal("expected adoptsUncertainty(OpTeardown) to be false")
	}

	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	stateFilePath := filepath.Join(root, s3OwnerStateName)

	daily := []byte("fixture-password")
	if err := owner.beginTeardown(cfg, daily); err != nil {
		t.Fatalf("beginTeardown failed: %v", err)
	}
	if err := owner.noteSave(""); err != nil {
		t.Fatalf("noteSave failed: %v", err)
	}

	if _, err := os.Stat(stateFilePath); err != nil {
		t.Fatalf("state.json missing after beginTeardown: %v", err)
	}

	// begin(OpCheck) and begin(OpSnapshot) while Active.Operation == OpTeardown and MayHaveWritten
	// return errS3OwnerPending and leave the file and Active in place
	for _, op := range []int32{OpCheck, OpSnapshot} {
		err := owner.begin(op, cfg, daily)
		if !errors.Is(err, errS3OwnerPending) {
			t.Fatalf("expected errS3OwnerPending for op %d, got: %v", op, err)
		}
		if owner.record.Active == nil || owner.record.Active.Operation != OpTeardown || !owner.record.Active.MayHaveWritten {
			t.Fatal("Active intent was modified or discarded")
		}
		if _, err := os.Stat(stateFilePath); err != nil {
			t.Fatalf("state.json was removed: %v", err)
		}
	}

	// Close leaves state.json; Close does not call Retire
	if err := owner.Close(); err != nil {
		t.Fatalf("Close failed: %v", err)
	}
	if _, err := os.Stat(stateFilePath); err != nil {
		t.Fatalf("state.json was removed by Close: %v", err)
	}

	// Retire removes state.json
	if err := owner.Retire(); err != nil {
		t.Fatalf("Retire failed: %v", err)
	}
	if _, err := os.Stat(stateFilePath); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("expected state.json to be removed by Retire, got err: %v", err)
	}
}
