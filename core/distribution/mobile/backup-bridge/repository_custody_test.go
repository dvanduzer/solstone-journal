// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build integration && (linux || darwin || ios)

package main

import (
	"bytes"
	"context"
	"fmt"
	"golang.org/x/sys/unix"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// Inspect names, regular bytes, symlink targets and xattrs independently of
// restic decryption. The scanner's positive control verifies each surface.
func rawMarkerSurfaces(rootPath, marker string) (map[string]bool, error) {
	found := map[string]bool{}
	root, err := os.OpenRoot(rootPath)
	if err != nil {
		return nil, err
	}
	defer root.Close()
	err = filepath.WalkDir(rootPath, func(path string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		rel, err := filepath.Rel(rootPath, path)
		if err != nil {
			return err
		}
		if strings.Contains(rel, marker) {
			found["name"] = true
		}
		if entry.Type()&os.ModeSymlink != 0 {
			target, err := os.Readlink(path)
			if err != nil {
				return err
			}
			if strings.Contains(target, marker) {
				found["symlink"] = true
			}
		} else if !entry.IsDir() {
			contents, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			if bytes.Contains(contents, []byte(marker)) {
				found["bytes"] = true
			}
		}
		file, err := openMetadataFile(root, rel)
		if err != nil {
			return err
		}
		attrs, readErr := readNodeFileXattrs(root, rel, file)
		closeErr := file.Close()
		if readErr != nil || closeErr != nil {
			return fmt.Errorf("raw metadata inspection: %v %v", readErr, closeErr)
		}
		for _, attr := range attrs {
			if strings.Contains(attr.Name, marker) || bytes.Contains(attr.Value, []byte(marker)) {
				found["xattr"] = true
			}
		}
		return nil
	})
	return found, err
}

func TestRawRepositoryMarkerInspectorPositiveControl(t *testing.T) {
	root := t.TempDir()
	marker := "private-marker-4969e551"
	path := filepath.Join(root, marker)
	if err := os.WriteFile(path, []byte(marker), 0600); err != nil {
		t.Fatal(err)
	}
	if err := unix.Setxattr(path, "user.fixture", []byte(marker), 0); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(marker, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	found, err := rawMarkerSurfaces(root, marker)
	if err != nil {
		t.Fatal(err)
	}
	for _, surface := range []string{"name", "bytes", "symlink", "xattr"} {
		if !found[surface] {
			t.Fatalf("marker inspector missed %s", surface)
		}
	}
}

func completeOwnedCall(t *testing.T, req bridgeRequest) bridgeResult {
	t.Helper()
	id, status := startBridgeCall(req)
	if status != StatusOk {
		t.Fatalf("start: %d", status)
	}
	handlesMu.Lock()
	handle := handles[id]
	handlesMu.Unlock()
	<-handle.doneCh
	result := handle.result
	if freeCall(id) != StatusOk {
		t.Fatal("actual worker was not drainable")
	}
	return result
}

func TestSymlinkPlaintextNeverEntersRepositoryAcrossOutcomes(t *testing.T) {
	req, path := workspaceRequest(t, "")
	defer func() { testGate = nil; os.Chmod(path, 0700) }()
	marker := "private-symlink-marker-1fbdd06c"
	if err := os.WriteFile(filepath.Join(req.sourcePath, "owner"), []byte("owner contents"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(marker, filepath.Join(req.sourcePath, "link")); err != nil {
		t.Fatal(err)
	}
	init, err := executeInit(context.Background(), bridgeRequest{operation: OpInit, password: append([]byte(nil), req.password...), repoPath: req.repoPath}, func() {})
	if err != nil || init.status != StatusOk {
		t.Fatalf("init: %+v %v", init, err)
	}
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	result := completeOwnedCall(t, req)
	assertPrivate := func(result bridgeResult) {
		t.Helper()
		found, err := rawMarkerSurfaces(req.repoPath, marker)
		if err != nil || len(found) != 0 || strings.Contains(fmt.Sprintf("%+v", result), marker) {
			t.Fatalf("plaintext marker escaped scratch: %v %v %+v", found, err, result)
		}
	}
	if result.status != StatusOk {
		t.Fatalf("snapshot failed: %+v", result)
	}
	assertPrivate(result)
	wrong := bridgeRequest{operation: OpOpen, password: []byte("wrong-private-key"), repoPath: req.repoPath}
	refused := completeOwnedCall(t, wrong)
	if refused.status == StatusOk {
		t.Fatal("wrong key accepted")
	}
	assertPrivate(refused)
	cancelledReq, _ := workspaceRequest(t, filepath.Dir(path))
	if err := pinMetadataWorkspace(&cancelledReq, path); err != nil {
		t.Fatal(err)
	}
	testGate = func(point string, _ context.Context) error {
		if point == "mutation-entered" {
			return context.Canceled
		}
		return nil
	}
	cancelled := completeOwnedCall(t, cancelledReq)
	testGate = nil
	if cancelled.status != StatusCancelled && cancelled.status != StatusCancelledUncertain {
		t.Fatalf("cancel not reported: %+v", cancelled)
	}
	assertPrivate(cancelled)
	failedReq, _ := workspaceRequest(t, filepath.Dir(path))
	if err := pinMetadataWorkspace(&failedReq, path); err != nil {
		t.Fatal(err)
	}
	testGate = func(point string, _ context.Context) error {
		if point == "before-restic" {
			seedMetadataResidue(t, failedReq.metadataRoot, marker)
			if err := os.Chmod(path, 0000); err != nil {
				return err
			}
			return context.Canceled
		}
		return nil
	}
	failed := completeOwnedCall(t, failedReq)
	testGate = nil
	defer func() { testGate = nil; os.Chmod(path, 0700) }()
	if failed.status != StatusFailed || failed.diagnostic != DiagWorkspaceCleanupRequired {
		t.Fatalf("cleanup not reported: %+v", failed)
	}
	assertPrivate(failed)
	if err := os.Chmod(path, 0700); err != nil {
		t.Fatal(err)
	}
	retry, _ := workspaceRequest(t, filepath.Dir(path))
	if err := pinMetadataWorkspace(&retry, path); err != nil {
		t.Fatal(err)
	}
	recovered := completeOwnedCall(t, retry)
	if recovered.status != StatusOk {
		t.Fatalf("ordinary retry failed: %+v", recovered)
	}
	assertPrivate(recovered)
}
