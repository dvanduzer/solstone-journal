//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

func admittedTargetTestRequest(t *testing.T, f *rootedTestFixture, target string, admission uint64, snapshot string, password []byte) bridgeRequest {
	t.Helper()
	r, status := borrowRootedTarget(admission, "journal")
	if status != StatusOk {
		t.Fatal("borrow admitted target", status)
	}
	return bridgeRequest{operation: OpRestore, repoPath: f.repo, restorePath: target, snapshotID: snapshot,
		password: append([]byte(nil), password...), rootedRestore: r}
}

func TestTargetAdmissionRemotePendingAndStateUncertaintyPreserveTargetOutcome(t *testing.T) {
	for _, uncertain := range []bool{false, true} {
		t.Run(map[bool]string{false: "pending-lock-cleanup", true: "uncertain-owner-sync"}[uncertain], func(t *testing.T) {
			f := newRootedTestFixture(t, true)
			target := filepath.Join(f.base, "target")
			if err := os.Mkdir(target, 0700); err != nil {
				t.Fatal(err)
			}
			admission, status := targetAdmissionTestAcquire(target)
			if status != StatusOk {
				t.Fatal(status)
			}
			t.Cleanup(func() { _ = releaseRootedTarget(admission) })
			owner, err := newS3Owner(f.state, f.cfg)
			if err != nil {
				t.Fatal(err)
			}
			if uncertain {
				owner.fault = func(point string) error {
					if point == "directory-sync" && owner.record.Complete != nil {
						return errors.New("controlled final owner sync failure")
					}
					return nil
				}
			}
			transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
				if !uncertain && req.Method == http.MethodDelete {
					return nil, errS3IO
				}
				return f.peer.RoundTrip(req)
			})
			operation, handle := s3TestStartOwnedOperation(t, f.cfg, owner, f.workspace, transport,
				admittedTargetTestRequest(t, f, target, admission, f.first, f.recovery))
			waitAdmittedTargetWorker(t, operation, handle, admission, target)
			result, mutated, _ := rootedTestWait(t, operation, handle)
			want := StatusRemotePending
			if uncertain {
				want = StatusCommittedUncertain
			}
			if result.status != want || !mutated {
				t.Fatal("remote outcome lost actual local mutation", result.status, mutated)
			}
			if data, err := os.ReadFile(filepath.Join(target, "owner.txt")); err != nil || string(data) != "original record" {
				t.Fatal("actual restored content missing", err)
			}
			assertTargetAdmissionFlockHeld(t, target)
			reopened := s3OwnerTestOpen(t, f.state, f.cfg)
			if uncertain {
				if reopened.record.Complete == nil || reopened.record.Active != nil {
					t.Fatal("owner sync uncertainty lost visible completion")
				}
			} else if reopened.record.Active == nil || !reopened.record.Active.MayHaveWritten {
				t.Fatal("failed remote lock cleanup lost pending state")
			}
			if releaseRootedTarget(admission) != StatusOk {
				t.Fatal("target release was confused with remote reconciliation")
			}
		})
	}
}

func TestTargetAdmissionActualWorkerCleanupFailureRetainsRefusedOwner(t *testing.T) {
	f := newRootedTestFixture(t, false)
	target := filepath.Join(f.base, "target")
	if err := os.Mkdir(target, 0700); err != nil {
		t.Fatal(err)
	}
	admission, status := targetAdmissionTestAcquire(target)
	if status != StatusOk {
		t.Fatal(status)
	}
	t.Cleanup(func() { cleanupFailedTargetAdmissionTest(t, admission) })
	targetAdmissionCloseHook = func(kind string, resource io.Closer) error {
		err := resource.Close()
		if kind == "worker-root" {
			return errors.Join(err, errors.New("controlled worker root post-close error"))
		}
		return err
	}
	operation, handle := rootedTestStart(t, f, admittedTargetTestRequest(t, f, target, admission, f.first, f.recovery))
	result, mutated, _ := rootedTestWait(t, operation, handle)
	targetAdmissionCloseHook = nil
	if result.status == StatusOk || !mutated || revalidateRootedTarget(admission) != StatusFailed || releaseRootedTarget(admission) != StatusFailed {
		t.Fatal("worker cleanup failure dropped ownership or mutation", result.status, mutated)
	}
	assertTargetAdmissionFlockHeld(t, target)
	if next, status := targetAdmissionTestAcquire(target); next != 0 || status != StatusFailed {
		t.Fatal("worker cleanup failure admitted replacement", next, status)
	}
}

func TestTargetAdmissionWorkerPanicRetainsBorrowUntilFree(t *testing.T) {
	f := newRootedTestFixture(t, false)
	target := filepath.Join(f.base, "target")
	if err := os.Mkdir(target, 0700); err != nil {
		t.Fatal(err)
	}
	admission, status := targetAdmissionTestAcquire(target)
	if status != StatusOk {
		t.Fatal(status)
	}
	t.Cleanup(func() { _ = releaseRootedTarget(admission) })
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			panic("controlled native operation panic")
		}
		return nil
	}
	t.Cleanup(func() { testGate = nil })
	operation, handle := rootedTestStart(t, f, admittedTargetTestRequest(t, f, target, admission, f.first, f.recovery))
	waitAdmittedTargetWorker(t, operation, handle, admission, target)
	result, mutated, _ := rootedTestWait(t, operation, handle)
	testGate = nil
	if result.status != StatusInternal || mutated {
		t.Fatal("prewrite panic misreported", result.status, mutated)
	}
	assertTargetAdmissionFlockHeld(t, target)
	if releaseRootedTarget(admission) != StatusOk {
		t.Fatal("post-panic explicit release failed")
	}
}

func waitAdmittedTargetWorker(t *testing.T, operation uint64, handle *callHandle, admission uint64, target string) {
	t.Helper()
	select {
	case <-handle.doneCh:
	case <-time.After(45 * time.Second):
		t.Fatal("admitted native worker did not actually drain")
	}
	if releaseRootedTarget(admission) != StatusBusy {
		t.Fatal("terminal operation released target before Free")
	}
	if r, status := borrowRootedTarget(admission, "journal"); r != nil || status != StatusBusy {
		t.Fatal("terminal operation lost its live borrow", status)
	}
	if revalidateRootedTarget(admission) != StatusOk {
		t.Fatal("terminal native cleanup lost target binding")
	}
	assertTargetAdmissionFlockHeld(t, target)
}

func TestTargetAdmissionRealLocalAndS3RestoreThroughPostWork(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(map[bool]string{false: "local", true: "S3"}[remote], func(t *testing.T) {
			f := newRootedTestFixture(t, remote)
			target := filepath.Join(f.base, "target")
			if err := os.Mkdir(target, 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(target, "sentinel"), []byte("preserve"), 0600); err != nil {
				t.Fatal(err)
			}
			admission, status := targetAdmissionTestAcquire(target)
			if status != StatusOk {
				t.Fatal(status)
			}
			t.Cleanup(func() { _ = releaseRootedTarget(admission) })
			lockBefore, _ := os.Stat(filepath.Join(target, rootedRestoreLock))
			for _, wanted := range []struct{ snapshot, content string }{
				{f.first, "original record"}, {f.second, "changed record"}, {f.first, "original record"},
			} {
				operation, handle := rootedTestStart(t, f, admittedTargetTestRequest(t, f, target, admission, wanted.snapshot, f.recovery))
				waitAdmittedTargetWorker(t, operation, handle, admission, target)
				result, mutated, metadata := rootedTestWait(t, operation, handle)
				if result.status != StatusOk || !mutated || len(metadata) == 0 {
					t.Fatal("actual admitted restore failed", result.status, mutated)
				}
				for _, name := range []string{"owner.txt", "owner-alias"} {
					data, err := os.ReadFile(filepath.Join(target, name))
					if err != nil || string(data) != wanted.content {
						t.Fatal("actual restored bytes", name, err)
					}
					assertRootedTestXattr(t, filepath.Join(target, name), "owner metadata")
				}
				first, _ := os.Stat(filepath.Join(target, "owner.txt"))
				alias, _ := os.Stat(filepath.Join(target, "owner-alias"))
				lockAfter, _ := os.Stat(filepath.Join(target, rootedRestoreLock))
				if !os.SameFile(first, alias) || !os.SameFile(lockBefore, lockAfter) {
					t.Fatal("restored hardlink or retained lock identity changed")
				}
				assertTargetAdmissionFlockHeld(t, target) // the operation was freed
				if next, status := targetAdmissionTestAcquire(target); next != 0 || status != StatusBusy {
					t.Fatal("Free ended whole-orchestration admission")
				}
				if err := os.WriteFile(filepath.Join(target, "post-work"), []byte("repair under held lease"), 0600); err != nil {
					t.Fatal(err)
				}
				ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
				child := exec.CommandContext(ctx, os.Args[0], "-test.run=^TestRootedRestoreLeaseProcessHelper$")
				child.Env = append(os.Environ(), "SOLSTONE_ROOTED_TEST_LEASE="+filepath.Join(target, rootedRestoreLock))
				output, err := child.CombinedOutput()
				cancel()
				if err != nil {
					t.Fatal("separate cooperating process acquired post-work lease", err, string(output))
				}
			}
			// A real failed operation has fresh mutation state on the same owner.
			operation, handle := rootedTestStart(t, f, admittedTargetTestRequest(t, f, target, admission, f.first, []byte("wrong key")))
			waitAdmittedTargetWorker(t, operation, handle, admission, target)
			result, mutated, _ := rootedTestWait(t, operation, handle)
			if result.status == StatusOk || mutated {
				t.Fatal("prewrite key refusal inherited prior mutation", result.status, mutated)
			}
			if data, _ := os.ReadFile(filepath.Join(target, "sentinel")); string(data) != "preserve" {
				t.Fatal("unrelated sentinel changed")
			}
			if revalidateRootedTarget(admission) != StatusOk || releaseRootedTarget(admission) != StatusOk {
				t.Fatal("final real admission release failed")
			}
			next, status := targetAdmissionTestAcquire(target)
			if status != StatusOk || releaseRootedTarget(next) != StatusOk {
				t.Fatal("actual post-release reacquire failed", status)
			}
		})
	}
}

func TestTargetAdmissionActualWriterCancelKeepsOwnerThroughFree(t *testing.T) {
	f := newRootedTestFixture(t, false)
	target := filepath.Join(f.base, "target")
	if err := os.Mkdir(target, 0700); err != nil {
		t.Fatal(err)
	}
	admission, status := targetAdmissionTestAcquire(target)
	if status != StatusOk {
		t.Fatal(status)
	}
	t.Cleanup(func() { _ = releaseRootedTarget(admission) })
	entered, release := make(chan struct{}), make(chan struct{})
	var enteredOnce, releaseOnce sync.Once
	testGate = func(stage string, _ context.Context) error {
		if stage == "rooted-restore-before-write" {
			data, _ := os.ReadFile(filepath.Join(target, "owner-alias"))
			if string(data) == "original record" {
				enteredOnce.Do(func() { close(entered) })
				<-release
			}
		}
		return nil
	}
	t.Cleanup(func() { testGate = nil })
	operation, handle := rootedTestStart(t, f, admittedTargetTestRequest(t, f, target, admission, f.first, f.password))
	t.Cleanup(func() { releaseOnce.Do(func() { close(release) }) })
	select {
	case <-entered:
	case <-time.After(15 * time.Second):
		t.Fatal("actual native post-write barrier absent")
	}
	if freeCall(operation) != StatusBusy || releaseRootedTarget(admission) != StatusBusy || cancelCall(operation) != StatusOk {
		t.Fatal("live writer lost actual ownership")
	}
	if releaseRootedTarget(admission) != StatusBusy || freeCall(operation) != StatusBusy {
		t.Fatal("cancel request substituted for actual drain")
	}
	assertTargetAdmissionFlockHeld(t, target)
	releaseOnce.Do(func() { close(release) })
	waitAdmittedTargetWorker(t, operation, handle, admission, target)
	result, mutated, _ := rootedTestWait(t, operation, handle)
	testGate = nil
	if result.status == StatusOk || !mutated {
		t.Fatal("post-write interruption lost possible mutation", result.status, mutated)
	}
	assertTargetAdmissionFlockHeld(t, target)
	operation, handle = rootedTestStart(t, f, admittedTargetTestRequest(t, f, target, admission, f.first, f.password))
	result, _, _ = rootedTestWait(t, operation, handle)
	if result.status != StatusOk || releaseRootedTarget(admission) != StatusOk {
		t.Fatal("same-owner actual retry/final release failed", result.status)
	}
}
