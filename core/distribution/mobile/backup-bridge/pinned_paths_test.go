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

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
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
		restoreParent: parent, restoreParentPath: root, restoreStageRoot: stageRoot, restoreStage: stage,
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
	if os.Geteuid() == 0 {
		t.Fatal("mode 0000 cleanup is not evidence when euid is 0")
	}
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
	if err := promoteRestoreStage(context.Background(), req); err != nil {
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
	if err := promoteRestoreStage(context.Background(), req); err == nil {
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
	if err := promoteRestoreStage(context.Background(), req); err != nil {
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
	if err := promoteRestoreStage(context.Background(), req); !errors.Is(err, errRestoreStageChanged) {
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
	err := promoteRestoreStage(context.Background(), req)
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

func TestMetadataOnlyOpenSurvivesPathReplacementAndNodeReStat(t *testing.T) {
	root := t.TempDir()
	sourceDir := filepath.Join(root, "source")
	if err := os.Mkdir(sourceDir, 0700); err != nil {
		t.Fatal(err)
	}
	testFile := filepath.Join(sourceDir, "file.txt")
	if err := os.WriteFile(testFile, []byte("initial"), 0600); err != nil {
		t.Fatal(err)
	}
	pinned, err := openPinnedDirectory(sourceDir)
	if err != nil {
		t.Fatal(err)
	}
	defer pinned.Close()

	adoptedRoot, err := adoptRoot(sourceDir, pinned)
	if err != nil {
		t.Fatal(err)
	}
	defer adoptedRoot.Close()

	rfs := newRootFS(adoptedRoot, "source")
	fileMeta, err := rfs.OpenFile("source/file.txt", os.O_RDONLY, true)
	if err != nil {
		t.Fatal(err)
	}
	defer fileMeta.Close()

	if err := os.WriteFile(testFile, []byte("updated larger content"), 0600); err != nil {
		t.Fatal(err)
	}

	st, err := fileMeta.Stat()
	if err != nil {
		t.Fatal(err)
	}
	if st.Size != int64(len("updated larger content")) {
		t.Fatalf("expected updated size, got %d", st.Size)
	}

	node, err := fileMeta.ToNode(false, nil)
	if err != nil {
		t.Fatal(err)
	}
	if node.Size != uint64(len("updated larger content")) {
		t.Fatalf("expected node updated size, got %d", node.Size)
	}
}

func TestInRootEscapingSymlinkContentOpenFails(t *testing.T) {
	root := t.TempDir()
	sourceDir := filepath.Join(root, "source")
	if err := os.Mkdir(sourceDir, 0700); err != nil {
		t.Fatal(err)
	}
	outsideFile := filepath.Join(root, "secret.txt")
	if err := os.WriteFile(outsideFile, []byte("secret"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outsideFile, filepath.Join(sourceDir, "link-outside")); err != nil {
		t.Fatal(err)
	}

	pinned, err := openPinnedDirectory(sourceDir)
	if err != nil {
		t.Fatal(err)
	}
	defer pinned.Close()

	adoptedRoot, err := adoptRoot(sourceDir, pinned)
	if err != nil {
		t.Fatal(err)
	}
	defer adoptedRoot.Close()

	rfs := newRootFS(adoptedRoot, "source")
	_, err = rfs.OpenFile("source/link-outside", os.O_RDONLY, false)
	if err == nil {
		t.Fatal("expected content open on symlink to fail")
	}
}

func TestSnapshotFailsWhenDirectoryUnreadableOrXattrFault(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath := filepath.Join(root, "repo")
	sourcePath := filepath.Join(root, "source")
	if err := os.Mkdir(repoPath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(sourcePath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "file.txt"), []byte("data"), 0600); err != nil {
		t.Fatal(err)
	}
	pass := []byte("fault_password")
	initReq := bridgeRequest{operation: OpInit, password: pass, repoPath: repoPath}
	if err := pinRequestPaths(&initReq); err != nil {
		t.Fatal(err)
	}
	if _, err := executeInit(ctx, initReq, func() {}); err != nil {
		closeRequestPaths(&initReq)
		t.Fatal(err)
	}
	closeRequestPaths(&initReq)

	readXattrFault = errors.New("synthetic xattr fault")
	defer func() { readXattrFault = nil }()

	snapReq := bridgeRequest{operation: OpSnapshot, password: pass, repoPath: repoPath, sourcePath: sourcePath}
	if err := pinRequestPaths(&snapReq); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&snapReq)
	res, err := executeSnapshot(ctx, snapReq, func() {})
	if err == nil || res.status == StatusOk {
		t.Fatal("expected snapshot to fail on xattr fault")
	}
}

func TestSnapshotPathsAndRootNodeOmitDescriptorOrStageNames(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath := filepath.Join(root, "repo")
	sourcePath := filepath.Join(root, "source")
	if err := os.Mkdir(repoPath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(sourcePath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "file.txt"), []byte("content"), 0600); err != nil {
		t.Fatal(err)
	}
	pass := []byte("test_password")
	initReq := bridgeRequest{operation: OpInit, password: pass, repoPath: repoPath}
	if err := pinRequestPaths(&initReq); err != nil {
		t.Fatal(err)
	}
	if _, err := executeInit(ctx, initReq, func() {}); err != nil {
		closeRequestPaths(&initReq)
		t.Fatal(err)
	}
	closeRequestPaths(&initReq)

	snapReq := bridgeRequest{operation: OpSnapshot, password: pass, repoPath: repoPath, sourcePath: sourcePath}
	if err := pinRequestPaths(&snapReq); err != nil {
		t.Fatal(err)
	}
	res, err := executeSnapshot(ctx, snapReq, func() {})
	closeRequestPaths(&snapReq)
	if err != nil || res.status != StatusOk {
		t.Fatalf("snapshot failed: %v", err)
	}

	repoFile, err := openPinnedDirectory(repoPath)
	if err != nil {
		t.Fatal(err)
	}
	defer repoFile.Close()
	repoRoot, err := adoptRoot(repoPath, repoFile)
	if err != nil {
		t.Fatal(err)
	}
	defer repoRoot.Close()
	be, err := newRootBackend(repoRoot)
	if err != nil {
		t.Fatal(err)
	}
	defer be.Close()
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := safeScanKey(ctx, repo, string(pass)); err != nil {
		t.Fatal(err)
	}
	snID, err := restic.ParseID(res.snapshotID)
	if err != nil {
		t.Fatal(err)
	}
	sn, err := data.LoadSnapshot(ctx, repo, snID)
	if err != nil {
		t.Fatal(err)
	}
	if len(sn.Paths) != 1 || sn.Paths[0] != "source" {
		t.Fatalf("expected sn.Paths to be ['source'], got %v", sn.Paths)
	}
}

func TestRestoreParentReplacedBeforeWrites(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath := filepath.Join(root, "repo")
	sourcePath := filepath.Join(root, "source")
	parentDir := filepath.Join(root, "parent")
	restorePath := filepath.Join(parentDir, "restore")
	for _, p := range []string{repoPath, sourcePath, parentDir} {
		if err := os.Mkdir(p, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "file.txt"), []byte("data"), 0600); err != nil {
		t.Fatal(err)
	}
	pass := []byte("restore_pass")
	initReq := bridgeRequest{operation: OpInit, password: pass, repoPath: repoPath}
	if err := pinRequestPaths(&initReq); err != nil {
		t.Fatal(err)
	}
	if _, err := executeInit(ctx, initReq, func() {}); err != nil {
		closeRequestPaths(&initReq)
		t.Fatal(err)
	}
	closeRequestPaths(&initReq)

	snapReq := bridgeRequest{operation: OpSnapshot, password: pass, repoPath: repoPath, sourcePath: sourcePath}
	if err := pinRequestPaths(&snapReq); err != nil {
		t.Fatal(err)
	}
	snapRes, err := executeSnapshot(ctx, snapReq, func() {})
	closeRequestPaths(&snapReq)
	if err != nil || snapRes.status != StatusOk {
		t.Fatal(err)
	}

	restoreReq := bridgeRequest{
		operation:   OpRestore,
		password:    pass,
		repoPath:    repoPath,
		sourcePath:  sourcePath,
		restorePath: restorePath,
		snapshotID:  snapRes.snapshotID,
	}
	if err := pinRequestPaths(&restoreReq); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&restoreReq)

	movedParent := filepath.Join(root, "moved-parent")
	if err := os.Rename(parentDir, movedParent); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(parentDir, 0700); err != nil {
		t.Fatal(err)
	}

	res, err := executeRestore(ctx, &restoreReq, func() {})
	if err == nil || res.status == StatusOk {
		t.Fatal("expected restore to fail after parent directory replacement before writes")
	}
}

func TestRestoreParentReplacedDuringWrites(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath := filepath.Join(root, "repo")
	sourcePath := filepath.Join(root, "source")
	parentDir := filepath.Join(root, "parent")
	restorePath := filepath.Join(parentDir, "restore")
	for _, p := range []string{repoPath, sourcePath, parentDir} {
		if err := os.Mkdir(p, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "file.txt"), []byte("data"), 0600); err != nil {
		t.Fatal(err)
	}
	pass := []byte("restore_pass")
	initReq := bridgeRequest{operation: OpInit, password: pass, repoPath: repoPath}
	if err := pinRequestPaths(&initReq); err != nil {
		t.Fatal(err)
	}
	if _, err := executeInit(ctx, initReq, func() {}); err != nil {
		closeRequestPaths(&initReq)
		t.Fatal(err)
	}
	closeRequestPaths(&initReq)

	snapReq := bridgeRequest{operation: OpSnapshot, password: pass, repoPath: repoPath, sourcePath: sourcePath}
	if err := pinRequestPaths(&snapReq); err != nil {
		t.Fatal(err)
	}
	snapRes, err := executeSnapshot(ctx, snapReq, func() {})
	closeRequestPaths(&snapReq)
	if err != nil || snapRes.status != StatusOk {
		t.Fatal(err)
	}

	restoreReq := bridgeRequest{
		operation:   OpRestore,
		password:    pass,
		repoPath:    repoPath,
		sourcePath:  sourcePath,
		restorePath: restorePath,
		snapshotID:  snapRes.snapshotID,
	}
	if err := pinRequestPaths(&restoreReq); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&restoreReq)

	testGate = func(point string, _ context.Context) error {
		if point == "restore-write" {
			moved := filepath.Join(root, "parent-moved")
			if err := os.Rename(parentDir, moved); err != nil {
				return err
			}
			if err := os.Mkdir(parentDir, 0700); err != nil {
				return err
			}
		}
		return nil
	}
	defer func() { testGate = nil }()

	res, err := executeRestore(ctx, &restoreReq, func() {})
	if err == nil || res.status == StatusOk {
		t.Fatal("expected restore to fail when parent directory is replaced during writes")
	}
}

func TestRestoreParentReplacedBeforePublication(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath := filepath.Join(root, "repo")
	sourcePath := filepath.Join(root, "source")
	parentDir := filepath.Join(root, "parent")
	restorePath := filepath.Join(parentDir, "restore")
	for _, p := range []string{repoPath, sourcePath, parentDir} {
		if err := os.Mkdir(p, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "file.txt"), []byte("data"), 0600); err != nil {
		t.Fatal(err)
	}
	pass := []byte("restore_pass")
	initReq := bridgeRequest{operation: OpInit, password: pass, repoPath: repoPath}
	if err := pinRequestPaths(&initReq); err != nil {
		t.Fatal(err)
	}
	if _, err := executeInit(ctx, initReq, func() {}); err != nil {
		closeRequestPaths(&initReq)
		t.Fatal(err)
	}
	closeRequestPaths(&initReq)

	snapReq := bridgeRequest{operation: OpSnapshot, password: pass, repoPath: repoPath, sourcePath: sourcePath}
	if err := pinRequestPaths(&snapReq); err != nil {
		t.Fatal(err)
	}
	snapRes, err := executeSnapshot(ctx, snapReq, func() {})
	closeRequestPaths(&snapReq)
	if err != nil || snapRes.status != StatusOk {
		t.Fatal(err)
	}

	restoreReq := bridgeRequest{
		operation:   OpRestore,
		password:    pass,
		repoPath:    repoPath,
		sourcePath:  sourcePath,
		restorePath: restorePath,
		snapshotID:  snapRes.snapshotID,
	}
	if err := pinRequestPaths(&restoreReq); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&restoreReq)

	testGate = func(point string, _ context.Context) error {
		if point == "before-publish" {
			moved := filepath.Join(root, "parent-moved-pub")
			if err := os.Rename(parentDir, moved); err != nil {
				return err
			}
			if err := os.Mkdir(parentDir, 0700); err != nil {
				return err
			}
		}
		return nil
	}
	defer func() { testGate = nil }()

	res, err := executeRestore(ctx, &restoreReq, func() {})
	if err == nil || res.status == StatusOk {
		t.Fatal("expected restore publication to fail after parent directory replaced before publication")
	}
}

func TestRepositoryDirectoryReplacementPreservesRetainedRepo(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath := filepath.Join(root, "repo")
	sourcePath := filepath.Join(root, "source")
	if err := os.Mkdir(repoPath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(sourcePath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "file.txt"), []byte("data"), 0600); err != nil {
		t.Fatal(err)
	}
	pass := []byte("repo_replace_pass")
	initReq := bridgeRequest{operation: OpInit, password: pass, repoPath: repoPath}
	if err := pinRequestPaths(&initReq); err != nil {
		t.Fatal(err)
	}
	if _, err := executeInit(ctx, initReq, func() {}); err != nil {
		closeRequestPaths(&initReq)
		t.Fatal(err)
	}
	closeRequestPaths(&initReq)

	snapReq := bridgeRequest{operation: OpSnapshot, password: pass, repoPath: repoPath, sourcePath: sourcePath}
	if err := pinRequestPaths(&snapReq); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&snapReq)

	movedRepo := filepath.Join(root, "repo-moved")
	if err := os.Rename(repoPath, movedRepo); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(repoPath, 0700); err != nil {
		t.Fatal(err)
	}

	res, err := executeSnapshot(ctx, snapReq, func() {})
	if err != nil || res.status != StatusOk {
		t.Fatalf("snapshot failed on retained repo: %v (result=%+v)", err, res)
	}

	entries, err := os.ReadDir(repoPath)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 0 {
		t.Fatalf("replacement repo directory was written to: %d entries", len(entries))
	}
}
