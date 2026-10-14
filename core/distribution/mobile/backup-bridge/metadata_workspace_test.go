// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build linux || darwin || ios

package main

import (
	"context"
	"errors"
	"fmt"
	"golang.org/x/sys/unix"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func workspaceRequest(t *testing.T, base string) (bridgeRequest, string) {
	t.Helper()
	if base == "" {
		base = t.TempDir()
	}
	paths := []string{filepath.Join(base, "repo"), filepath.Join(base, "source"), filepath.Join(base, "workspace")}
	for _, path := range paths {
		if err := os.MkdirAll(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	req := bridgeRequest{operation: OpSnapshot, password: []byte("private-workspace-fixture"), repoPath: paths[0], sourcePath: paths[1]}
	if err := pinRequestPaths(&req); err != nil {
		t.Fatal(err)
	}
	return req, paths[2]
}

func seedMetadataResidue(t *testing.T, root *os.Root, target string) string {
	t.Helper()
	name := metadataDirectoryPrefix + strings.Repeat("a", 32)
	if err := root.Mkdir(name, 0700); err != nil {
		t.Fatal(err)
	}
	if target != "" {
		if err := root.Symlink(target, name+"/"+strings.Repeat("b", 32)); err != nil {
			t.Fatal(err)
		}
	}
	if err := root.Chmod(name, 0300); err != nil {
		t.Fatal(err)
	}
	return name
}

func TestWorkspaceContentionAndRejectedClosePreserveWinner(t *testing.T) {
	req, path := workspaceRequest(t, "")
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&req)
	for i := 0; i < 2; i++ {
		other, _ := workspaceRequest(t, filepath.Dir(path))
		err := pinMetadataWorkspace(&other, path)
		_ = closeRequestPaths(&other)
		if !errors.Is(err, unix.EWOULDBLOCK) && !errors.Is(err, unix.EAGAIN) {
			t.Fatalf("competing open admitted or wrong failure: %v", err)
		}
	}
	child := exec.Command(os.Args[0], "-test.run=^TestWorkspaceSubprocessLock$")
	child.Env = append(os.Environ(), "BACKUP_TEST_LOCK_PATH="+path, "BACKUP_TEST_LOCK_EXPECT=busy")
	if output, err := child.CombinedOutput(); err != nil {
		t.Fatalf("cross-process lock: %s %v", output, err)
	}
	if err := closeRequestPaths(&req); err != nil {
		t.Fatal(err)
	}
	child = exec.Command(os.Args[0], "-test.run=^TestWorkspaceSubprocessLock$")
	child.Env = append(os.Environ(), "BACKUP_TEST_LOCK_PATH="+path, "BACKUP_TEST_LOCK_EXPECT=free")
	if output, err := child.CombinedOutput(); err != nil {
		t.Fatalf("cross-process release: %s %v", output, err)
	}
}

func TestWorkspaceSubprocessLock(t *testing.T) {
	path := os.Getenv("BACKUP_TEST_LOCK_PATH")
	if path == "" {
		return
	}
	file, err := openPinnedDirectory(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	err = unix.Flock(int(file.Fd()), unix.LOCK_EX|unix.LOCK_NB)
	if os.Getenv("BACKUP_TEST_LOCK_EXPECT") == "busy" {
		if !errors.Is(err, unix.EWOULDBLOCK) && !errors.Is(err, unix.EAGAIN) {
			t.Fatalf("active lock lost: %v", err)
		}
	} else if err != nil {
		t.Fatal(err)
	}
}

func TestWorkspacePermissionAndOverlapRefusal(t *testing.T) {
	req, path := workspaceRequest(t, "")
	defer closeRequestPaths(&req)
	for _, candidate := range []string{req.repoPath, req.sourcePath, filepath.Dir(path), filepath.Join(req.sourcePath, "inside")} {
		if err := os.MkdirAll(candidate, 0700); err != nil {
			t.Fatal(err)
		}
		if err := pinMetadataWorkspace(&req, candidate); err == nil {
			t.Fatalf("overlap accepted: %s", candidate)
		}
	}
	if err := os.Chmod(path, 0750); err != nil {
		t.Fatal(err)
	}
	if err := pinMetadataWorkspace(&req, path); err == nil {
		t.Fatal("non-private workspace accepted")
	}
}

func TestWorkspaceReplacementAndAliasCannotRedirectCustody(t *testing.T) {
	req, path := workspaceRequest(t, "")
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&req)
	if err := os.Rename(path, path+"-retained"); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(path, 0700); err != nil {
		t.Fatal(err)
	}
	canary := filepath.Join(path, "foreign")
	if err := os.WriteFile(canary, []byte("replacement canary"), 0600); err != nil {
		t.Fatal(err)
	}
	other, _ := workspaceRequest(t, filepath.Dir(path))
	err := pinMetadataWorkspace(&other, path+"-retained")
	_ = closeRequestPaths(&other)
	if !errors.Is(err, unix.EWOULDBLOCK) && !errors.Is(err, unix.EAGAIN) {
		t.Fatalf("alias stole custody: %v", err)
	}
	seedMetadataResidue(t, req.metadataRoot, "retained target")
	if err := cleanupMetadataWorkspace(&req); err != nil {
		t.Fatal(err)
	}
	value, err := os.ReadFile(canary)
	if err != nil || string(value) != "replacement canary" {
		t.Fatalf("replacement changed: %q %v", value, err)
	}
	entries, err := os.ReadDir(path + "-retained")
	if err != nil || len(entries) != 0 {
		t.Fatalf("original residue not cleared: %v %v", entries, err)
	}
}

func TestWorkspaceForeignCensusPrecedesAllDeletion(t *testing.T) {
	req, path := workspaceRequest(t, "")
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&req)
	valid := seedMetadataResidue(t, req.metadataRoot, "retained private target")
	foreign := metadataDirectoryPrefix + strings.Repeat("c", 32)
	if err := req.metadataRoot.Mkdir(foreign, 0700); err != nil {
		t.Fatal(err)
	}
	if err := req.metadataRoot.WriteFile(foreign+"/foreign", []byte("foreign canary"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := req.metadataRoot.Chmod(foreign, 0300); err != nil {
		t.Fatal(err)
	}
	if err := cleanupMetadataWorkspace(&req); !errors.Is(err, errMetadataCleanup) {
		t.Fatalf("foreign content accepted: %v", err)
	}
	info, err := req.metadataRoot.Stat(foreign)
	if err != nil || info.Mode().Perm() != 0300 {
		t.Fatalf("foreign mode changed: %v %v", info, err)
	}
	value, err := os.ReadFile(filepath.Join(path, foreign, "foreign"))
	if err != nil || string(value) != "foreign canary" {
		t.Fatalf("foreign content changed: %q %v", value, err)
	}
	if _, err := req.metadataRoot.Lstat(valid); err != nil {
		t.Fatal("valid residue deleted before complete census", err)
	}
	if err := req.metadataRoot.Chmod(foreign, 0700); err != nil {
		t.Fatal(err)
	}
	if err := req.metadataRoot.Remove(foreign + "/foreign"); err != nil {
		t.Fatal(err)
	}
	if err := req.metadataRoot.Remove(foreign); err != nil {
		t.Fatal(err)
	}
	if err := cleanupMetadataWorkspace(&req); err != nil {
		t.Fatal(err)
	}
	outside := filepath.Join(t.TempDir(), "canary")
	if err := os.WriteFile(outside, []byte("outside"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := req.metadataRoot.Symlink(outside, foreign); err != nil {
		t.Fatal(err)
	}
	if err := cleanupMetadataWorkspace(&req); !errors.Is(err, errMetadataCleanup) {
		t.Fatal("foreign symlink accepted", err)
	}
	value, err = os.ReadFile(outside)
	if err != nil || string(value) != "outside" {
		t.Fatal("foreign symlink target changed", err)
	}
	if err := req.metadataRoot.Remove(foreign); err != nil {
		t.Fatal(err)
	}
}

func TestWorkspaceCleanupFailureDrainsReleasesAndSupportsRetry(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Fatal("requires real non-root permissions")
	}
	req, path := workspaceRequest(t, "")
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	entered, release := make(chan struct{}), make(chan struct{})
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			close(entered)
			<-release
			return context.Canceled
		}
		return nil
	}
	defer func() { testGate = nil; _ = os.Chmod(path, 0700) }()
	id, status := startBridgeCall(req)
	if status != StatusOk {
		t.Fatal(status)
	}
	handlesMu.Lock()
	handle := handles[id]
	handlesMu.Unlock()
	<-entered
	seedMetadataResidue(t, req.metadataRoot, "private cleanup failure marker")
	if err := os.Chmod(path, 0000); err != nil {
		t.Fatal(err)
	}
	if cancelCall(id) != StatusOk || freeCall(id) != StatusBusy {
		t.Fatal("running worker incorrectly released")
	}
	close(release)
	<-handle.doneCh
	if handle.result.status != StatusFailed || handle.result.diagnostic != DiagWorkspaceCleanupRequired {
		t.Fatalf("cleanup failure hidden: %+v", handle.result)
	}
	if freeCall(id) != StatusOk {
		t.Fatal("drained handle not released")
	}
	if err := os.Chmod(path, 0700); err != nil {
		t.Fatal(err)
	}
	testGate = nil
	retry, _ := workspaceRequest(t, filepath.Dir(path))
	if err := pinMetadataWorkspace(&retry, path); err != nil {
		t.Fatal("retry could not acquire released custody", err)
	}
	defer closeRequestPaths(&retry)
	if err := cleanupMetadataWorkspace(&retry); err != nil {
		t.Fatal("retry did not reclaim retained residue", err)
	}
	entries, err := os.ReadDir(path)
	if err != nil || len(entries) != 0 {
		t.Fatalf("retry residue: %v %v", entries, err)
	}
}

func TestWorkspaceCleanupRequiredSurvivesAdmissionRejection(t *testing.T) {
	req, path := workspaceRequest(t, "")
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	seedMetadataResidue(t, req.metadataRoot, "retained admission marker")
	if err := os.Chmod(path, 0000); err != nil {
		t.Fatal(err)
	}
	defer os.Chmod(path, 0700)
	active := make([]*callHandle, 16)
	for index := range active {
		active[index] = syntheticHandle(t)
	}
	defer func() {
		for _, handle := range active {
			finishCall(handle, bridgeResult{status: StatusOk}, false)
			freeCall(handle.id)
		}
	}()
	id, status := startBridgeCall(req)
	if id != 0 || status != StatusWorkspaceCleanupRequired {
		t.Fatalf("admission masked cleanup failure: %d %d", id, status)
	}
	if err := os.Chmod(path, 0700); err != nil {
		t.Fatal(err)
	}
	retry, _ := workspaceRequest(t, filepath.Dir(path))
	if err := pinMetadataWorkspace(&retry, path); err != nil {
		t.Fatal(err)
	}
	if err := closeRequestPaths(&retry); err != nil {
		t.Fatal(err)
	}
}

func TestWorkspaceCleanupRequiredSurvivesWorkerPanic(t *testing.T) {
	req, path := workspaceRequest(t, "")
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			seedMetadataResidue(t, req.metadataRoot, "retained panic marker")
			if err := os.Chmod(path, 0000); err != nil {
				panic(err)
			}
			panic("fixture panic")
		}
		return nil
	}
	defer func() { testGate = nil; os.Chmod(path, 0700) }()
	id, status := startBridgeCall(req)
	if status != StatusOk {
		t.Fatal(status)
	}
	handlesMu.Lock()
	handle := handles[id]
	handlesMu.Unlock()
	<-handle.doneCh
	if handle.result.status != StatusFailed || handle.result.diagnostic != DiagWorkspaceCleanupRequired {
		t.Fatalf("panic masked cleanup failure: %+v", handle.result)
	}
	if freeCall(id) != StatusOk {
		t.Fatal("drained panic handle not released")
	}
	if err := os.Chmod(path, 0700); err != nil {
		t.Fatal(err)
	}
	retry, _ := workspaceRequest(t, filepath.Dir(path))
	if err := pinMetadataWorkspace(&retry, path); err != nil {
		t.Fatal(err)
	}
	if err := closeRequestPaths(&retry); err != nil {
		t.Fatal(err)
	}
}

func TestWorkspaceResidueCensusIsBoundedBeforeDeletion(t *testing.T) {
	req, path := workspaceRequest(t, "")
	if err := pinMetadataWorkspace(&req, path); err != nil {
		t.Fatal(err)
	}
	defer closeRequestPaths(&req)
	for index := 0; index < 65; index++ {
		name := metadataDirectoryPrefix + fmt.Sprintf("%032x", index)
		if err := req.metadataRoot.Mkdir(name, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := cleanupMetadataWorkspace(&req); !errors.Is(err, errMetadataCleanup) {
		t.Fatal("unbounded residue accepted", err)
	}
	entries, err := os.ReadDir(path)
	if err != nil || len(entries) != 65 {
		t.Fatalf("census partially deleted excess residue: %d %v", len(entries), err)
	}
	for _, entry := range entries {
		if err := req.metadataRoot.Remove(entry.Name()); err != nil {
			t.Fatal(err)
		}
	}
}
