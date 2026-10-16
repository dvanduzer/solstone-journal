//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

type directoryFaultCase struct {
	Kind   string
	Remote bool
	Mode   string
}

func TestDirectoryIdentityPublicCMismatchCleanupFaults(t *testing.T) {
	for _, control := range []directoryFaultCase{
		{Kind: "state", Remote: true}, {Kind: "workspace", Remote: false}, {Kind: "workspace", Remote: true},
	} {
		for _, mode := range []string{"error", "panic", "closed-error"} {
			control.Mode = mode
			t.Run(fmt.Sprint(control.Kind, "/remote=", control.Remote, "/", mode), func(t *testing.T) {
				raw, err := json.Marshal(control)
				if err != nil {
					t.Fatal(err)
				}
				ownedRunControlChild(t, "TestDirectoryIdentityMismatchFaultChild", "SOLSTONE_DIRECTORY_FAULT_CHILD", string(raw), "DIRECTORY_FAULT_PROVED")
			})
		}
	}
}

func TestDirectoryIdentityMismatchFaultChild(t *testing.T) {
	raw := os.Getenv("SOLSTONE_DIRECTORY_FAULT_CHILD")
	if raw == "" {
		t.Skip("disposable child only")
	}
	var control directoryFaultCase
	if json.Unmarshal([]byte(raw), &control) != nil {
		t.Fatal("invalid control")
	}
	f := newDirectoryFixture(t, control.Remote)
	f.call(t, OpInit)
	f.call(t, OpSnapshot) // actual new-entry healthy twin
	path, held := f.input.workspace, f.workspace
	stage := "metadata-unassigned-directory"
	if control.Kind == "state" {
		path, held = f.input.state, f.state
		stage = "s3-owner-directory"
	}
	original, err := held.Stat()
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(path, path+"-original"); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(path, 0700); err != nil {
		t.Fatal(err)
	}
	replacement, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	sentinelBase, err := os.Open(f.input.source)
	if err != nil {
		t.Fatal(err)
	}
	defer sentinelBase.Close()
	var actualAcquired, sentinel *os.File
	var attempts atomic.Int32
	var observed atomic.Bool
	var signalled atomic.Bool
	var current atomic.Pointer[ownedCall]
	fault := make(chan *ownedCall, 1)
	previousStage := testOwnedStage
	testOwnedStage = func(name string, entry *ownedCall) {
		if name == "copied" {
			current.Store(entry)
		}
		if name == "fault-retained" && signalled.CompareAndSwap(false, true) {
			fault <- entry
		}
		previousStage(name, entry)
	}
	testDirectoryIdentity = func(kind string, file *os.File, expected *borrowedDirectoryIdentity) {
		if kind != control.Kind {
			return
		}
		info, err := file.Stat()
		if err != nil {
			t.Error(err)
			return
		}
		var a unix.Stat_t
		if err := unix.Fstat(int(held.Fd()), &a); err != nil {
			t.Error(err)
			return
		}
		observed.Store(os.SameFile(info, replacement) && expected.device == uint64(a.Dev) && expected.inode == uint64(a.Ino))
		actualAcquired = file
	}
	testOwnedClose = func(name string, resource io.Closer, actual func() error) error {
		if name != stage {
			return actual()
		}
		if resource != actualAcquired || !observed.Load() {
			t.Error("fault did not reach actual mismatch directory")
			return actual()
		}
		if attempts.Add(1) != 1 {
			t.Error("closer repeated")
			return errors.New("repeated closer")
		}
		entry := current.Load()
		if entry == nil {
			t.Error("request copy observation missing")
		} else {
			for _, secret := range [][]byte{entry.request.password, entry.request.recoveryPassword} {
				for _, v := range secret {
					if v != 0 {
						t.Error("rejection closer preceded secret scrubbing")
						break
					}
				}
			}
		}
		if control.Mode == "panic" {
			panic("synthetic private mismatch close panic")
		}
		if control.Mode == "closed-error" {
			number := int(actualAcquired.Fd())
			if err := actual(); err != nil {
				t.Error(err)
				return err
			}
			reused, err := unix.FcntlInt(sentinelBase.Fd(), unix.F_DUPFD_CLOEXEC, number)
			if err != nil {
				t.Error(err)
				return err
			}
			sentinel = os.NewFile(uintptr(reused), "mismatch failed-close reuse sentinel")
			if reused != number {
				t.Error("failed native FD not reused", reused, number)
			}
		}
		return errors.New("synthetic private mismatch close error")
	}
	returned := make(chan struct{}, 1)
	workers, requests := f.workers.Load(), f.requests.Load()
	go func() { f.start(OpSnapshot); returned <- struct{}{} }()
	var entry *ownedCall
	select {
	case entry = <-fault:
	case <-returned:
		t.Fatal("mismatch closer fault returned as drained")
	case <-time.After(10 * time.Second):
		t.Fatal("actual mismatch closer not reached")
	}
	if !entry.failed() || attempts.Load() != 1 || !observed.Load() {
		t.Fatal("actual mismatch fault lost")
	}
	ownedAwaitActualBlocked(t, false)
	select {
	case <-returned:
		t.Fatal("faulted entry returned")
	default:
	}
	entry.mu.Lock()
	closeRecord := entry.resources[actualAcquired]
	retained := entry.request != nil && closeRecord != nil && closeRecord.started && closeRecord.err == errOwnedCleanup
	if control.Kind == "state" {
		retained = retained && entry.owner != nil && entry.owner.directory == actualAcquired
	}
	entry.mu.Unlock()
	if !retained {
		t.Fatal("actual failed mismatch resource/request ownership lost")
	}
	if f.workers.Load() != workers || f.requests.Load() != requests {
		t.Fatal("refused mismatch reached worker/peer")
	}
	for _, remote := range []bool{false, true} {
		input := f.input
		if !remote {
			input.remote = nil
		} else if input.remote == nil {
			cfg := s3TestConfig()
			input.remote = &cfg
		}
		id, status := directoryCStart(input, OpSnapshot, int(f.source.Fd()), int(f.state.Fd()), int(f.workspace.Fd()), []byte(`[]`), 0)
		if id != 0 || status != StatusFailed {
			t.Fatal("failed mismatch did not seal new-entry replacement", status, id)
		}
	}
	if attempts.Load() != 1 || f.workers.Load() != workers || f.requests.Load() != requests {
		t.Fatal("replacement repeated close/work")
	}
	directoryAssertCaller(t, held, original)
	for _, file := range []*os.File{f.source, f.state, f.workspace} {
		if _, err := file.Stat(); err != nil {
			t.Fatal("caller FD closed", err)
		}
	}
	if sentinel != nil {
		info, err := sentinelBase.Stat()
		if err != nil {
			t.Fatal(err)
		}
		directoryAssertCaller(t, sentinel, info)
	}
	if t.Failed() {
		return
	}
	fmt.Println("DIRECTORY_FAULT_PROVED " + raw)
	// Process isolation owns the permanent seal and deliberately retained native
	// entry stack. No reset, fake terminal result or retry closes that resource.
}
