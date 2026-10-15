// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"golang.org/x/sys/unix"
)

func targetAdmissionTestBase(t *testing.T) string {
	t.Helper()
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	return base
}

func TestTargetAdmissionHandleCapacityRefusalReleasesBorrowOnly(t *testing.T) {
	base := targetAdmissionTestBase(t)
	target, repo := filepath.Join(base, "target"), filepath.Join(base, "repo")
	for _, path := range []string{target, repo} {
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	admission, status := targetAdmissionTestAcquire(target)
	if status != StatusOk {
		t.Fatal(status)
	}
	t.Cleanup(func() { _ = releaseRootedTarget(admission) })
	release := make(chan struct{})
	var ids []uint64
	testGate = func(string, context.Context) error { <-release; return context.Canceled }
	t.Cleanup(func() {
		close(release)
		for _, id := range ids {
			_ = cancelCall(id)
			handlesMu.Lock()
			h := handles[id]
			handlesMu.Unlock()
			<-h.doneCh
			_ = freeCall(id)
		}
		testGate = nil
	})
	for repeat := 0; repeat < 4; repeat++ {
		id, status := startBridgeCall(bridgeRequest{operation: OpOpen, password: []byte("synthetic held worker")})
		if status != StatusOk {
			t.Fatal("actual capacity fixture failed", status)
		}
		ids = append(ids, id)
	}
	r, status := borrowRootedTarget(admission, "journal")
	if status != StatusOk {
		t.Fatal(status)
	}
	password := []byte("synthetic rejected request")
	req := bridgeRequest{operation: OpRestore, password: password, repoPath: repo, restorePath: target,
		snapshotID: strings.Repeat("a", 64), rootedRestore: r}
	if err := pinRequestPaths(&req); err != nil || !r.admitted {
		_ = rejectBridgeCall(&req, StatusInvalidArgument)
		t.Fatal("actual pinned request fixture failed", err)
	}
	if operation, status := startBridgeCall(req); operation != 0 || status != StatusBusy {
		t.Fatal("actual handle capacity did not refuse", operation, status)
	}
	if _, err := req.repoRoot.Stat("."); !errors.Is(err, os.ErrClosed) {
		t.Fatal("capacity rejection retained usable request repository root", err)
	}
	if _, err := req.restoreRoot.Stat("."); !errors.Is(err, os.ErrClosed) {
		t.Fatal("capacity rejection retained usable request target root", err)
	}
	for _, value := range password {
		if value != 0 {
			t.Fatal("capacity rejection retained request credential bytes")
		}
	}
	if r.mayMutate.Load() || revalidateRootedTarget(admission) != StatusOk {
		t.Fatal("rejection mutated target or lost admission")
	}
	assertTargetAdmissionFlockHeld(t, target)
	next, status := borrowRootedTarget(admission, "journal")
	if status != StatusOk || releaseRootedRestoreBorrow(next) != nil {
		t.Fatal("capacity rejection orphaned its retained-target borrow", status)
	}
	if releaseRootedTarget(admission) != StatusOk {
		t.Fatal("actual post-refusal target release failed")
	}
}

func targetAdmissionTestAcquire(path string) (uint64, int32) {
	file, err := os.Open(path)
	if err != nil {
		return 0, StatusInvalidArgument
	}
	defer file.Close()
	return admitRootedTarget(int(file.Fd()), path)
}

func assertTargetAdmissionFlockHeld(t *testing.T, path string) {
	t.Helper()
	file, err := os.OpenFile(filepath.Join(path, rootedRestoreLock), os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if err := unix.Flock(int(file.Fd()), unix.LOCK_EX|unix.LOCK_NB); !errors.Is(err, unix.EWOULDBLOCK) && !errors.Is(err, unix.EAGAIN) {
		t.Fatal("independently opened description did not observe the held lease", err)
	}
}

func TestTargetAdmissionRetainsOneLeaseThroughBorrowAndPostWork(t *testing.T) {
	path := targetAdmissionTestBase(t)
	lock := filepath.Join(path, rootedRestoreLock)
	if err := os.WriteFile(lock, []byte("persistent lock bytes"), 0600); err != nil {
		t.Fatal(err)
	}
	before, _ := os.Stat(lock)
	id, status := targetAdmissionTestAcquire(path)
	if status != StatusOk || id == 0 {
		t.Fatal(id, status)
	}
	t.Cleanup(func() { _ = releaseRootedTarget(id) })
	assertTargetAdmissionFlockHeld(t, path) // caller's original FD is already closed
	for repeat := 0; repeat < 2; repeat++ {
		r, status := borrowRootedTarget(id, "journal")
		if status != StatusOk || r.mayMutate.Load() {
			t.Fatal("borrow inherited earlier state", status)
		}
		req := bridgeRequest{restorePath: path, rootedRestore: r}
		if err := pinRootedRestore(&req, nil); err != nil {
			t.Fatal(err)
		}
		r.mayMutate.Store(true)
		if _, status := borrowRootedTarget(id, "journal"); status != StatusBusy || releaseRootedTarget(id) != StatusBusy {
			t.Fatal("a live borrow did not exclude release/replacement")
		}
		if err := closeRequestPaths(&req); err != nil {
			t.Fatal(err)
		}
		if releaseRootedTarget(id) != StatusBusy {
			t.Fatal("worker path cleanup prematurely released the borrow")
		}
		if err := releaseRootedRestoreBorrow(r); err != nil {
			t.Fatal(err)
		}
		assertTargetAdmissionFlockHeld(t, path)
		if revalidateRootedTarget(id) != StatusOk {
			t.Fatal("post-work admission lost")
		}
		if err := os.WriteFile(filepath.Join(path, "post-work"), []byte("joined"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	after, _ := os.Stat(lock)
	data, _ := os.ReadFile(lock)
	if !os.SameFile(before, after) || string(data) != "persistent lock bytes" {
		t.Fatal("lock was replaced or truncated")
	}
	if releaseRootedTarget(id) != StatusOk || revalidateRootedTarget(id) != StatusInvalidHandle || releaseRootedTarget(id) != StatusInvalidHandle {
		t.Fatal("released/stale admission accepted")
	}
	next, status := targetAdmissionTestAcquire(path)
	if status != StatusOk || next == id {
		t.Fatal("successful release did not permit fresh admission", status)
	}
	if releaseRootedTarget(next) != StatusOk {
		t.Fatal("fresh release failed")
	}
}

func targetAdmissionTestKind(path string, retained bool) (func(), int32) {
	if retained {
		id, status := targetAdmissionTestAcquire(path)
		return func() { _ = releaseRootedTarget(id) }, status
	}
	file, err := os.Open(path)
	if err != nil {
		return func() {}, StatusInvalidArgument
	}
	r, status := newRootedRestore(int(file.Fd()), "journal")
	_ = file.Close()
	if status != StatusOk {
		return func() {}, status
	}
	req := &bridgeRequest{restorePath: path, rootedRestore: r}
	cleanup := func() { _ = closeRequestPaths(req) }
	if err := pinRootedRestore(req, nil); err != nil {
		cleanup()
		return func() {}, targetAdmissionStatus(err)
	}
	return cleanup, StatusOk
}

func TestTargetAdmissionAncestryAndSiblingsInBothOrders(t *testing.T) {
	for _, firstRetained := range []bool{false, true} {
		for _, secondRetained := range []bool{false, true} {
			for _, reverse := range []bool{false, true} {
				base := targetAdmissionTestBase(t)
				outer, inner, sibling := filepath.Join(base, "journal"), filepath.Join(base, "journal", "nested"), filepath.Join(base, "sibling")
				for _, path := range []string{inner, sibling} {
					if err := os.MkdirAll(path, 0700); err != nil {
						t.Fatal(err)
					}
				}
				first, second := outer, inner
				if reverse {
					first, second = second, first
				}
				closeFirst, status := targetAdmissionTestKind(first, firstRetained)
				if status != StatusOk {
					t.Fatal("first", status)
				}
				closeSecond, status := targetAdmissionTestKind(second, secondRetained)
				closeSecond()
				if status != StatusBusy {
					closeFirst()
					t.Fatal("overlapping admitted targets", firstRetained, secondRetained, reverse, status)
				}
				closeSibling, status := targetAdmissionTestKind(sibling, secondRetained)
				if status != StatusOk {
					closeFirst()
					t.Fatal("unrelated sibling refused", status)
				}
				closeSibling()
				closeFirst()
				closeSecond, status = targetAdmissionTestKind(second, secondRetained)
				if status != StatusOk {
					t.Fatal("released ancestry still refused", status)
				}
				closeSecond()
			}
		}
	}
}

// Test-only disposal knows which closers were consumed by the injected fault.
// It never retries an ambiguous close or changes production failed custody.
func cleanupFailedTargetAdmissionTest(t *testing.T, id uint64) {
	t.Helper()
	targetAdmissionCloseHook = nil
	rootedTargets.Lock()
	defer rootedTargets.Unlock()
	owner := rootedTargets.owners[id]
	if owner == nil || owner.borrow != nil {
		t.Fatal("missing refused test owner")
	}
	if owner.root != nil {
		_ = owner.root.Close()
	}
	for _, file := range []*os.File{owner.rooted.lease, owner.rooted.target} {
		if file != nil {
			_ = file.Close()
		}
	}
	delete(rootedTargets.owners, id)
	delete(rootedTargets.failed, owner.rooted)
	if rootedTargets.live[owner.rooted.identity] == owner.rooted {
		delete(rootedTargets.live, owner.rooted.identity)
	}
}

func TestTargetAdmissionCloseFailuresRetainRefusedCustodyWithoutRetry(t *testing.T) {
	for _, stage := range []string{"root", "lease", "target"} {
		t.Run(stage, func(t *testing.T) {
			path := targetAdmissionTestBase(t)
			id, status := targetAdmissionTestAcquire(path)
			if status != StatusOk {
				t.Fatal(status)
			}
			t.Cleanup(func() { cleanupFailedTargetAdmissionTest(t, id) })
			calls := 0
			targetAdmissionCloseHook = func(kind string, resource io.Closer) error {
				calls++
				err := resource.Close()
				if kind == stage {
					return errors.Join(err, errors.New("controlled post-close error"))
				}
				return err
			}
			if releaseRootedTarget(id) != StatusFailed || revalidateRootedTarget(id) != StatusFailed {
				t.Fatal("close error reported healthy release/admission")
			}
			before := calls
			if releaseRootedTarget(id) != StatusFailed || calls != before {
				t.Fatal("ambiguous close retried")
			}
			if r, status := borrowRootedTarget(id, "journal"); r != nil || status != StatusFailed {
				t.Fatal("refused custody granted a borrow")
			}
			if next, status := targetAdmissionTestAcquire(path); next != 0 || status != StatusFailed {
				t.Fatal("failed ownership disappeared from admission")
			}
		})
	}
}

func TestTargetAdmissionAcquireFailureReturnsRefusedOwner(t *testing.T) {
	path := targetAdmissionTestBase(t)
	file, err := os.OpenFile(filepath.Join(path, rootedRestoreLock), os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if err := unix.Flock(int(file.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		t.Fatal(err)
	}
	targetAdmissionCloseHook = func(kind string, resource io.Closer) error {
		err := resource.Close()
		if kind == "lease" {
			return errors.Join(err, errors.New("failed acquisition cleanup"))
		}
		return err
	}
	id, status := targetAdmissionTestAcquire(path)
	if status != StatusFailed || id == 0 {
		targetAdmissionCloseHook = nil
		t.Fatal("incomplete acquisition cleanup lost its carrier", id, status)
	}
	defer cleanupFailedTargetAdmissionTest(t, id)
	if revalidateRootedTarget(id) != StatusFailed || releaseRootedTarget(id) != StatusFailed {
		t.Fatal("refused acquisition became usable")
	}
}

func TestTargetAdmissionBorrowReleaseAndRevalidateRace(t *testing.T) {
	for repeat := 0; repeat < 24; repeat++ {
		path := targetAdmissionTestBase(t)
		id, status := targetAdmissionTestAcquire(path)
		if status != StatusOk {
			t.Fatal(status)
		}
		var workers sync.WaitGroup
		workers.Add(2)
		go func() {
			defer workers.Done()
			r, status := borrowRootedTarget(id, "journal")
			if status == StatusOk {
				if err := releaseRootedRestoreBorrow(r); err != nil {
					t.Error(err)
				}
			} else if status != StatusInvalidHandle {
				t.Error("unexpected racing borrow result", status)
			}
		}()
		go func() {
			defer workers.Done()
			if result := revalidateRootedTarget(id); result != StatusOk && result != StatusInvalidHandle {
				t.Error("racing revalidation", result)
			}
			if result := releaseRootedTarget(id); result != StatusOk && result != StatusBusy {
				t.Error("racing release", result)
			}
		}()
		workers.Wait()
		if result := releaseRootedTarget(id); result != StatusOk && result != StatusInvalidHandle {
			t.Fatal("settled release", result)
		}
	}
}

func TestTargetAdmissionRejectsPathAndLeaseDriftWhileRetainingCustody(t *testing.T) {
	for _, drift := range []string{"target", "lock"} {
		t.Run(drift, func(t *testing.T) {
			base := targetAdmissionTestBase(t)
			path := filepath.Join(base, "target")
			if err := os.Mkdir(path, 0700); err != nil {
				t.Fatal(err)
			}
			id, status := targetAdmissionTestAcquire(path)
			if status != StatusOk {
				t.Fatal(status)
			}
			t.Cleanup(func() { _ = releaseRootedTarget(id) })
			original := path
			if drift == "target" {
				if err := os.Rename(path, path+"-retained"); err != nil {
					t.Fatal(err)
				}
				if err := os.Mkdir(path, 0700); err != nil {
					t.Fatal(err)
				}
				original += "-retained"
			} else {
				lock := filepath.Join(path, rootedRestoreLock)
				if err := os.Rename(lock, lock+"-retained"); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(lock, []byte("replacement"), 0600); err != nil {
					t.Fatal(err)
				}
			}
			if revalidateRootedTarget(id) != StatusInvalidArgument {
				t.Fatal("changed target binding remained valid")
			}
			if r, status := borrowRootedTarget(id, "journal"); r != nil || status != StatusInvalidArgument {
				t.Fatal("changed target binding granted a writer", status)
			}
			if next, status := targetAdmissionTestAcquire(original); next != 0 || status != StatusBusy {
				t.Fatal("failed revalidation dropped original reservation", next, status)
			}
			if releaseRootedTarget(id) != StatusOk {
				t.Fatal("explicit release of retained original failed")
			}
		})
	}
}

func TestTargetAdmissionCapturedPathAndPrivateOverlapRefuseBeforeMutation(t *testing.T) {
	base := targetAdmissionTestBase(t)
	target, other := filepath.Join(base, "target"), filepath.Join(base, "other")
	for _, path := range []string{target, other} {
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	fd, err := os.Open(target)
	if err != nil {
		t.Fatal(err)
	}
	defer fd.Close()
	if id, status := admitRootedTarget(int(fd.Fd()), other); id != 0 || status != StatusInvalidArgument {
		t.Fatal("FD/path mismatch accepted", id, status)
	}
	id, status := targetAdmissionTestAcquire(target)
	if status != StatusOk {
		t.Fatal(status)
	}
	t.Cleanup(func() { _ = releaseRootedTarget(id) })
	if freeCall(id) != StatusInvalidHandle {
		t.Fatal("admission ID accepted as operation")
	}
	r, status := borrowRootedTarget(id, "journal")
	if status != StatusOk {
		t.Fatal(status)
	}
	req := bridgeRequest{restorePath: other, rootedRestore: r}
	if err := pinRootedRestore(&req, nil); err == nil || r.admitted || r.mayMutate.Load() {
		t.Fatal("borrow switched target pathname")
	}
	if rejectBridgeCall(&req, StatusInvalidArgument) != StatusInvalidArgument || revalidateRootedTarget(id) != StatusOk {
		t.Fatal("rejected request lost retained owner or borrow")
	}
	if err := reserveRootedPrivateDirectory(fd); !errors.Is(err, errS3OwnerBusy) {
		t.Fatal("private workspace overlapped live retained target", err)
	}
	if releaseRootedTarget(id) != StatusOk {
		t.Fatal("clean release failed")
	}
	if err := reserveRootedPrivateDirectory(fd); err != nil {
		t.Fatal(err)
	}
	if next, status := targetAdmissionTestAcquire(target); next != 0 || status != StatusBusy {
		t.Fatal("retained target overlapped live private workspace", next, status)
	}
	if err := closeRootedPrivateDirectory(fd); err != nil {
		t.Fatal(err)
	}
}
