// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build integration && (darwin || ios)

package main

import (
	"context"
	"crypto/sha256"
	"fmt"
	"golang.org/x/sys/unix"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func repositoryFixtureDigest(t *testing.T, root string) string {
	t.Helper()
	hash := sha256.New()
	err := filepath.WalkDir(root, func(path string, entry os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		fmt.Fprintln(hash, rel, entry.Type())
		if !entry.IsDir() {
			data, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			hash.Write(data)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	return fmt.Sprintf("%x", hash.Sum(nil))
}

func TestDarwinIndependentRepositoryVolumeAndWorkspaceAdmission(t *testing.T) {
	other := os.Getenv("BACKUP_TEST_OTHER_VOLUME")
	if other == "" {
		t.Skip("requires explicit disposable second-volume fixture")
	}
	repo, err := os.MkdirTemp(other, "encrypted-repository-")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(repo)
	bad, err := os.MkdirTemp(other, "wrong-source-workspace-")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(bad)
	req, workspace := workspaceRequest(t, "")
	_ = closeRequestPaths(&req)
	req.repoPath = repo
	var sourceStat, repoStat unix.Stat_t
	if err := unix.Stat(req.sourcePath, &sourceStat); err != nil {
		t.Fatal(err)
	}
	if err := unix.Stat(repo, &repoStat); err != nil {
		t.Fatal(err)
	}
	if sourceStat.Dev == repoStat.Dev {
		t.Fatal("second-volume control did not use another device")
	}
	marker := "private-cross-volume-symlink-44ac2da8"
	if err := os.WriteFile(filepath.Join(req.sourcePath, "owner"), []byte("source-volume owner contents"), 0640); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(marker, filepath.Join(req.sourcePath, "link")); err != nil {
		t.Fatal(err)
	}
	init, err := executeInit(context.Background(), bridgeRequest{operation: OpInit, password: append([]byte(nil), req.password...), repoPath: repo}, func() {})
	if err != nil || init.status != StatusOk {
		t.Fatalf("init: %+v %v", init, err)
	}
	if err := pinRequestPaths(&req); err != nil {
		t.Fatal(err)
	}
	before := repositoryFixtureDigest(t, repo)
	if err := pinMetadataWorkspace(&req, bad); err == nil || !strings.Contains(err.Error(), "different source volume") {
		t.Fatalf("wrong-volume workspace not explicitly refused: %v", err)
	}
	if after := repositoryFixtureDigest(t, repo); before != after {
		t.Fatal("workspace refusal changed repository bytes or names")
	}
	if err := pinMetadataWorkspace(&req, workspace); err != nil {
		t.Fatal(err)
	}
	result := completeOwnedCall(t, req)
	if result.status != StatusOk {
		t.Fatalf("independent-volume snapshot failed: %+v", result)
	}
	found, err := rawMarkerSurfaces(repo, marker)
	if err != nil || len(found) != 0 {
		t.Fatalf("cross-volume repository leaked metadata marker: %v %v", found, err)
	}
	destination := filepath.Join(t.TempDir(), "restore")
	restore := bridgeRequest{operation: OpRestore, password: []byte("private-workspace-fixture"), repoPath: repo, sourcePath: req.sourcePath, restorePath: destination, snapshotID: result.snapshotID}
	restored, err := executeRestore(context.Background(), &restore, func() {})
	if err != nil || restored.status != StatusOk {
		t.Fatalf("independent-volume restore failed: %+v %v", restored, err)
	}
	contents, err := os.ReadFile(filepath.Join(destination, "source", "owner"))
	if err != nil || string(contents) != "source-volume owner contents" {
		t.Fatalf("restored bytes mismatch: %q %v", contents, err)
	}
	target, err := os.Readlink(filepath.Join(destination, "source", "link"))
	if err != nil || target != marker {
		t.Fatalf("restored symlink mismatch: %q %v", target, err)
	}
	t.Logf("source/workspace device=%d independent repository/wrong-workspace device=%d", sourceStat.Dev, repoStat.Dev)
}
