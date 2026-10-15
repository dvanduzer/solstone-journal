// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"golang.org/x/sys/unix"
)

func TestRootedSelectorIsClosedBeforeNormalization(t *testing.T) {
	for _, value := range []string{"journal", "/journal", "parent/owner 界", "a:b"} {
		if !validateRootedSelector(value) {
			t.Fatal("valid selector refused", value)
		}
	}
	for _, value := range []string{"", "/", "//journal", "a//b", "a/", "a/./b", "a/../b", "../b", "a\\b", "a\x00b", "a\nb", string([]byte{0xff}), strings.Repeat("x", 4097), strings.Repeat("a/", 64) + "b"} {
		if validateRootedSelector(value) {
			t.Fatalf("malformed selector normalized: %q", value)
		}
	}
}

func TestRootedOutcomeSerializesAgainstActualFree(t *testing.T) {
	id, status := allocateID(&nextID)
	if status != StatusOk {
		t.Fatal(status)
	}
	r := &rootedRestore{admitted: true}
	r.mayMutate.Store(true)
	h := &callHandle{id: id, state: handleStopped, doneCh: make(chan struct{}), rootedRestore: r, result: bridgeResult{status: StatusFailed}}
	close(h.doneCh)
	handlesMu.Lock()
	handles[id] = h
	handlesMu.Unlock()
	entered, release, readDone, freeDone := make(chan struct{}), make(chan struct{}), make(chan int32, 1), make(chan int32, 1)
	var once sync.Once
	go func() {
		readDone <- withRootedRestoreOutcome(id, func(admitted, mutated bool) int32 {
			if !admitted || !mutated {
				return StatusFailed
			}
			once.Do(func() { close(entered) })
			<-release
			return StatusOk
		})
	}()
	<-entered
	go func() { freeDone <- freeCall(id) }()
	close(release)
	if <-readDone != StatusOk || <-freeDone != StatusOk {
		t.Fatal("terminal getter/free lost actual serialization")
	}
	if withRootedRestoreOutcome(id, func(bool, bool) int32 { return StatusOk }) != StatusInvalidHandle {
		t.Fatal("freed outcome remained available")
	}
}

func TestRootedTargetReservationAndLockSubstitutionRetainCustody(t *testing.T) {
	path, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	borrowed, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	r, status := newRootedRestore(int(borrowed.Fd()), "journal")
	if status != StatusOk {
		t.Fatal(status)
	}
	_ = borrowed.Close()
	req := bridgeRequest{operation: OpRestore, restorePath: path, rootedRestore: r}
	defer closeRequestPaths(&req)
	if err := pinDataPaths(&req, nil); err != nil {
		t.Fatal(err)
	}
	lock := filepath.Join(path, rootedRestoreLock)
	if err := os.Rename(lock, lock+"-retained"); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(lock, []byte("replacement lock"), 0600); err != nil {
		t.Fatal(err)
	}
	if recheckRootedRestore(&req) == nil {
		t.Fatal("lock substitution accepted")
	}
	borrowed, err = os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	other, status := newRootedRestore(int(borrowed.Fd()), "journal")
	_ = borrowed.Close()
	if status != StatusOk {
		t.Fatal(status)
	}
	replacement := bridgeRequest{operation: OpRestore, restorePath: path, rootedRestore: other}
	defer closeRequestPaths(&replacement)
	if err := pinDataPaths(&replacement, nil); !errors.Is(err, errS3OwnerBusy) {
		t.Fatal("replacement lock bypassed same-process target reservation", err)
	}
	// The scope deliberately makes no cross-process claim for a new lock inode.
	file, err := os.OpenFile(lock, os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	if err := unix.Flock(int(file.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		_ = file.Close()
		t.Fatal("unexpected replacement-inode lock behavior", err)
	}
	_ = file.Close()
}

func TestRootedDescriptorKindsAndProtectedOverlapRefuse(t *testing.T) {
	path, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	file, err := os.Create(filepath.Join(path, "ordinary"))
	if err != nil {
		t.Fatal(err)
	}
	if r, status := newRootedRestore(int(file.Fd()), "journal"); r != nil || status != StatusInvalidArgument {
		t.Fatal("regular target descriptor admitted")
	}
	closed := int(file.Fd())
	_ = file.Close()
	if r, status := newRootedRestore(closed, "journal"); r != nil || status != StatusInvalidArgument {
		t.Fatal("closed target descriptor admitted")
	}
	directory, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer directory.Close()
	r, status := newRootedRestore(int(directory.Fd()), "journal")
	if status != StatusOk {
		t.Fatal(status)
	}
	req := bridgeRequest{operation: OpRestore, restorePath: path, rootedRestore: r}
	defer closeRequestPaths(&req)
	if err := pinDataPaths(&req, []*os.File{directory}); !errors.Is(err, errPathOverlap) {
		t.Fatal("repository/target alias overlap admitted", err)
	}
	if _, err := os.Stat(filepath.Join(path, rootedRestoreLock)); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("overlap refusal created an admission lock")
	}
}

func TestRootedPrivateCustodyExcludesBothOrdersAndAncestry(t *testing.T) {
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	privatePath := filepath.Join(base, "private")
	childPath := filepath.Join(privatePath, "child")
	if err := os.MkdirAll(childPath, 0700); err != nil {
		t.Fatal(err)
	}
	private, err := openPinnedDirectory(privatePath)
	if err != nil {
		t.Fatal(err)
	}
	defer closeRootedPrivateDirectory(private)
	if err := reserveRootedPrivateDirectory(private); err != nil {
		t.Fatal(err)
	}
	for _, target := range []string{base, privatePath, childPath} {
		file, err := os.Open(target)
		if err != nil {
			t.Fatal(err)
		}
		r, status := newRootedRestore(int(file.Fd()), "journal")
		_ = file.Close()
		if status != StatusOk {
			t.Fatal(status)
		}
		req := bridgeRequest{operation: OpRestore, restorePath: target, rootedRestore: r}
		if err := pinDataPaths(&req, nil); !errors.Is(err, errS3OwnerBusy) {
			_ = closeRequestPaths(&req)
			t.Fatal("live private root admitted as restore target/ancestor/child", target, err)
		}
		_ = closeRequestPaths(&req)
	}
	// Distinct admissions of the same private inode are distinct retained tokens.
	second, err := openPinnedDirectory(privatePath)
	if err != nil {
		t.Fatal(err)
	}
	if err := reserveRootedPrivateDirectory(second); err != nil {
		t.Fatal("private/private policy changed", err)
	}
	if err := closeRootedPrivateDirectory(private); err != nil {
		t.Fatal(err)
	}
	file, err := os.Open(privatePath)
	if err != nil {
		t.Fatal(err)
	}
	r, status := newRootedRestore(int(file.Fd()), "journal")
	_ = file.Close()
	if status != StatusOk {
		t.Fatal(status)
	}
	req := bridgeRequest{operation: OpRestore, restorePath: privatePath, rootedRestore: r}
	if err := pinDataPaths(&req, nil); !errors.Is(err, errS3OwnerBusy) {
		_ = closeRequestPaths(&req)
		t.Fatal("closing one private admission released another", err)
	}
	_ = closeRequestPaths(&req)
	if err := closeRootedPrivateDirectory(second); err != nil {
		t.Fatal(err)
	}
	file, err = os.Open(privatePath)
	if err != nil {
		t.Fatal(err)
	}
	r, status = newRootedRestore(int(file.Fd()), "journal")
	_ = file.Close()
	if status != StatusOk {
		t.Fatal(status)
	}
	req = bridgeRequest{operation: OpRestore, restorePath: privatePath, rootedRestore: r}
	defer closeRequestPaths(&req)
	if err := pinDataPaths(&req, nil); err != nil {
		t.Fatal("settled private tokens retained false custody", err)
	}
	for _, privateRoot := range []string{base, privatePath, childPath} {
		file, err := openPinnedDirectory(privateRoot)
		if err != nil {
			t.Fatal(err)
		}
		if err := reserveRootedPrivateDirectory(file); !errors.Is(err, errS3OwnerBusy) {
			_ = closeRootedPrivateDirectory(file)
			t.Fatal("live restore accepted overlapping private admission", err)
		}
		_ = closeRootedPrivateDirectory(file)
	}
}

func TestRootedPrivateAndTargetAdmissionIsOneAtomicDecision(t *testing.T) {
	path, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	private, err := openPinnedDirectory(path)
	if err != nil {
		t.Fatal(err)
	}
	defer closeRootedPrivateDirectory(private)
	file, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	r, status := newRootedRestore(int(file.Fd()), "journal")
	_ = file.Close()
	if status != StatusOk {
		t.Fatal(status)
	}
	req := bridgeRequest{operation: OpRestore, restorePath: path, rootedRestore: r}
	defer closeRequestPaths(&req)
	start := make(chan struct{})
	targetResult, privateResult := make(chan error, 1), make(chan error, 1)
	go func() { <-start; targetResult <- pinDataPaths(&req, nil) }()
	go func() { <-start; privateResult <- reserveRootedPrivateDirectory(private) }()
	close(start)
	targetErr, privateErr := <-targetResult, <-privateResult
	if (targetErr == nil) == (privateErr == nil) {
		t.Fatal("target/private admissions did not exclude each other atomically", targetErr, privateErr)
	}
}

func TestRootedPathsRefuseActualInvalidUTF8Names(t *testing.T) {
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	invalid := filepath.Join(base, string([]byte{'o', 'w', 'n', 'e', 'r', 0xff}))
	if err := os.Mkdir(invalid, 0700); err != nil {
		t.Skip("filesystem refuses invalid UTF8 names")
	}
	valid := filepath.Join(base, "valid")
	if err := os.Mkdir(valid, 0700); err != nil {
		t.Fatal(err)
	}
	for _, pair := range [][2]string{{valid, invalid}, {invalid, valid}} {
		file, err := os.Open(pair[1])
		if err != nil {
			t.Fatal(err)
		}
		r, status := newRootedRestore(int(file.Fd()), "journal")
		_ = file.Close()
		if status != StatusOk {
			t.Fatal(status)
		}
		req := bridgeRequest{operation: OpRestore, repoPath: pair[0], restorePath: pair[1], rootedRestore: r}
		if err := pinRequestPaths(&req); err == nil {
			_ = closeRequestPaths(&req)
			t.Fatal("new rooted path accepted invalid UTF8")
		}
		_ = closeRequestPaths(&req)
	}
}
