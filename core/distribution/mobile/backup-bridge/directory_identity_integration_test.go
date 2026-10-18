//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

type directoryFixture struct {
	input                    sourceCInput
	source, state, workspace *os.File
	peer                     *s3MemoryPeer
	requests                 atomic.Int32
	workers                  atomic.Int32
}

func newDirectoryFixture(t *testing.T, remote bool) *directoryFixture {
	t.Helper()
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	f := &directoryFixture{input: sourceCInput{repository: filepath.Join(base, "repo"), source: filepath.Join(base, "journal"), state: s3OwnerTestDirectory(t), workspace: s3OwnerTestDirectory(t), password: []byte("directory-daily-synthetic")}}
	for _, path := range []string{f.input.repository, f.input.source} {
		if err := os.Mkdir(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	for name, body := range map[string]string{"owner.txt": "actual directory-bound owner record", "excluded.dat": "excluded synthetic bytes"} {
		if err := os.WriteFile(filepath.Join(f.input.source, name), []byte(body), 0600); err != nil {
			t.Fatal(err)
		}
	}
	f.source, err = os.Open(f.input.source)
	if err != nil {
		t.Fatal(err)
	}
	f.state, err = os.Open(f.input.state)
	if err != nil {
		t.Fatal(err)
	}
	f.workspace, err = os.Open(f.input.workspace)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { f.source.Close(); f.state.Close(); f.workspace.Close() })
	if remote {
		cfg := s3TestConfig()
		f.input.remote = &cfg
		f.peer = &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	}
	testOwnedStage = func(stage string, entry *ownedCall) {
		if stage == "before-publication" && entry.request.remote != nil {
			entry.request.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				transport := s3TestTransport(func(req *http.Request) (*http.Response, error) { f.requests.Add(1); return f.peer.RoundTrip(req) })
				return &s3Backend{cfg: cfg, ctx: owned, cancel: cancel, transport: transport}, nil
			}
		}
	}
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			f.workers.Add(1)
		}
		return nil
	}
	t.Cleanup(func() { testOwnedStage = nil; testGate = nil; testDirectoryIdentity = nil })
	return f
}

func (f *directoryFixture) start(op int32) (uint64, int32) {
	source, workspace := -1, -1
	var selection []byte
	if op == OpSnapshot {
		source = int(f.source.Fd())
		workspace = int(f.workspace.Fd())
		selection = []byte(`["excluded.dat"]`)
	}
	return directoryCStart(f.input, op, source, int(f.state.Fd()), workspace, selection, 0)
}

func (f *directoryFixture) call(t *testing.T, op int32) bridgeResult {
	t.Helper()
	id, status := f.start(op)
	if status != StatusOk || id == 0 {
		t.Fatal("real directory entry refused", op, status, id)
	}
	result, waited, freed := sourceCWaitAndFree(id)
	if waited != StatusOk || freed != StatusOk || result.status != StatusOk {
		t.Fatal("real directory operation failed", op, result.status, result.diagnostic, waited, freed)
	}
	return result
}

// Cleanup-matrix adapter only: the explicit replacement controls above/below
// supply independently held A descriptors directly, never this convenience path.
func directoryCWithPaths(input sourceCInput, operation int32, sourceFD int, selection []byte) (uint64, int32) {
	state, err := os.Open(input.state)
	if err != nil {
		return 0, StatusInvalidArgument
	}
	defer state.Close()
	workspaceFD := -1
	if operation == OpSnapshot {
		workspace, err := os.Open(input.workspace)
		if err != nil {
			return 0, StatusInvalidArgument
		}
		defer workspace.Close()
		workspaceFD = int(workspace.Fd())
	} else {
		sourceFD = -1
	}
	return directoryCStart(input, operation, sourceFD, int(state.Fd()), workspaceFD, selection, 0)
}

func directoryAssertCaller(t *testing.T, file *os.File, before os.FileInfo) {
	t.Helper()
	after, err := file.Stat()
	if err != nil || !os.SameFile(before, after) {
		t.Fatal("caller descriptor was changed/closed", err)
	}
}

func TestDirectoryIdentityPublicCNormalOperationsAndReadback(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newDirectoryFixture(t, remote)
			beforeSource, err := f.source.Stat()
			if err != nil {
				t.Fatal(err)
			}
			beforeState, err := f.state.Stat()
			if err != nil {
				t.Fatal(err)
			}
			beforeWorkspace, err := f.workspace.Stat()
			if err != nil {
				t.Fatal(err)
			}
			if r := f.call(t, OpInit); !validS3ID(r.keyID) {
				t.Fatal("init returned no strict key ID")
			}
			f.input.recovery = []byte("directory-recovery-distinct")
			if r := f.call(t, OpAddRecoveryKey); !validS3ID(r.keyID) {
				t.Fatal("add-key returned no strict key ID")
			}
			f.input.password, f.input.recovery = f.input.recovery, nil
			f.call(t, OpOpen)
			result := f.call(t, OpSnapshot)
			if !validS3ID(result.snapshotID) {
				t.Fatal("snapshot returned no strict ID")
			}
			if r := f.call(t, OpListSnapshots); r.snapshotCount != 1 {
				t.Fatal("real catalog missed new snapshot", r.snapshotCount)
			}
			f.call(t, OpCheck)
			directoryAssertCaller(t, f.source, beforeSource)
			directoryAssertCaller(t, f.state, beforeState)
			directoryAssertCaller(t, f.workspace, beforeWorkspace)
			restore := filepath.Join(filepath.Dir(f.input.repository), "readback")
			req := bridgeRequest{operation: OpRestore, password: append([]byte(nil), f.input.password...), repoPath: f.input.repository, sourcePath: f.input.source, restorePath: restore, snapshotID: result.snapshotID}
			var id uint64
			var handle *callHandle
			if remote {
				id, handle = s3TestStartOperation(t, *f.input.remote, f.input.state, f.input.workspace, f.peer, req)
			} else {
				if err := pinRequestPaths(&req); err != nil {
					t.Fatal(err)
				}
				var status int32
				id, status = startBridgeCall(req)
				if status != StatusOk {
					t.Fatal(status)
				}
				handlesMu.Lock()
				handle = handles[id]
				handlesMu.Unlock()
			}
			if r := s3TestWaitOperation(t, id, handle); r.status != StatusOk {
				t.Fatal("real encrypted readback failed", r.status, r.diagnostic)
			}
			body, err := os.ReadFile(filepath.Join(restore, filepath.Base(f.input.source), "owner.txt"))
			if err != nil || string(body) != "actual directory-bound owner record" {
				t.Fatal("readback bytes changed", err)
			}
			if _, err := os.Lstat(filepath.Join(restore, filepath.Base(f.input.source), "excluded.dat")); !os.IsNotExist(err) {
				t.Fatal("exclusion was lost", err)
			}
			if f.workers.Load() < 6 || remote && f.requests.Load() == 0 {
				t.Fatal("real worker/peer observers not calibrated")
			}
		})
	}
}

func TestDirectoryIdentityPublicCRefusalAndABA(t *testing.T) {
	for _, kind := range []string{"state", "workspace"} {
		for _, aba := range []bool{false, true} {
			t.Run(fmt.Sprint(kind, "/aba=", aba), func(t *testing.T) {
				remote := kind == "state"
				f := newDirectoryFixture(t, remote)
				f.call(t, OpInit)
				var before []byte
				if remote {
					owner, err := newS3Owner(f.input.state, *f.input.remote)
					if err != nil {
						t.Fatal(err)
					}
					if owner.begin(OpSnapshot, *f.input.remote, f.input.password) != nil || owner.noteSave("") != nil {
						t.Fatal("real pending record was not established")
					}
					s3OwnerTestClose(t, owner)
					before, err = os.ReadFile(filepath.Join(f.input.state, s3OwnerStateName))
					if err != nil {
						t.Fatal(err)
					}
				}
				path, held := f.input.workspace, f.workspace
				if kind == "state" {
					path, held = f.input.state, f.state
				}
				original, err := held.Stat()
				if err != nil {
					t.Fatal(err)
				}
				saved, replaced := path+"-original", path+"-replacement"
				if err := os.Rename(path, saved); err != nil {
					t.Fatal(err)
				}
				if err := os.Mkdir(path, 0700); err != nil {
					t.Fatal(err)
				}
				replacement, err := os.Stat(path)
				if err != nil {
					t.Fatal(err)
				}
				// A recognized residue would be removed by accepted workspace cleanup.
				residue := filepath.Join(path, metadataDirectoryPrefix+"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
				if kind == "workspace" {
					if err := os.Mkdir(residue, 0700); err != nil {
						t.Fatal(err)
					}
				}
				var observed, afterComparison bool
				var hookErr error
				priorStage := testOwnedStage
				testOwnedStage = func(stage string, entry *ownedCall) {
					if observed && stage == "before-adopt-root" {
						afterComparison = true
					}
					priorStage(stage, entry)
				}
				testDirectoryIdentity = func(actualKind string, file *os.File, expected *borrowedDirectoryIdentity) {
					if actualKind != kind {
						return
					}
					actual, err := file.Stat()
					if err != nil {
						hookErr = err
						return
					}
					var a unix.Stat_t
					if err := unix.Fstat(int(held.Fd()), &a); err != nil {
						hookErr = err
						return
					}
					observed = os.SameFile(actual, replacement) && uint64(a.Dev) == expected.device && uint64(a.Ino) == expected.inode
					if aba {
						if err := os.Rename(path, replaced); err != nil {
							hookErr = err
							return
						}
						hookErr = os.Rename(saved, path)
					}
				}
				workers, requests := f.workers.Load(), f.requests.Load()
				id, status := f.start(OpSnapshot)
				if id != 0 {
					sourceCCancel(id)
					sourceCWaitAndFree(id)
				}
				if hookErr != nil || !observed || afterComparison || status != StatusInvalidArgument || id != 0 || f.workers.Load() != workers || f.requests.Load() != requests {
					t.Fatal("comparison did not itself reject acquired B before later adoption/work", hookErr, observed, afterComparison, status, id)
				}
				testDirectoryIdentity = nil
				directoryAssertCaller(t, held, original)
				b := path
				if aba {
					b = replaced
				}
				if kind == "workspace" {
					if _, err := os.Stat(filepath.Join(b, filepath.Base(residue))); err != nil {
						t.Fatal("refused B scratch residue changed", err)
					}
				} else {
					names, err := os.ReadDir(b)
					if err != nil || len(names) != 0 {
						t.Fatal("refused B state was touched", err)
					}
				}
				if !aba {
					if err := os.Rename(path, replaced); err != nil {
						t.Fatal(err)
					}
					if err := os.Rename(saved, path); err != nil {
						t.Fatal(err)
					}
				}
				if remote {
					after, err := os.ReadFile(filepath.Join(path, s3OwnerStateName))
					if err != nil || !bytes.Equal(before, after) {
						t.Fatal("A pending state changed", err)
					}
					owner, err := newS3Owner(path, *f.input.remote)
					if err != nil {
						t.Fatal(err)
					}
					// Init cannot adopt an uncertain attempt, so it must still refuse.
					pending := owner.begin(OpInit, *f.input.remote, f.input.password)
					s3OwnerTestClose(t, owner)
					if !errors.Is(pending, errS3OwnerPending) {
						t.Fatal("matching A reopen bypassed pending", pending)
					}
					after, err = os.ReadFile(filepath.Join(path, s3OwnerStateName))
					if err != nil || !bytes.Equal(before, after) {
						t.Fatal("matching refusal changed A", err)
					}
				}
			})
		}
	}
}

func TestDirectoryIdentityPublicCInvalidAndUnsupported(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newDirectoryFixture(t, remote)
			f.call(t, OpInit)
			regular, err := os.Open(filepath.Join(f.input.source, "owner.txt"))
			if err != nil {
				t.Fatal(err)
			}
			defer regular.Close()
			closed, err := unix.FcntlInt(f.source.Fd(), unix.F_DUPFD_CLOEXEC, 128)
			if err != nil {
				t.Fatal(err)
			}
			if err := unix.Close(closed); err != nil {
				t.Fatal(err)
			}
			workers, requests := f.workers.Load(), f.requests.Load()
			refuse := func(op int32, source, state, workspace int, policy []byte) {
				t.Helper()
				id, status := directoryCStart(f.input, op, source, state, workspace, policy, 0)
				if id != 0 {
					sourceCCancel(id)
					sourceCWaitAndFree(id)
				}
				if id != 0 || status != StatusInvalidArgument || f.workers.Load() != workers || f.requests.Load() != requests {
					t.Fatal("invalid new entry reached work", op, source, state, workspace, status, id)
				}
			}
			for _, bad := range []int{-2, -1, closed, int(regular.Fd())} {
				refuse(OpSnapshot, bad, int(f.state.Fd()), int(f.workspace.Fd()), []byte(`[]`))
				refuse(OpSnapshot, int(f.source.Fd()), int(f.state.Fd()), bad, []byte(`[]`))
				if remote {
					refuse(OpOpen, -1, bad, -1, nil)
				}
			}
			refuse(OpSnapshot, int(f.source.Fd()), int(f.state.Fd()), int(f.source.Fd()), []byte(`[]`))
			if remote {
				refuse(OpSnapshot, int(f.source.Fd()), int(f.workspace.Fd()), int(f.state.Fd()), []byte(`[]`))
			}
			refuse(OpOpen, int(f.source.Fd()), int(f.state.Fd()), -1, nil)
			refuse(OpOpen, -1, int(f.state.Fd()), int(f.workspace.Fd()), nil)
			refuse(OpOpen, -1, int(f.state.Fd()), -1, []byte(`[]`))
			for _, op := range []int32{-1, 0, OpRestore, OpS3Inspect, OpS3Inspect + 1} {
				refuse(op, -1, int(f.state.Fd()), -1, nil)
			}
		})
	}
}

func TestDirectoryIdentityPublicCPrivateFDReuseAfterEntry(t *testing.T) {
	for _, remote := range []bool{false, true} {
		t.Run(fmt.Sprint("remote=", remote), func(t *testing.T) {
			f := newDirectoryFixture(t, remote)
			f.call(t, OpInit)
			entered, proceed := make(chan struct{}), make(chan struct{})
			var once sync.Once
			release := func() { once.Do(func() { close(proceed) }) }
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
			id, status := f.start(OpSnapshot)
			if status != StatusOk || id == 0 {
				t.Fatal(status, id)
			}
			complete := false
			defer func() {
				release()
				if !complete {
					sourceCCancel(id)
					sourceCWaitAndFree(id)
				}
			}()
			select {
			case <-entered:
			case <-time.After(5 * time.Second):
				t.Fatal("actual worker not held")
			}
			sentinelBase, err := os.Open(f.input.source)
			if err != nil {
				t.Fatal(err)
			}
			defer sentinelBase.Close()
			files := []*os.File{f.workspace}
			if remote {
				files = append(files, f.state)
			}
			var sentinels []*os.File
			for _, file := range files {
				number := int(file.Fd())
				if err := file.Close(); err != nil {
					t.Fatal(err)
				}
				reused, err := unix.FcntlInt(sentinelBase.Fd(), unix.F_DUPFD_CLOEXEC, number)
				if err != nil || reused != number {
					t.Fatal("reuse was not established", err, reused, number)
				}
				sentinel := os.NewFile(uintptr(reused), "caller private fd reuse sentinel")
				sentinels = append(sentinels, sentinel)
				defer sentinel.Close()
			}
			release()
			result, waited, freed := sourceCWaitAndFree(id)
			complete = true
			if result.status != StatusOk || waited != StatusOk || freed != StatusOk {
				t.Fatal("private FD reuse broke real completion", result.status, result.diagnostic, waited, freed)
			}
			before, err := sentinelBase.Stat()
			if err != nil {
				t.Fatal(err)
			}
			for _, file := range sentinels {
				directoryAssertCaller(t, file, before)
			}
		})
	}
}
