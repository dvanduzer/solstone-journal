//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"fmt"
	"golang.org/x/sys/unix"
	"net/http"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/restic/restic/internal/repository"
	"time"
)

func TestSourceIdentityPublicCMatchingTwinsAndSelection(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newRootedTestFixture(t, remote)
			f.source += "-retained"
			if err := os.Chmod(f.source, 0755); err != nil {
				t.Fatal(err)
			}
			original := []byte("actual new public source export owner record")
			if err := os.WriteFile(filepath.Join(f.source, "owner.txt"), original, 0640); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(f.source, "exclude.dat"), []byte("excluded synthetic bytes"), 0600); err != nil {
				t.Fatal(err)
			}
			held, err := os.Open(f.source)
			if err != nil {
				t.Fatal(err)
			}
			defer held.Close()
			wrongPath := filepath.Join(f.base, "admissible-other")
			if err := os.Mkdir(wrongPath, 0755); err != nil {
				t.Fatal(err)
			}
			wrong, err := os.Open(wrongPath)
			if err != nil {
				t.Fatal(err)
			}
			defer wrong.Close()
			var workers, requests atomic.Int32
			testGate = func(stage string, _ context.Context) error {
				if stage == "before-restic" {
					workers.Add(1)
				}
				return nil
			}
			testSourceIdentity = func(_ string, req *bridgeRequest) {
				if req.remote != nil {
					req.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
						owned, cancel := context.WithCancel(ctx)
						transport := s3TestTransport(func(request *http.Request) (*http.Response, error) { requests.Add(1); return f.peer.RoundTrip(request) })
						return &s3Backend{cfg: cfg, transport: transport, ctx: owned, cancel: cancel}, nil
					}
				}
			}
			t.Cleanup(func() { testGate = nil; testSourceIdentity = nil })
			input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password}
			if remote {
				input.remote = &f.cfg
			}
			regular, err := os.Open(filepath.Join(f.source, "owner.txt"))
			if err != nil {
				t.Fatal(err)
			}
			defer regular.Close()
			closed, err := unix.FcntlInt(held.Fd(), unix.F_DUPFD_CLOEXEC, 128)
			if err != nil {
				t.Fatal(err)
			}
			if err := unix.Close(closed); err != nil {
				t.Fatal(err)
			}
			var absent unix.Stat_t
			if unix.Fstat(closed, &absent) != unix.EBADF {
				t.Fatal("closed descriptor control not established")
			}
			for _, invalid := range []int{-1, closed, int(regular.Fd())} {
				id, status := sourceCStart(input, invalid, []byte(`["exclude.dat"]`), 0)
				if id != 0 {
					_ = cancelCall(id)
					_, _, _ = sourceCWaitAndFree(id)
				}
				if status != StatusInvalidArgument || id != 0 || workers.Load() != 0 || requests.Load() != 0 {
					t.Fatal("invalid descriptor reached actual public work or output sentinel survived", invalid, status, id, workers.Load(), requests.Load())
				}
			}
			id, status := sourceCStart(input, int(wrong.Fd()), []byte(`["exclude.dat"]`), 0)
			if status != StatusInvalidArgument || id != 0 || workers.Load() != 0 || requests.Load() != 0 {
				if id != 0 {
					_ = cancelCall(id)
					_, _, _ = sourceCWaitAndFree(id)
				}
				t.Fatal("otherwise-valid mismatch reached work", status, id, workers.Load(), requests.Load())
			}
			id, status = sourceCStart(input, int(held.Fd()), []byte(`["exclude.dat"]`), 0)
			if status != StatusOk || id == 0 {
				t.Fatal("matching public C entry refused", status, id)
			}
			result, waited, freed := sourceCWaitAndFree(id)
			if waited != StatusOk || freed != StatusOk || result.status != StatusOk || !validS3ID(result.snapshotID) || workers.Load() == 0 || (remote && requests.Load() == 0) {
				t.Fatal("matching public C snapshot did not perform actual work", result.status, result.diagnostic, waited, freed, workers.Load(), requests.Load())
			}
			testGate = nil
			testSourceIdentity = nil
			if err := held.Close(); err != nil {
				t.Fatal(err)
			}
			restore := filepath.Join(f.base, "source-export-restore")
			req := bridgeRequest{operation: OpRestore, password: append([]byte(nil), f.recovery...), repoPath: f.repo, sourcePath: f.source, restorePath: restore, snapshotID: result.snapshotID}
			var handle *callHandle
			if remote {
				id, handle = s3TestStartOperation(t, f.cfg, f.state, f.workspace, f.peer, req)
			} else {
				if err := pinRequestPaths(&req); err != nil {
					t.Fatal(err)
				}
				id, status = startBridgeCall(req)
				if status != StatusOk {
					t.Fatal(status)
				}
				handlesMu.Lock()
				handle = handles[id]
				handlesMu.Unlock()
			}
			if restored := s3TestWaitOperation(t, id, handle); restored.status != StatusOk {
				t.Fatal("actual source-export restore failed", restored.status, restored.diagnostic)
			}
			base := filepath.Join(restore, filepath.Base(f.source))
			got, err := os.ReadFile(filepath.Join(base, "owner.txt"))
			if err != nil || string(got) != string(original) {
				t.Fatal("source-export owner bytes changed", err)
			}
			if _, err := os.Lstat(filepath.Join(base, "exclude.dat")); !os.IsNotExist(err) {
				t.Fatal("public export dropped real exclusion", err)
			}
		})
	}
}

func TestSourceIdentityPublicCCapturedPathABA(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newRootedTestFixture(t, remote)
			f.source += "-retained"
			held, err := os.Open(f.source)
			if err != nil {
				t.Fatal(err)
			}
			defer held.Close()
			originalInfo, err := held.Stat()
			if err != nil {
				t.Fatal(err)
			}
			saved := f.source + "-aba-original"
			replacement := f.source + "-aba-replacement"
			if err := os.Rename(f.source, saved); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(f.source, 0755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(f.source, "owner.txt"), []byte("replacement must not be read"), 0600); err != nil {
				t.Fatal(err)
			}
			replacementInfo, err := os.Stat(f.source)
			if err != nil {
				t.Fatal(err)
			}
			var witnessed bool
			var hookErr error
			var workers atomic.Int32
			testGate = func(stage string, _ context.Context) error {
				if stage == "before-restic" {
					workers.Add(1)
				}
				return nil
			}
			testSourceIdentity = func(_ string, req *bridgeRequest) {
				info, err := req.sourceRoot.Stat(".")
				if err != nil {
					hookErr = err
					return
				}
				rootMatched := os.SameFile(info, replacementInfo)
				fileMatched := false
				for _, file := range req.pathFiles {
					info, err := file.Stat()
					if err != nil {
						hookErr = err
						return
					}
					fileMatched = fileMatched || os.SameFile(info, replacementInfo)
				}
				witnessed = rootMatched && fileMatched
				if err := os.Rename(f.source, replacement); err != nil {
					hookErr = err
					return
				}
				hookErr = os.Rename(saved, f.source)
			}
			t.Cleanup(func() { testGate = nil; testSourceIdentity = nil })
			input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password}
			if remote {
				input.remote = &f.cfg
			}
			id, status := sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
			if id != 0 {
				_ = cancelCall(id)
				_, _, _ = sourceCWaitAndFree(id)
			}
			current, err := os.Stat(f.source)
			if hookErr != nil || !witnessed || err != nil || !os.SameFile(current, originalInfo) || status != StatusInvalidArgument || id != 0 || workers.Load() != 0 {
				t.Fatal("public ABA did not refuse the captured replacement", hookErr, witnessed, err, status, id, workers.Load())
			}
		})
	}
}

func TestSourceIdentityPublicCSourceRemainsPinnedAfterReturn(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newRootedTestFixture(t, remote)
			f.source += "-retained"
			held, err := os.Open(f.source)
			if err != nil {
				t.Fatal(err)
			}
			number := int(held.Fd())
			defer held.Close()
			wrongPath := filepath.Join(f.base, "reused-descriptor")
			if err := os.Mkdir(wrongPath, 0700); err != nil {
				t.Fatal(err)
			}
			wrong, err := os.Open(wrongPath)
			if err != nil {
				t.Fatal(err)
			}
			defer wrong.Close()
			entered, proceed := make(chan struct{}), make(chan struct{})
			var releaseOnce sync.Once
			release := func() { releaseOnce.Do(func() { close(proceed) }) }
			defer release()
			testGate = func(stage string, ctx context.Context) error {
				if stage == "before-restic" {
					close(entered)
					select {
					case <-proceed:
					case <-ctx.Done():
						return ctx.Err()
					}
				}
				return nil
			}
			testSourceIdentity = func(_ string, req *bridgeRequest) {
				if req.remote != nil {
					req.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
						owned, cancel := context.WithCancel(ctx)
						return &s3Backend{cfg: cfg, transport: f.peer, ctx: owned, cancel: cancel}, nil
					}
				}
			}
			t.Cleanup(func() { testGate = nil; testSourceIdentity = nil })
			input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password}
			if remote {
				input.remote = &f.cfg
			}
			id, status := sourceCStart(input, number, []byte(`[]`), 0)
			if status != StatusOk || id == 0 {
				t.Fatal("public source entry refused", status, id)
			}
			completed := false
			defer func() {
				release()
				if !completed {
					_ = cancelCall(id)
					_, _, _ = sourceCWaitAndFree(id)
				}
			}()
			select {
			case <-entered:
			case <-time.After(5 * time.Second):
				t.Fatal("actual worker did not enter held stage")
			}
			if err := held.Close(); err != nil {
				t.Fatal(err)
			}
			duplicate, err := unix.FcntlInt(wrong.Fd(), unix.F_DUPFD_CLOEXEC, number)
			if err != nil {
				t.Fatal(err)
			}
			reused := os.NewFile(uintptr(duplicate), "reused caller fd sentinel")
			defer reused.Close()
			if duplicate != number {
				t.Fatal("caller descriptor reuse control was not established", duplicate, number)
			}
			retained := f.source + "-after-entry"
			if err := os.Rename(f.source, retained); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(f.source, 0755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(f.source, "owner.txt"), []byte("replacement after entry"), 0600); err != nil {
				t.Fatal(err)
			}
			release()
			result, waited, freed := sourceCWaitAndFree(id)
			completed = true
			if waited != StatusOk || freed != StatusOk {
				t.Fatal("actual wait/free failed", waited, freed)
			}
			sentinel, err := reused.Stat()
			other, otherErr := wrong.Stat()
			if err != nil || otherErr != nil || !os.SameFile(sentinel, other) {
				t.Fatal("native borrowed/reclosed reused caller descriptor", err, otherErr)
			}
			testGate = nil
			testSourceIdentity = nil
			if result.status == StatusOk {
				restore := filepath.Join(f.base, "post-entry-restore")
				req := bridgeRequest{operation: OpRestore, password: append([]byte(nil), f.recovery...), repoPath: f.repo, sourcePath: f.source, restorePath: restore, snapshotID: result.snapshotID}
				var handle *callHandle
				if remote {
					id, handle = s3TestStartOperation(t, f.cfg, f.state, f.workspace, f.peer, req)
				} else {
					if err := pinRequestPaths(&req); err != nil {
						t.Fatal(err)
					}
					id, status = startBridgeCall(req)
					if status != StatusOk {
						t.Fatal(status)
					}
					handlesMu.Lock()
					handle = handles[id]
					handlesMu.Unlock()
				}
				if restored := s3TestWaitOperation(t, id, handle); restored.status != StatusOk {
					t.Fatal("post-entry actual restore failed", restored.status, restored.diagnostic)
				}
				got, err := os.ReadFile(filepath.Join(restore, filepath.Base(f.source), "owner.txt"))
				if err != nil || string(got) != "changed record" {
					t.Fatal("post-entry snapshot read replacement bytes", string(got), err)
				}
			}
		})
	}
}

func TestSourceIdentityPublicCActualLockRetry(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newRootedTestFixture(t, remote)
			f.source += "-retained"
			held, err := os.Open(f.source)
			if err != nil {
				t.Fatal(err)
			}
			defer held.Close()
			var owner *repository.Repository
			if remote {
				owner, err = repository.New(guardedBackend{s3TestBackend(t, f.cfg, f.peer)}, repository.Options{})
				if err != nil {
					t.Fatal(err)
				}
				if _, err = safeScanKey(context.Background(), owner, string(f.password)); err != nil {
					t.Fatal(err)
				}
			} else {
				owner = optionsLockRepository(t, bridgeRequest{repoPath: f.repo, password: append([]byte(nil), f.password...)}, nil)
			}
			unlock, _, err := lockRepo(context.Background(), owner, true)
			if err != nil {
				t.Fatal(err)
			}
			defer unlock()
			var mu sync.Mutex
			var observations []time.Time
			var releaseAtSecond bool
			entered, proceed := make(chan struct{}), make(chan struct{})
			var releaseOnce sync.Once
			release := func() { releaseOnce.Do(func() { close(proceed) }) }
			defer release()
			observe := func(ctx context.Context) {
				mu.Lock()
				observations = append(observations, time.Now())
				n := len(observations)
				block := releaseAtSecond && n == 2
				mu.Unlock()
				if block {
					close(entered)
					select {
					case <-proceed:
					case <-ctx.Done():
					}
				}
			}
			if !remote {
				testRootLockList = observe
			}
			testSourceIdentity = func(_ string, req *bridgeRequest) {
				if req.remote != nil {
					req.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
						owned, cancel := context.WithCancel(ctx)
						transport := s3TestTransport(func(request *http.Request) (*http.Response, error) {
							if request.Method == http.MethodGet && request.URL.Query().Get("prefix") == cfg.prefix+"/locks/" {
								observe(request.Context())
							}
							return f.peer.RoundTrip(request)
						})
						return &s3Backend{cfg: cfg, transport: transport, ctx: owned, cancel: cancel}, nil
					}
				}
			}
			t.Cleanup(func() { testRootLockList = nil; testSourceIdentity = nil })
			input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password}
			if remote {
				input.remote = &f.cfg
			}
			// The pinned SDK makes a final attempt even with retry=0. Keep the owner
			// held for this calibrated negative and measure only actual lock attempts.
			id, status := sourceCStart(input, int(held.Fd()), []byte(`[]`), 0)
			if status != StatusOk || id == 0 {
				t.Fatal("zero retry did not admit real worker", status, id)
			}
			result, waited, freed := sourceCWaitAndFree(id)
			mu.Lock()
			zeroTimes := append([]time.Time(nil), observations...)
			observations = nil
			releaseAtSecond = true
			mu.Unlock()
			budget := 2 * time.Second
			if waited != StatusOk || freed != StatusOk || result.status == StatusOk || result.snapshotID != "" || len(zeroTimes) < 2 || zeroTimes[1].Sub(zeroTimes[0]) >= budget/2 {
				t.Fatal("zero-retry/held-lock calibration failed", result.status, result.diagnostic, waited, freed, len(zeroTimes))
			}
			id, status = sourceCStart(input, int(held.Fd()), []byte(`[]`), uint64(budget/time.Millisecond))
			if status != StatusOk || id == 0 {
				t.Fatal("retry source entry refused", status, id)
			}
			joined := false
			defer func() {
				release()
				if !joined {
					_ = cancelCall(id)
					_, _, _ = sourceCWaitAndFree(id)
				}
			}()
			select {
			case <-entered:
			case <-time.After(10 * time.Second):
				t.Fatal("actual public source retry did not reach second acquisition")
			}
			mu.Lock()
			times := append([]time.Time(nil), observations...)
			mu.Unlock()
			if len(times) != 2 || times[1].Sub(times[0]) < budget-100*time.Millisecond {
				t.Error("public source entry ignored retry budget", len(times), times[1].Sub(times[0]))
			}
			unlock() // only release after the real delayed acquisition is witnessed
			release()
			result, waited, freed = sourceCWaitAndFree(id)
			joined = true
			if waited != StatusOk || freed != StatusOk || result.status != StatusOk || !validS3ID(result.snapshotID) {
				t.Fatal("actual public retry snapshot failed", result.status, result.diagnostic, waited, freed)
			}
		})
	}
}

func TestS3GenericEntriesRejectOp10BeforeOwnerAdmission(t *testing.T) {
	type stateCase struct {
		name  string
		setup func(owner *s3Owner) error
	}
	cases := []stateCase{
		{
			name: "lock-id",
			setup: func(owner *s3Owner) error {
				return owner.noteSave(s3TestID(10))
			},
		},
		{
			name: "may-have-written-no-locks",
			setup: func(owner *s3Owner) error {
				return owner.noteSave("")
			},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := s3TestConfig()
			root := s3OwnerTestDirectory(t)
			owner := s3OwnerTestOpen(t, root, cfg)
			if err := owner.begin(OpRemoveRecoveryKey, cfg, []byte("daily-1")); err != nil {
				t.Fatalf("begin failed: %v", err)
			}
			if err := tc.setup(owner); err != nil {
				t.Fatalf("setup failed: %v", err)
			}
			s3OwnerTestClose(t, owner)

			stateFilePath := filepath.Join(root, s3OwnerStateName)
			beforeBytes, err := os.ReadFile(stateFilePath)
			if err != nil {
				t.Fatalf("read state file failed: %v", err)
			}

			priorStage := testOwnedStage
			tripwireCalled := false
			testOwnedStage = func(stage string, entry *ownedCall) {
				tripwireCalled = true
				t.Errorf("unexpected testOwnedStage call: stage=%s", stage)
			}
			defer func() { testOwnedStage = priorStage }()

			input := sourceCInput{
				remote:   &cfg,
				state:    root,
				password: []byte("daily-1"),
			}

			id1, status1 := sourceCRawS3Start(input, OpRemoveRecoveryKey)
			if status1 != StatusInvalidArgument {
				t.Fatalf("sourceCRawS3Start status = %d, expected %d", status1, StatusInvalidArgument)
			}
			if id1 != 0 {
				t.Fatalf("sourceCRawS3Start id = %d, expected 0", id1)
			}
			if tripwireCalled {
				t.Fatal("tripwire called during sourceCRawS3Start")
			}
			afterBytes1, err := os.ReadFile(stateFilePath)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(beforeBytes, afterBytes1) {
				t.Fatal("state file modified by sourceCRawS3Start")
			}

			id2, status2 := sourceCRawS3OperationV1Start(input, OpRemoveRecoveryKey)
			if status2 != StatusInvalidArgument {
				t.Fatalf("sourceCRawS3OperationV1Start status = %d, expected %d", status2, StatusInvalidArgument)
			}
			if id2 != 0 {
				t.Fatalf("sourceCRawS3OperationV1Start id = %d, expected 0", id2)
			}
			if tripwireCalled {
				t.Fatal("tripwire called during sourceCRawS3OperationV1Start")
			}
			afterBytes2, err := os.ReadFile(stateFilePath)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(beforeBytes, afterBytes2) {
				t.Fatal("state file modified by sourceCRawS3OperationV1Start")
			}
		})
	}
}
