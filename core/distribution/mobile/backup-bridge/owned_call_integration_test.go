//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

type ownedFaultCase struct {
	Stage     string `json:"stage"`
	Remote    bool   `json:"remote"`
	Published bool   `json:"published"`
	Mode      string `json:"mode"`
	Setup     string `json:"setup,omitempty"`
	Options   bool   `json:"options,omitempty"`
	Operation int32  `json:"operation,omitempty"`
}

func ownedControlledFactory(entry *ownedCall, peer *s3MemoryPeer) {
	entry.mu.Lock()
	req := entry.request
	entry.mu.Unlock()
	if req != nil && req.remote != nil {
		req.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
			owned, cancel := context.WithCancel(ctx)
			return &s3Backend{cfg: cfg, transport: peer, ctx: owned, cancel: cancel}, nil
		}
	}
}
func ownedAwaitActualBlocked(t *testing.T, needWait bool) {
	t.Helper()
	for attempt := 0; attempt < 128; attempt++ {
		stack := make([]byte, 2*1024*1024)
		n := runtime.Stack(stack, true)
		text := string(stack[:n])
		if strings.Contains(text, "(*ownedCall).blockIfFault") && strings.Contains(text, "[select (no cases)") && (!needWait || strings.Contains(text, ".restic_bridge_wait(")) {
			return
		}
		runtime.Gosched()
	}
	t.Fatal("actual native retained entry/worker and wait stacks were not observed")
}

// Each permanent fault owns a disposable process. No reset opens the seal.
func TestOwnedPublicCleanupFaultsRetainActualCustody(t *testing.T) {
	cases := []ownedFaultCase{
		{Stage: "repository-root", Remote: false, Published: false, Mode: "error", Setup: ""},
		{Stage: "source-root", Remote: false, Published: false, Mode: "panic", Setup: ""},
		{Stage: "request-directory", Remote: false, Published: false, Mode: "closed-error", Setup: ""},
		{Stage: "metadata-root", Remote: false, Published: false, Mode: "error", Setup: ""},
		{Stage: "metadata-directory", Remote: false, Published: true, Mode: "closed-error", Setup: ""},
		{Stage: "metadata-census", Remote: false, Published: true, Mode: "error", Setup: ""},
		{Stage: "s3-owner-state-file", Remote: true, Published: false, Mode: "closed-error", Setup: ""},
		{Stage: "s3-owner-root", Remote: true, Published: false, Mode: "panic", Setup: ""},
		{Stage: "s3-owner-directory", Remote: true, Published: true, Mode: "error", Setup: ""},
		{Stage: "s3-operation-backend", Remote: true, Published: true, Mode: "panic", Setup: ""},
		{Stage: "s3-state-write-temporary", Remote: true, Published: true, Mode: "closed-error", Setup: ""},
		{Stage: "s3-state-displaced-file", Remote: true, Published: true, Mode: "closed-error", Setup: ""},
		{Stage: "directory-open-ancestor", Remote: false, Published: false, Mode: "closed-error", Setup: ""},
		{Stage: "ancestry-chain", Remote: false, Published: false, Mode: "error", Setup: ""},
		{Stage: "adopt-rejected-root", Remote: false, Published: false, Mode: "panic", Setup: "replace-source-before-adopt"},
		{Stage: "metadata-unassigned-directory", Remote: false, Published: false, Mode: "closed-error", Setup: "workspace-mode"},
		{Stage: "metadata-overlap-directory", Remote: false, Published: false, Mode: "error", Setup: "workspace-overlap"},
		{Stage: "metadata-census", Remote: false, Published: false, Mode: "panic", Setup: ""},
		{Stage: "metadata-residue-directory", Remote: false, Published: true, Mode: "closed-error", Setup: "recognized-residue"},
		{Stage: "metadata-residue-census", Remote: false, Published: true, Mode: "error", Setup: "recognized-residue"},
		{Stage: "metadata-leaf-census", Remote: false, Published: true, Mode: "panic", Setup: "recognized-residue"},
		{Stage: "metadata-leaf-identity", Remote: false, Published: true, Mode: "closed-error", Setup: "recognized-residue"},
		{Stage: "s3-state-census", Remote: true, Published: false, Mode: "closed-error", Setup: "invalid-state-census"},
		{Stage: "s3-state-rejected-read", Remote: true, Published: false, Mode: "panic", Setup: "invalid-state-record"},
		{Stage: "s3-state-read-identity", Remote: true, Published: true, Mode: "closed-error", Setup: "state-before-rename-error"},
		{Stage: "s3-operation-backend", Remote: true, Published: true, Mode: "panic", Setup: "cleanup-backend"},
	}
	cases = append(cases,
		ownedFaultCase{Stage: "repository-root", Published: true, Mode: "error", Options: true},
		ownedFaultCase{Stage: "metadata-unassigned-directory", Mode: "closed-error", Setup: "workspace-mode", Options: true},
		ownedFaultCase{Stage: "s3-owner-state-file", Remote: true, Published: true, Mode: "error", Options: true},
		ownedFaultCase{Stage: "s3-state-census", Remote: true, Mode: "panic", Setup: "invalid-state-census", Options: true},
		ownedFaultCase{Stage: "adopt-rejected-root", Mode: "panic", Setup: "replace-repo-before-adopt", Options: true, Operation: OpInit},
		ownedFaultCase{Stage: "s3-state-census", Remote: true, Mode: "closed-error", Setup: "invalid-state-census", Options: true, Operation: OpInit},
	)
	if runtime.GOOS == "darwin" {
		cases = append(cases, ownedFaultCase{Stage: "metadata-clone-directory", Remote: false, Published: true, Mode: "closed-error", Setup: ""})
	}

	for _, control := range cases {
		t.Run(fmt.Sprintf("%s/remote=%t/published=%t/%s", fmt.Sprint(control.Stage, "/", control.Setup, "/options=", control.Options, "/operation=", control.Operation), control.Remote, control.Published, control.Mode), func(t *testing.T) {
			encoded, err := json.Marshal(control)
			if err != nil {
				t.Fatal(err)
			}
			ownedRunControlChild(t, "TestOwnedPublicCleanupFaultChild", "SOLSTONE_OWNED_FAULT_CHILD", string(encoded), "OWNED_FAULT_CONTROL_PROVED")
		})
	}
}

func TestOwnedPublicCleanupFaultChild(t *testing.T) {
	raw := os.Getenv("SOLSTONE_OWNED_FAULT_CHILD")
	if raw == "" {
		t.Skip("disposable child only")
	}
	var control ownedFaultCase
	if json.Unmarshal([]byte(raw), &control) != nil {
		t.Fatal("invalid private control")
	}
	f := newRootedTestFixture(t, control.Remote)
	f.source += "-retained"
	held, err := os.Open(f.source)
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	wrongPath := filepath.Join(f.base, "source-mismatch")
	if err := os.Mkdir(wrongPath, 0755); err != nil {
		t.Fatal(err)
	}
	wrong, err := os.Open(wrongPath)
	if err != nil {
		t.Fatal(err)
	}
	defer wrong.Close()
	// Create the replacement source for fd-reuse before the actual closer runs.
	sentinelBase, err := os.Open(f.source)
	if err != nil {
		t.Fatal(err)
	}
	defer sentinelBase.Close()
	var sentinel *os.File
	var failedResource io.Closer
	var recoveryCopied atomic.Bool
	var rejectionUnscrubbed atomic.Bool
	var attempts, copied, opens, workers atomic.Int32
	var consumed atomic.Bool
	var current atomic.Pointer[ownedCall]
	var replaced atomic.Bool
	var deleteAttempts atomic.Int32
	var setupArmed atomic.Bool
	fault := make(chan *ownedCall, 1)
	testOwnedStage = func(stage string, entry *ownedCall) {
		switch stage {
		case "copied":
			current.Store(entry)
			if setupArmed.Load() && control.Operation == OpInit {
				req := entry.request
				recoveryCopied.Store(len(req.password) > 0 && len(req.recoveryPassword) > 0 && string(req.password) != string(req.recoveryPassword))
			}
			copied.Add(1)
		case "resource-open":
			opens.Add(1)
		case "before-publication":
			ownedControlledFactory(entry, f.peer)
			if setupArmed.Load() && control.Setup == "state-before-rename-error" {
				entry.owner.fault = func(stage string) error {
					if stage == "before-rename" {
						return errors.New("synthetic pre-rename refusal")
					}
					return nil
				}
			}
			if setupArmed.Load() && control.Setup == "cleanup-backend" {
				entry.request.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
					owned, cancel := context.WithCancel(ctx)
					return &s3Backend{cfg: cfg, ctx: owned, cancel: cancel, transport: s3TestTransport(func(request *http.Request) (*http.Response, error) {
						if request.Method == http.MethodDelete && strings.Contains(request.URL.Path, "/locks/") && deleteAttempts.Add(1) == 1 {
							return nil, errS3IO
						}
						return f.peer.RoundTrip(request)
					})}, nil
				}
			}
		case "before-adopt-root":
			if setupArmed.Load() && control.Setup == "replace-source-before-adopt" && entry.request.repoRoot != nil && entry.request.sourceRoot == nil && replaced.CompareAndSwap(false, true) {
				if err := os.Rename(f.source, f.source+"-adopt-original"); err != nil {
					t.Error(err)
				}
				if err := os.Mkdir(f.source, 0700); err != nil {
					t.Error(err)
				}
			}
			if setupArmed.Load() && control.Setup == "replace-repo-before-adopt" && entry.request.repoRoot == nil && replaced.CompareAndSwap(false, true) {
				if err := os.Rename(f.repo, f.repo+"-adopt-original"); err != nil {
					t.Error(err)
				}
				if err := os.Mkdir(f.repo, 0700); err != nil {
					t.Error(err)
				}
			}
		case "fault-retained":
			if consumed.CompareAndSwap(false, true) {
				fault <- entry
			}
		}
	}
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			workers.Add(1)
		}
		return nil
	}
	testOwnedClose = func(stage string, resource io.Closer, actual func() error) error {
		if stage != control.Stage {
			return actual()
		}
		if control.Setup == "workspace-overlap" {
			file, ok := resource.(*os.File)
			if !ok {
				return actual()
			}
			info, err := file.Stat()
			repo, repoErr := os.Stat(f.repo)
			if err != nil || repoErr != nil || !os.SameFile(info, repo) {
				return actual()
			}
		}
		if control.Setup == "cleanup-backend" {
			backend, ok := resource.(*s3OperationBackend)
			if !ok || !backend.cleanup {
				return actual()
			}
		}
		if !control.Published && control.Setup != "" && control.Setup != "recognized-residue" {
			entry := current.Load()
			if entry == nil {
				t.Error("rejection copy observer absent")
			} else {
				for _, buffer := range [][]byte{entry.request.password, entry.request.recoveryPassword} {
					for _, v := range buffer {
						if v != 0 {
							rejectionUnscrubbed.Store(true)
							break
						}
					}
				}
			}
		}
		failedResource = resource
		if attempts.Add(1) != 1 {
			t.Error("actual closer repeated")
			return errors.New("repeated synthetic closer")
		}
		if control.Mode == "panic" {
			panic("synthetic closer panic")
		}
		if control.Mode == "closed-error" {
			file, ok := resource.(*os.File)
			if !ok {
				t.Error("fd-reuse control did not get actual file")
				return errors.New("wrong resource")
			}
			number := int(file.Fd())
			if err := actual(); err != nil {
				t.Error("actual close before synthetic error failed", err)
				return err
			}
			duplicate, err := unix.FcntlInt(sentinelBase.Fd(), unix.F_DUPFD_CLOEXEC, number)
			if err != nil {
				t.Error(err)
				return errors.New("reuse failed")
			}
			sentinel = os.NewFile(uintptr(duplicate), "failed closer fd-reuse sentinel")
			if duplicate != number {
				t.Error("actual fd number was not reused", duplicate, number)
			}
		}
		return errors.New("synthetic owned close failure")
	}
	input := sourceCInput{repository: f.repo, source: f.source, workspace: f.workspace, state: f.state, password: f.password}
	if control.Remote {
		input.remote = &f.cfg
	}
	// Healthy twins positively calibrate all four public cohort families and the
	// actual copy/resource/worker observations before installing the fault.
	testOwnedCloseSaved := testOwnedClose
	testOwnedClose = nil
	for _, remote := range []bool{false, true} {
		// This fixture has exactly one real repository kind. Source/options twins
		// are calibrated here; the other kind is calibrated by its own child case.
		if remote != control.Remote {
			continue
		}
		for _, sourceEntry := range []bool{false, true} {
			id, status := sourceCVersionedStart(input, OpSnapshot, sourceEntry, int(held.Fd()), []byte(`[]`), 0)
			if status != StatusOk || id == 0 {
				t.Fatal("healthy public twin refused", status, id)
			}
			result, waited, freed := sourceCWaitAndFree(id)
			if result.status != StatusOk || waited != StatusOk || freed != StatusOk {
				t.Fatal("healthy public twin did not complete actual snapshot", result.status, result.diagnostic, waited, freed)
			}
		}
	}
	if copied.Load() == 0 || opens.Load() == 0 || workers.Load() == 0 {
		t.Fatal("production observers were not positively calibrated")
	}
	// Calibrate the actual retained-target door on the same owned fixture paths.
	for _, path := range []string{f.repo, f.workspace, f.state} {
		file, err := os.Open(path)
		if err != nil {
			t.Fatal(err)
		}
		target, status := admitRootedTarget(int(file.Fd()), path)
		if err := file.Close(); err != nil {
			t.Fatal(err)
		}
		if status != StatusOk || target == 0 {
			t.Fatal("healthy retained-target calibration failed", status, target)
		}
		if releaseRootedTarget(target) != StatusOk {
			t.Fatal("healthy target calibration did not release")
		}
		if err := os.Remove(filepath.Join(path, rootedRestoreLock)); err != nil && !os.IsNotExist(err) {
			t.Fatal(err)
		}
	}
	var originalState []byte
	setupArmed.Store(true)
	switch control.Setup {
	case "workspace-mode":
		if err := os.Chmod(f.workspace, 0755); err != nil {
			t.Fatal(err)
		}
	case "workspace-overlap":
		input.workspace = f.repo
	case "recognized-residue":
		name := filepath.Join(f.workspace, metadataDirectoryPrefix+strings.Repeat("a", 32))
		if err := os.Mkdir(name, 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink("synthetic retained symlink", filepath.Join(name, strings.Repeat("b", 32))); err != nil {
			t.Fatal(err)
		}
	case "invalid-state-census":
		if err := os.WriteFile(filepath.Join(f.state, "foreign-owned-fixture"), []byte("foreign synthetic state"), 0600); err != nil {
			t.Fatal(err)
		}
	case "invalid-state-record":
		originalState, err = os.ReadFile(filepath.Join(f.state, s3OwnerStateName))
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(f.state, s3OwnerStateName), []byte(`{"invalid":true}`), 0600); err != nil {
			t.Fatal(err)
		}
	}
	testOwnedClose = testOwnedCloseSaved
	if control.Operation == OpInit {
		input.recovery = f.recovery
	}
	descriptor := int(wrong.Fd())
	if control.Published {
		descriptor = int(held.Fd())
	}
	type started struct {
		id     uint64
		status int32
	}
	returned := make(chan started, 1)
	go func() {
		operation := control.Operation
		if operation == 0 {
			operation = OpSnapshot
		}
		var selection []byte
		if operation == OpSnapshot {
			selection = []byte(`[]`)
		}
		id, status := sourceCVersionedStart(input, operation, !control.Options, descriptor, selection, 0)
		returned <- started{id, status}
	}()
	var entry *ownedCall
	select {
	case entry = <-fault:
	case result := <-returned:
		if !control.Published {
			t.Fatal("rejected cleanup returned instead of retaining", result.status, result.id)
		}
		// A published entry normally returns before worker cleanup. Keep its actual id.
		returned <- result
		select {
		case entry = <-fault:
		case <-time.After(15 * time.Second):
			t.Fatal("actual closer fault was not consumed", control.Stage)
		}
	case <-time.After(15 * time.Second):
		t.Fatal("actual closer fault was not consumed", control.Stage)
	}
	if replaced.Load() {
		replacedPath := f.source
		if control.Setup == "replace-repo-before-adopt" {
			replacedPath = f.repo
		}
		if err := os.Rename(replacedPath, replacedPath+"-adopt-replacement"); err != nil {
			t.Fatal(err)
		}
		if err := os.Rename(replacedPath+"-adopt-original", replacedPath); err != nil {
			t.Fatal(err)
		}
	}
	if control.Setup == "workspace-mode" {
		if err := os.Chmod(f.workspace, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if control.Setup == "invalid-state-census" {
		if err := os.Remove(filepath.Join(f.state, "foreign-owned-fixture")); err != nil {
			t.Fatal(err)
		}
	}
	if control.Setup == "invalid-state-record" {
		if err := os.WriteFile(filepath.Join(f.state, s3OwnerStateName), originalState, 0600); err != nil {
			t.Fatal(err)
		}
	}
	if attempts.Load() != 1 || !entry.failed() {
		t.Fatal("fault did not carry actual closer/record")
	}
	if rejectionUnscrubbed.Load() {
		t.Fatal("known rejection closer began before mutable buffer scrub")
	}
	entry.mu.Lock()
	req, owner := entry.request, entry.owner
	retained := len(entry.resources)
	state := entry.resources[failedResource]
	exactFault := state != nil && state.started && state.err == errOwnedCleanup
	if state != nil {
		select {
		case <-state.done:
		default:
			exactFault = false
		}
	}
	entry.mu.Unlock()
	if !exactFault || failedResource == nil {
		t.Fatal("the actual failed closer identity/fault record was lost")
	}
	if control.Operation == OpInit && !recoveryCopied.Load() {
		t.Fatal("nonempty distinct password/recovery copies were not witnessed")
	}
	if req == nil {
		t.Fatal("actual retained request lost")
	}
	switch control.Stage {
	case "repository-root":
		if req.repoRoot != failedResource {
			t.Fatal("actual failed repository root field lost")
		}
	case "source-root":
		if req.sourceRoot != failedResource {
			t.Fatal("actual failed source root field lost")
		}
	case "metadata-root":
		if req.metadataRoot != failedResource {
			t.Fatal("actual failed metadata root field lost")
		}
	case "metadata-directory":
		if req.metadataFile != failedResource {
			t.Fatal("actual failed metadata directory field lost")
		}
	case "request-directory":
		matched := false
		for _, file := range req.pathFiles {
			matched = matched || file == failedResource
		}
		if !matched {
			t.Fatal("actual failed request directory field lost")
		}
	}

	if req == nil || retained == 0 {
		t.Fatal("partial request/resource ownership was lost")
	}
	if !control.Published {
		for _, buffer := range [][]byte{req.password, req.recoveryPassword} {
			for _, value := range buffer {
				if value != 0 {
					t.Fatal("rejection buffer retained secret bytes before blocked cleanup")
				}
			}
		}
		select {
		case result := <-returned:
			t.Fatal("blocked actual entry returned", result.status, result.id)
		default:
		}
	}
	if control.Remote {
		if owner == nil {
			t.Fatal("actual remote owner lost")
		}
		ownerUsable := make(chan bool, 1)
		go func() {
			owner.mu.Lock()
			matches := !owner.closed && owner.owned == entry
			switch control.Stage {
			case "s3-owner-state-file", "s3-state-displaced-file":
				matches = matches && owner.stateFile == failedResource
			case "s3-owner-root":
				matches = matches && owner.root == failedResource
			case "s3-owner-directory":
				matches = matches && owner.directory == failedResource
			}
			owner.mu.Unlock()
			ownerUsable <- matches
		}()
		select {
		case matches := <-ownerUsable:
			if !matches {
				t.Fatal("actual retained remote owner/failed field lost")
			}
		case <-time.After(5 * time.Second):
			t.Fatal("retained owner mutex remained locked after fault")
		}
		s3OwnersMu.Lock()
		registered := s3Owners[owner.destination] == owner
		s3OwnersMu.Unlock()
		if !registered {
			t.Fatal("actual remote owner namespace identity lost")
		}
		legacy, err := newS3Owner(f.state, f.cfg)
		if legacy != nil {
			_ = legacy.Close()
		}
		if legacy != nil || !errors.Is(err, errS3OwnerBusy) {
			t.Fatal("legacy same-namespace admission lost exclusion", err)
		}
	}
	rootedTargets.Lock()
	privateCount := len(rootedTargets.private)
	rootedTargets.Unlock()
	if control.Stage != "directory-open-ancestor" {
		if privateCount == 0 {
			t.Error("retained private registrations disappeared")
		}
		path := f.repo
		if strings.HasPrefix(control.Stage, "metadata-") && control.Setup != "workspace-mode" {
			path = input.workspace
		}
		if control.Remote {
			path = f.state
		}
		probe, err := os.Open(path)
		if err != nil {
			t.Fatal(err)
		}
		target, status := admitRootedTarget(int(probe.Fd()), path)
		if err := probe.Close(); err != nil {
			t.Fatal(err)
		}
		if status == StatusOk {
			if target != 0 {
				_ = releaseRootedTarget(target)
			}
			t.Fatal("real retained-target door accepted retained failed private authority")
		}
		if target != 0 && revalidateRootedTarget(target) == StatusOk {
			t.Fatal("refused target identity became usable")
		}
	}
	var id uint64
	if control.Published {
		select {
		case result := <-returned:
			if result.status != StatusOk || result.id == 0 {
				t.Fatal("actual worker was not published", result.status, result.id)
			}
			id = result.id
		case <-time.After(5 * time.Second):
			t.Fatal("published native entry did not return handle")
		}
		waitReturned := make(chan struct{})
		go func() { _, _, _ = sourceCWaitAndFree(id); close(waitReturned) }()
		if sourceCCancel(id) != StatusOk || sourceCFree(id) != StatusBusy {
			t.Fatal("cancel/free falsely released undrained native owner")
		}
		ownedAwaitActualBlocked(t, true)
		select {
		case <-waitReturned:
			t.Fatal("actual native wait returned despite retained fault")
		default:
		}
	} else {
		ownedAwaitActualBlocked(t, false)
	}
	// The fault sealed one cohort: all otherwise-valid entry families refuse
	// before any new credential copy, resource acquisition or worker starts.
	beforeCopied, beforeOpens, beforeWorkers := copied.Load(), opens.Load(), workers.Load()
	for _, remote := range []bool{false, true} {
		probe := input
		if remote {
			probe.remote = &f.cfg
		} else {
			probe.remote = nil
		}
		for _, sourceEntry := range []bool{false, true} {
			next, status := sourceCVersionedStart(probe, OpSnapshot, sourceEntry, int(held.Fd()), []byte(`[]`), 0)
			if next != 0 || status != StatusFailed {
				t.Fatal("sealed public entry admitted authority", remote, sourceEntry, status, next)
			}
		}
	}
	if copied.Load() != beforeCopied || opens.Load() != beforeOpens || workers.Load() != beforeWorkers || attempts.Load() != 1 {
		t.Fatal("seal refusal touched credentials/resources or repeated closer")
	}
	if sentinel != nil {
		if _, err := sentinel.Stat(); err != nil {
			t.Fatal("fallback closer reached reused descriptor", err)
		}
	}
	ownedCalls.Lock()
	_, kept := ownedCalls.live[entry]
	sealed := ownedCalls.sealed
	ownedCalls.Unlock()
	if !kept || !sealed {
		t.Fatal("actual failed ownership retired or seal reopened")
	}
	// Ending this disposable process is explicitly the fault-control boundary,
	// never a claim that its blocked native operation completed.
	if t.Failed() {
		os.Exit(1)
	}
	fmt.Println("OWNED_FAULT_CONTROL_PROVED " + raw)
	os.Exit(0)
}
