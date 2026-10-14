//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sync"
	"testing"

	"github.com/restic/restic/internal/crypto"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
)

func TestPinnedCallablePathsDoNotFollowReplacements(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	repoPath := filepath.Join(root, "repo")
	sourcePath := filepath.Join(root, "source")
	restorePath := filepath.Join(root, "restore")
	for _, path := range []string{repoPath, sourcePath} {
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "owner.txt"), []byte("retained"), 0600); err != nil {
		t.Fatal(err)
	}
	password := []byte("pinned-callable-fixture")
	initReq := bridgeRequest{operation: OpInit, password: append([]byte(nil), password...), repoPath: repoPath}
	if err := pinRequestPaths(&initReq); err != nil {
		t.Fatal(err)
	}
	result, err := executeInit(ctx, initReq, func() {})
	closeRequestPaths(&initReq)
	if err != nil || result.status != StatusOk {
		t.Fatalf("pinned init failed: result=%+v err=%v", result, err)
	}

	snapshotReq := bridgeRequest{
		operation:  OpSnapshot,
		password:   append([]byte(nil), password...),
		repoPath:   repoPath,
		sourcePath: sourcePath,
	}
	if err := pinRequestPaths(&snapshotReq); err != nil {
		t.Fatal(err)
	}
	movedSource := filepath.Join(root, "source-retained")
	if err := os.Rename(sourcePath, movedSource); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(sourcePath, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(sourcePath, "attacker.txt"), []byte("wrong"), 0600); err != nil {
		t.Fatal(err)
	}
	result, err = executeSnapshot(
		ctx,
		snapshotReq,
		func() {},
	)
	closeRequestPaths(&snapshotReq)
	if err != nil || result.status != StatusOk {
		t.Fatalf("pinned snapshot failed: result=%+v err=%v", result, err)
	}

	snapshotID := result.snapshotID
	restoreReq := bridgeRequest{
		operation:   OpRestore,
		password:    append([]byte(nil), password...),
		repoPath:    repoPath,
		sourcePath:  sourcePath,
		restorePath: restorePath,
		snapshotID:  snapshotID,
	}
	if err := pinRequestPaths(&restoreReq); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(root, restorePath); err != nil {
		t.Fatal(err)
	}
	result, err = executeRestore(ctx, &restoreReq, func() {})
	closeRequestPaths(&restoreReq)
	if err == nil || result.status == StatusOk {
		t.Fatalf("replaced restore destination accepted: result=%+v err=%v", result, err)
	}
	if err := os.Remove(restorePath); err != nil {
		t.Fatal(err)
	}
	restoreReq = bridgeRequest{
		operation:   OpRestore,
		password:    append([]byte(nil), password...),
		repoPath:    repoPath,
		sourcePath:  sourcePath,
		restorePath: restorePath,
		snapshotID:  snapshotID,
	}
	if err := pinRequestPaths(&restoreReq); err != nil {
		t.Fatal(err)
	}
	result, err = executeRestore(ctx, &restoreReq, func() {})
	closeRequestPaths(&restoreReq)
	if err != nil || result.status != StatusOk {
		t.Fatalf("pinned restore failed after stable retry: result=%+v err=%v", result, err)
	}
	foundOwner := false
	foundAttacker := false
	var restoredPaths []string
	err = filepath.Walk(restorePath, func(path string, info os.FileInfo, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		restoredPaths = append(restoredPaths, path)
		switch info.Name() {
		case "owner.txt":
			data, readErr := os.ReadFile(path)
			if readErr != nil || string(data) != "retained" {
				return errors.New("restored owner content changed")
			}
			foundOwner = true
		case "attacker.txt":
			foundAttacker = true
		}
		return nil
	})
	if err != nil || !foundOwner || foundAttacker {
		t.Fatalf("restore followed replacement: owner=%t attacker=%t err=%v paths=%v", foundOwner, foundAttacker, err, restoredPaths)
	}
}

func TestWrongPasswordKeyScanHasRepositoryWideBound(t *testing.T) {
	ctx := context.Background()
	repoPath := filepath.Join(t.TempDir(), "repo")
	password := []byte("key-count-fixture")
	if result, err := executeInit(ctx, bridgeRequest{operation: OpInit, password: password, repoPath: repoPath}, func() {}); err != nil || result.status != StatusOk {
		t.Fatalf("init failed: result=%+v err=%v", result, err)
	}
	key := map[string]any{
		"kdf": "scrypt", "N": 2, "r": 1, "p": 1,
		"salt": bytes.Repeat([]byte{1}, 64), "data": bytes.Repeat([]byte{2}, 32),
	}
	encoded, err := json.Marshal(key)
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < maxKeyFiles; i++ {
		name := restic.Hash([]byte{byte(i)}).String()
		if err := os.WriteFile(filepath.Join(repoPath, "keys", name), encoded, 0600); err != nil {
			t.Fatal(err)
		}
	}
	result, _ := executeOpen(ctx, bridgeRequest{operation: OpOpen, password: []byte("wrong"), repoPath: repoPath})
	if result.status != StatusCapExceeded || result.diagnostic != DiagCapExceeded {
		t.Fatalf("unbounded key scan did not refuse as cap: %+v", result)
	}
}

// TestIntegrationLifecycle tests full real-repository operations and lifecycle.
func TestIntegrationLifecycle(t *testing.T) {
	tempDir := t.TempDir()
	repoPath := filepath.Join(tempDir, "repo")
	sourceName := "source"
	sourcePath := filepath.Join(tempDir, sourceName)
	restorePath := filepath.Join(tempDir, "restore")

	if err := os.Mkdir(sourcePath, 0700); err != nil {
		t.Fatal(err)
	}

	testFile := filepath.Join(sourcePath, "hello.txt")
	testData := []byte("hello solstone backup bridge!")
	if err := os.WriteFile(testFile, testData, 0600); err != nil {
		t.Fatal(err)
	}

	dailyPass := []byte("daily_secret_password_12345")
	recoveryPass := []byte("recovery_secret_password_67890")

	ctx := context.Background()

	// 1. Init
	initReq := bridgeRequest{
		operation: OpInit,
		password:  dailyPass,
		repoPath:  repoPath,
	}
	initRes, err := executeInit(ctx, initReq, func() {})
	if err != nil || initRes.status != StatusOk {
		t.Fatalf("Init failed: status=%d, diag=%s, err=%v", initRes.status, initRes.diagnostic, err)
	}
	if initRes.keyID == "" {
		t.Fatal("Init returned empty key ID")
	}

	// 2. Open with wrong password fails
	wrongReq := bridgeRequest{
		operation: OpOpen,
		password:  []byte("wrong_password"),
		repoPath:  repoPath,
	}
	wrongRes, _ := executeOpen(ctx, wrongReq)
	if wrongRes.status != StatusFailed || wrongRes.diagnostic != DiagAuthFailed {
		t.Fatalf("expected StatusFailed and DiagAuthFailed, got status=%d, diag=%s", wrongRes.status, wrongRes.diagnostic)
	}

	// 3. Open with daily password succeeds
	openReq := bridgeRequest{
		operation: OpOpen,
		password:  dailyPass,
		repoPath:  repoPath,
	}
	openRes, err := executeOpen(ctx, openReq)
	if err != nil || openRes.status != StatusOk {
		t.Fatalf("Open failed: status=%d, diag=%s, err=%v", openRes.status, openRes.diagnostic, err)
	}

	// 4. Add recovery key
	recReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		password:         dailyPass,
		recoveryPassword: recoveryPass,
		repoPath:         repoPath,
	}
	recRes, err := executeAddRecoveryKey(ctx, recReq, func() {})
	if err != nil || recRes.status != StatusOk {
		t.Fatalf("AddRecoveryKey failed: status=%d, diag=%s, err=%v", recRes.status, recRes.diagnostic, err)
	}

	// 5. Open with recovery password alone
	openRecReq := bridgeRequest{
		operation: OpOpen,
		password:  recoveryPass,
		repoPath:  repoPath,
	}
	openRecRes, err := executeOpen(ctx, openRecReq)
	if err != nil || openRecRes.status != StatusOk {
		t.Fatalf("Open with recovery key failed: status=%d, diag=%s, err=%v", openRecRes.status, openRecRes.diagnostic, err)
	}

	// 6. Snapshot 1
	snapReq := bridgeRequest{
		operation:  OpSnapshot,
		password:   dailyPass,
		repoPath:   repoPath,
		sourcePath: sourcePath,
	}
	snapRes1, err := executeSnapshot(ctx, snapReq, func() {})
	if err != nil || snapRes1.status != StatusOk {
		t.Fatalf("Snapshot 1 failed: status=%d, diag=%s, err=%v", snapRes1.status, snapRes1.diagnostic, err)
	}

	// 7. Second snapshot with different bytes
	secondFile := filepath.Join(sourcePath, "second.txt")
	if err := os.WriteFile(secondFile, []byte("second payload"), 0600); err != nil {
		t.Fatal(err)
	}
	snapRes2, err := executeSnapshot(ctx, snapReq, func() {})
	if err != nil || snapRes2.status != StatusOk {
		t.Fatalf("Snapshot 2 failed: status=%d, diag=%s, err=%v", snapRes2.status, snapRes2.diagnostic, err)
	}
	if snapRes1.snapshotID == snapRes2.snapshotID {
		t.Fatal("expected distinct snapshot IDs")
	}

	// 8. List snapshots
	listReq := bridgeRequest{
		operation: OpListSnapshots,
		password:  dailyPass,
		repoPath:  repoPath,
	}
	listRes, err := executeListSnapshots(ctx, listReq)
	if err != nil || listRes.status != StatusOk {
		t.Fatalf("ListSnapshots failed: status=%d, diag=%s, err=%v", listRes.status, listRes.diagnostic, err)
	}
	if listRes.snapshotCount != 2 {
		t.Fatalf("expected 2 snapshots, got %d", listRes.snapshotCount)
	}
	// A valid object hash does not imply that ciphertext contains a nonce/MAC.
	// Exercise the parallel snapshot loader so an upstream goroutine cannot panic.
	for _, short := range [][]byte{nil, make([]byte, 15)} {
		planted := filepath.Join(repoPath, "snapshots", restic.Hash(short).String())
		if err := os.WriteFile(planted, short, 0600); err != nil {
			t.Fatal(err)
		}
		if result, err := executeListSnapshots(ctx, listReq); err == nil || result.status == StatusOk {
			t.Fatal("truncated snapshot accepted")
		}
		if err := os.Remove(planted); err != nil {
			t.Fatal(err)
		}
	}

	// 9. Non-empty restore destination rejected
	if err := os.Mkdir(restorePath, 0700); err != nil {
		t.Fatal(err)
	}
	dummyFile := filepath.Join(restorePath, "dummy.txt")
	dummyBytes := []byte("pre-existing content")
	if err := os.WriteFile(dummyFile, dummyBytes, 0600); err != nil {
		t.Fatal(err)
	}

	restoreReq := bridgeRequest{
		operation:   OpRestore,
		password:    dailyPass,
		repoPath:    repoPath,
		sourcePath:  sourcePath,
		restorePath: restorePath,
		snapshotID:  snapRes2.snapshotID,
	}
	badRestoreRes, _ := executeRestore(ctx, &restoreReq, func() {})
	if badRestoreRes.status != StatusInvalidArgument || badRestoreRes.diagnostic != DiagRestoreDestinationExists {
		t.Fatalf("expected DiagRestoreDestinationExists, got status=%d, diag=%s", badRestoreRes.status, badRestoreRes.diagnostic)
	}
	readDummy, _ := os.ReadFile(dummyFile)
	if !bytes.Equal(readDummy, dummyBytes) {
		t.Fatal("non-empty restore modified existing file")
	}

	// Remove the caller-owned destination; publication requires an absent name.
	_ = os.RemoveAll(restorePath)

	// 10. Clean restore and byte-for-byte check
	restoreRes, err := executeRestore(ctx, &restoreReq, func() {})
	if err != nil || restoreRes.status != StatusOk {
		t.Fatalf("Restore failed: status=%d, diag=%s, err=%v", restoreRes.status, restoreRes.diagnostic, err)
	}
	restoredFile := filepath.Join(restorePath, sourceName, "hello.txt")
	restoredBytes, err := os.ReadFile(restoredFile)
	if err != nil || !bytes.Equal(restoredBytes, testData) {
		t.Fatalf("restored data mismatch: %v", err)
	}
	secondBytes, err := os.ReadFile(filepath.Join(restorePath, sourceName, "second.txt"))
	if err != nil || !bytes.Equal(secondBytes, []byte("second payload")) {
		t.Fatalf("changed snapshot missing second file: %v", err)
	}
	originalRestore := filepath.Join(tempDir, "original-restore")
	originalReq := restoreReq
	originalReq.restorePath = originalRestore
	originalReq.snapshotID = snapRes1.snapshotID
	if result, err := executeRestore(ctx, &originalReq, func() {}); err != nil || result.status != StatusOk {
		t.Fatalf("original restore failed: %v %v", result, err)
	}
	if _, err := os.Stat(filepath.Join(originalRestore, sourceName, "second.txt")); !os.IsNotExist(err) {
		t.Fatal("original snapshot acquired later content")
	}
	originalBytes, err := os.ReadFile(filepath.Join(originalRestore, sourceName, "hello.txt"))
	if err != nil || !bytes.Equal(originalBytes, testData) {
		t.Fatalf("original snapshot bytes differ: %v", err)
	}

	// 11. Check repository integrity
	checkReq := bridgeRequest{
		operation: OpCheck,
		password:  dailyPass,
		repoPath:  repoPath,
	}
	checkRes, err := executeCheck(ctx, checkReq)
	if err != nil || checkRes.status != StatusOk {
		t.Fatalf("Check failed: status=%d, diag=%s, err=%v", checkRes.status, checkRes.diagnostic, err)
	}
}

// TestPlantedShortDataKey verifies safe scan ignores keys with Data < 16 bytes.
func TestPlantedShortDataKey(t *testing.T) {
	tempDir := t.TempDir()
	repoPath := filepath.Join(tempDir, "repo")
	dailyPass := []byte("valid_password_pass")

	ctx := context.Background()

	// Init valid repo
	initReq := bridgeRequest{operation: OpInit, password: dailyPass, repoPath: repoPath}
	_, err := executeInit(ctx, initReq, func() {})
	if err != nil {
		t.Fatal(err)
	}

	// Plant short data key file
	plantedJSON := []byte(`{"created":"2026-01-01T00:00:00Z","username":"attacker","hostname":"bad","kdf":"scrypt","N":16384,"r":8,"p":1,"salt":"YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYQ==","data":"dGVzdA=="}`) // 4 bytes data
	plantedPath := filepath.Join(repoPath, "keys", restic.Hash(plantedJSON).String())
	if err := os.WriteFile(plantedPath, plantedJSON, 0600); err != nil {
		t.Fatal(err)
	}

	// Open should safely ignore the planted file without panicking
	openReq := bridgeRequest{operation: OpOpen, password: dailyPass, repoPath: repoPath}
	res, err := executeOpen(ctx, openReq)
	if err != nil || res.status != StatusOk {
		t.Fatalf("Open failed on planted short key: %v (status=%d)", err, res.status)
	}

	// Planted file must still be present
	if _, err := os.Stat(plantedPath); err != nil {
		t.Fatal("planted key file was removed")
	}
}

// TestInProgressCancel verifies in-flight cancellation cleans up repository locks.
func TestInProgressCancel(t *testing.T) {
	tempDir := t.TempDir()
	repoPath := filepath.Join(tempDir, "repo")
	sourcePath := filepath.Join(tempDir, "source")
	if err := os.Mkdir(sourcePath, 0700); err != nil {
		t.Fatal(err)
	}

	testFile := filepath.Join(sourcePath, "large.txt")
	if err := os.WriteFile(testFile, bytes.Repeat([]byte("A"), 1024*1024), 0600); err != nil {
		t.Fatal(err)
	}

	dailyPass := []byte("daily_pass_cancel")
	ctx := context.Background()

	initReq := bridgeRequest{operation: OpInit, password: dailyPass, repoPath: repoPath}
	if _, err := executeInit(ctx, initReq, func() {}); err != nil {
		t.Fatal(err)
	}

	cancelCtx, cancelFunc := context.WithCancel(context.Background())
	onReadEntered := make(chan struct{})
	blockReadCh := make(chan struct{})
	var readOnce sync.Once
	blockContentRead = func() {
		readOnce.Do(func() {
			close(onReadEntered)
			<-blockReadCh
		})
	}
	defer func() {
		blockContentRead = nil
	}()

	snapReq := bridgeRequest{
		operation:  OpSnapshot,
		password:   dailyPass,
		repoPath:   repoPath,
		sourcePath: sourcePath,
	}

	doneCh := make(chan struct{})
	var snapRes bridgeResult
	var snapErr error

	go func() {
		defer close(doneCh)
		snapRes, snapErr = executeSnapshot(cancelCtx, snapReq, func() {})
	}()

	// Wait until file Read is actively blocked
	<-onReadEntered

	// Cancel operation while inside restic
	cancelFunc()

	// Unblock Read
	close(blockReadCh)

	// Wait for worker to finish
	<-doneCh

	if snapRes.status != StatusCancelledUncertain {
		t.Fatalf("expected StatusCancelledUncertain, got status=%d (err: %v)", snapRes.status, snapErr)
	}

	// Ensure lock directory has no lingering lock files
	locksDir := filepath.Join(repoPath, "locks")
	entries, _ := os.ReadDir(locksDir)
	if len(entries) > 0 {
		t.Fatalf("locks still present after cancellation: %d", len(entries))
	}
}

// TestConcurrentFirstInit verifies process-wide addKeyMu serializes first Init calls.
func TestConcurrentFirstInit(t *testing.T) {
	tempDir := t.TempDir()
	repo1 := filepath.Join(tempDir, "repo1")
	repo2 := filepath.Join(tempDir, "repo2")
	pass := []byte("concurrency_test_password")

	var wg sync.WaitGroup
	errs := make([]error, 2)

	wg.Add(2)
	go func() {
		defer wg.Done()
		_, errs[0] = executeInit(context.Background(), bridgeRequest{operation: OpInit, password: pass, repoPath: repo1}, func() {})
	}()
	go func() {
		defer wg.Done()
		_, errs[1] = executeInit(context.Background(), bridgeRequest{operation: OpInit, password: pass, repoPath: repo2}, func() {})
	}()

	wg.Wait()
	if errs[0] != nil || errs[1] != nil {
		t.Fatalf("concurrent init failed: err0=%v, err1=%v", errs[0], errs[1])
	}
}

// TestCorruptedPackCheck verifies check fails when pack file is damaged.
func TestCorruptedPackCheck(t *testing.T) {
	tempDir := t.TempDir()
	repoPath := filepath.Join(tempDir, "repo")
	sourcePath := filepath.Join(tempDir, "source")
	_ = os.Mkdir(sourcePath, 0700)
	_ = os.WriteFile(filepath.Join(sourcePath, "file.txt"), []byte("data to check"), 0600)

	pass := []byte("check_corrupt_pass")
	ctx := context.Background()

	if result, err := executeInit(ctx, bridgeRequest{operation: OpInit, password: pass, repoPath: repoPath}, func() {}); err != nil || result.status != StatusOk {
		t.Fatalf("fixture init failed: %v %v", result, err)
	}
	if result, err := executeSnapshot(ctx, bridgeRequest{operation: OpSnapshot, password: pass, repoPath: repoPath, sourcePath: sourcePath}, func() {}); err != nil || result.status != StatusOk {
		t.Fatalf("fixture snapshot failed: %v %v", result, err)
	}
	if result, err := executeCheck(ctx, bridgeRequest{operation: OpCheck, password: pass, repoPath: repoPath}); err != nil || result.status != StatusOk {
		t.Fatalf("uncorrupted control failed: %v %v", result, err)
	}

	// Find a pack file under repo/data/
	dataDir := filepath.Join(repoPath, "data")
	var packPath string
	_ = filepath.Walk(dataDir, func(path string, info os.FileInfo, err error) error {
		if err == nil && !info.IsDir() {
			packPath = path
			return filepath.SkipAll
		}
		return nil
	})

	if packPath == "" {
		t.Fatal("no archived pack was found to corrupt")
	}
	if err := os.Chmod(packPath, 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(packPath, []byte("corrupted pack header"), 0600); err != nil {
		t.Fatal(err)
	}

	checkRes, _ := executeCheck(ctx, bridgeRequest{operation: OpCheck, password: pass, repoPath: repoPath})
	if checkRes.status != StatusFailed || checkRes.diagnostic != DiagRepoCorrupt {
		t.Fatalf("expected StatusFailed and DiagRepoCorrupt, got %d, %s", checkRes.status, checkRes.diagnostic)
	}
}

// Authenticated malformed metadata must also fail inside real parallel loaders.
func TestAuthenticatedMalformedMetadataRefusesWithoutProcessPanic(t *testing.T) {
	ctx := context.Background()
	repoPath := filepath.Join(t.TempDir(), "repo")
	req := bridgeRequest{operation: OpInit, password: []byte("synthetic-metadata-password"), repoPath: repoPath}
	if result, err := executeInit(ctx, req, func() {}); err != nil || result.status != StatusOk {
		t.Fatalf("init: %v %v", result, err)
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
	if _, err := safeScanKey(ctx, repo, string(req.password)); err != nil {
		t.Fatal(err)
	}
	id, err := data.SaveSnapshot(ctx, repo, &data.Snapshot{})
	if err != nil {
		t.Fatal(err)
	}
	checkRefusal := func() {
		copyReq := bridgeRequest{operation: OpCheck, password: append([]byte(nil), req.password...), repoPath: repoPath}
		handleID, status := startBridgeCall(copyReq)
		if status != StatusOk {
			t.Fatalf("start: %d", status)
		}
		handlesMu.Lock()
		handle := handles[handleID]
		handlesMu.Unlock()
		<-handle.doneCh
		if handle.result.status == StatusOk {
			t.Fatal("authenticated malformed metadata accepted")
		}
		if freeCall(handleID) != StatusOk {
			t.Fatal("failed worker not released")
		}
	}
	checkRefusal()
	if err := os.Remove(filepath.Join(repoPath, "snapshots", id.String())); err != nil {
		t.Fatal(err)
	}
	var malformedTreeID restic.ID
	if err := repo.WithBlobUploader(ctx, func(ctx context.Context, uploader restic.BlobSaverWithAsync) error {
		var saveErr error
		malformedTreeID, _, _, saveErr = uploader.SaveBlob(ctx, restic.TreeBlob, []byte(`{"nodes":[{"name":"broken","type":`), restic.ID{}, false)
		return saveErr
	}); err != nil {
		t.Fatal(err)
	}
	malformedSnapshotID, err := data.SaveSnapshot(ctx, repo, &data.Snapshot{Tree: &malformedTreeID})
	if err != nil {
		t.Fatal(err)
	}
	checkRefusal()
	if err := os.Remove(filepath.Join(repoPath, "snapshots", malformedSnapshotID.String())); err != nil {
		t.Fatal(err)
	}
	malformed := []byte(`{"packs":[{"id":"0000000000000000000000000000000000000000000000000000000000000001","blobs":[{"id":"0000000000000000000000000000000000000000000000000000000000000002","type":"data","offset":67108865,"length":32}]}]}`)
	nonce := crypto.NewRandomNonce()
	ciphertext := append([]byte(nil), nonce...)
	ciphertext = repo.Key().Seal(ciphertext, nonce, malformed, nil)
	if err := os.WriteFile(filepath.Join(repoPath, "index", restic.Hash(ciphertext).String()), ciphertext, 0600); err != nil {
		t.Fatal(err)
	}
	checkRefusal()
}
