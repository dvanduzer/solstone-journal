//go:build darwin || ios || linux

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func pinnedRestoreFixture(t *testing.T, root string) (*bridgeRequest, string) {
	t.Helper()
	parent, err := openPinnedDirectory(root)
	if err != nil {
		t.Fatal(err)
	}
	stageRootName := restoreStagePrefix + "root-fixture"
	stageRootPath := filepath.Join(root, stageRootName)
	stageName := restoreStagePrefix + "payload-fixture"
	stagePath := filepath.Join(stageRootPath, stageName)
	if err := os.Mkdir(stageRootPath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(stagePath, 0700); err != nil {
		t.Fatal(err)
	}
	stageRoot, err := openPinnedDirectory(stageRootPath)
	if err != nil {
		t.Fatal(err)
	}
	stage, err := openPinnedDirectory(stagePath)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(stageRootPath, 0300); err != nil {
		t.Fatal(err)
	}
	return &bridgeRequest{
		restoreParent: parent, restoreStageRoot: stageRoot, restoreStage: stage,
		restoreName: "restore", restoreStageRootName: stageRootName, restoreStageName: stageName,
		pathFiles: []*os.File{parent, stageRoot, stage},
	}, stagePath
}

func TestRestoreDestinationMustBeAbsent(t *testing.T) {
	destination := filepath.Join(t.TempDir(), "restore")
	if err := os.Mkdir(destination, 0700); err != nil {
		t.Fatal(err)
	}
	if !errors.Is(validateRestoreDest(destination), errRestoreExists) {
		t.Fatal("existing destination metadata could be replaced")
	}
}

func TestRestoreStageRootDoesNotExposePayloadName(t *testing.T) {
	root := t.TempDir()
	req, _ := pinnedRestoreFixture(t, root)
	stageRootPath := filepath.Join(root, req.restoreStageRootName)
	if _, err := os.ReadDir(stageRootPath); err == nil {
		t.Fatal("private staging payload name was enumerable")
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
}

func TestRestoreCleanupTraversesRestrictiveModes(t *testing.T) {
	root := t.TempDir()
	req, stagePath := pinnedRestoreFixture(t, root)
	nested := filepath.Join(stagePath, "private", "nested")
	if err := os.MkdirAll(nested, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(nested, "owner.txt"), []byte("sensitive"), 0000); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(filepath.Join(stagePath, "private"), 0000); err != nil {
		t.Fatal(err)
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Lstat(stagePath); !os.IsNotExist(err) {
		t.Fatalf("restore staging residue retained: %v", err)
	}
}

func TestRestoreCleanupFailureIsReturned(t *testing.T) {
	root := t.TempDir()
	req, _ := pinnedRestoreFixture(t, root)
	if err := req.restoreParent.Close(); err != nil {
		t.Fatal(err)
	}
	if err := closeRequestPaths(req); err == nil {
		t.Fatal("cleanup failure was suppressed")
	}
}

func TestRestorePromotionPublishesAbsentDestination(t *testing.T) {
	root := t.TempDir()
	destination := filepath.Join(root, "restore")
	req, stagePath := pinnedRestoreFixture(t, root)
	if err := os.WriteFile(filepath.Join(stagePath, "owner.txt"), []byte("restored"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := promoteRestoreStage(req); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(filepath.Join(destination, "owner.txt"))
	if err != nil || string(data) != "restored" {
		t.Fatalf("restored tree was not atomically published: %q %v", data, err)
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
}

func TestRestorePromotionNeverReplacesConcurrentDestination(t *testing.T) {
	root := t.TempDir()
	req, stagePath := pinnedRestoreFixture(t, root)
	destination := filepath.Join(root, "restore")
	if err := os.WriteFile(filepath.Join(stagePath, "restored.txt"), []byte("restored"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(destination, 0700); err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(destination, "concurrent.txt")
	if err := os.WriteFile(marker, []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := promoteRestoreStage(req); err == nil {
		t.Fatal("concurrent destination was replaced")
	}
	data, err := os.ReadFile(marker)
	if err != nil || string(data) != "keep" {
		t.Fatalf("concurrent destination changed: %q %v", data, err)
	}
	if _, err := os.Stat(filepath.Join(destination, "restored.txt")); !os.IsNotExist(err) {
		t.Fatalf("staged data became visible: %v", err)
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
}

func TestRestorePublicationUsesPinnedStageRoot(t *testing.T) {
	root := t.TempDir()
	req, stagePath := pinnedRestoreFixture(t, root)
	if err := os.WriteFile(filepath.Join(stagePath, "owner.txt"), []byte("restored"), 0600); err != nil {
		t.Fatal(err)
	}
	visibleRoot := filepath.Join(root, req.restoreStageRootName)
	movedRoot := filepath.Join(root, "moved-stage-root")
	if err := os.Rename(visibleRoot, movedRoot); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(visibleRoot, 0700); err != nil {
		t.Fatal(err)
	}
	marker := filepath.Join(visibleRoot, "replacement.txt")
	if err := os.WriteFile(marker, []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := promoteRestoreStage(req); err != nil {
		t.Fatalf("pinned staging payload was not published: %v", err)
	}
	data, err := os.ReadFile(filepath.Join(root, "restore", "owner.txt"))
	if err != nil || string(data) != "restored" {
		t.Fatalf("wrong staging tree published: %q %v", data, err)
	}
	if err := closeRequestPaths(req); !errors.Is(err, errRestoreStageChanged) {
		t.Fatalf("changed stage alias was not reported: %v", err)
	}
	if data, err := os.ReadFile(marker); err != nil || string(data) != "keep" {
		t.Fatalf("replacement stage alias was modified: %q %v", data, err)
	}
}

func TestRestorePublicationRefusesChangedPayloadAlias(t *testing.T) {
	root := t.TempDir()
	req, stagePath := pinnedRestoreFixture(t, root)
	defer os.Chmod(filepath.Dir(stagePath), 0700)
	if err := os.WriteFile(filepath.Join(stagePath, "owner.txt"), []byte("restored"), 0600); err != nil {
		t.Fatal(err)
	}
	movedPayload := filepath.Join(filepath.Dir(stagePath), "moved-payload")
	if err := os.Rename(stagePath, movedPayload); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(stagePath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(stagePath, "attacker.txt"), []byte("wrong"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := promoteRestoreStage(req); !errors.Is(err, errRestoreStageChanged) {
		t.Fatalf("changed payload alias was not refused: %v", err)
	}
	if _, err := os.Lstat(filepath.Join(root, "restore")); !os.IsNotExist(err) {
		t.Fatalf("changed payload became visible: %v", err)
	}
	if err := closeRequestPaths(req); !errors.Is(err, errRestoreStageChanged) {
		t.Fatalf("changed payload cleanup was not reported: %v", err)
	}
	if data, err := os.ReadFile(filepath.Join(stagePath, "attacker.txt")); err != nil || string(data) != "wrong" {
		t.Fatalf("replacement payload was modified: %q %v", data, err)
	}
}

func TestRestoreCancellationLinearizesBeforePublication(t *testing.T) {
	root := t.TempDir()
	req, stagePath := pinnedRestoreFixture(t, root)
	if err := os.WriteFile(filepath.Join(stagePath, "owner.txt"), []byte("restored"), 0600); err != nil {
		t.Fatal(err)
	}
	h := syntheticHandle(t)
	defer func() {
		handlesMu.Lock()
		delete(handles, h.id)
		handlesMu.Unlock()
	}()
	h.cancelRequested = true
	h.cancel()
	if err := publishRestoreForHandle(h, req); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled publication was not refused: %v", err)
	}
	if _, err := os.Lstat(filepath.Join(root, "restore")); !os.IsNotExist(err) {
		t.Fatalf("cancelled restore was published: %v", err)
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
}

func TestRestorePublicationLinearizesBeforeLaterCancellation(t *testing.T) {
	root := t.TempDir()
	req, stagePath := pinnedRestoreFixture(t, root)
	if err := os.WriteFile(filepath.Join(stagePath, "owner.txt"), []byte("restored"), 0600); err != nil {
		t.Fatal(err)
	}
	h := syntheticHandle(t)
	handlesMu.Lock()
	handles[h.id] = h
	handlesMu.Unlock()
	defer func() {
		handlesMu.Lock()
		delete(handles, h.id)
		handlesMu.Unlock()
	}()
	if err := publishRestoreForHandle(h, req); err != nil {
		t.Fatal(err)
	}
	if status := cancelCall(h.id); status != StatusOk {
		t.Fatalf("late cancel returned status %d", status)
	}
	if h.cancelRequested || !h.publicationCommitted {
		t.Fatal("late cancellation rewrote committed publication")
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
}

func TestRestoreReportsCommittedUncertainAfterPostPublishSyncFailure(t *testing.T) {
	root := t.TempDir()
	req, _ := pinnedRestoreFixture(t, root)
	originalSync := syncDirectory
	calls := 0
	syncDirectory = func(fd int) error {
		calls++
		if calls == 4 {
			return errors.New("synthetic post-publication sync failure")
		}
		return originalSync(fd)
	}
	defer func() { syncDirectory = originalSync }()
	err := promoteRestoreStage(req)
	if !errors.Is(err, errRestoreCommittedUncertain) {
		t.Fatalf("committed uncertainty not reported: %v", err)
	}
	if _, err := os.Stat(filepath.Join(root, "restore")); err != nil {
		t.Fatalf("published destination missing after commit uncertainty: %v", err)
	}
	if err := closeRequestPaths(req); err != nil {
		t.Fatal(err)
	}
}
