//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func teardownCWait(t *testing.T, id uint64) (bridgeResult, []byte) {
	t.Helper()
	handlesMu.Lock()
	handle := handles[id]
	handlesMu.Unlock()
	if handle == nil {
		t.Fatal("actual native handle absent")
	}
	t.Cleanup(func() { sourceCCancel(id); sourceCWaitAndFree(id) })
	select {
	case <-handle.doneCh:
	case <-time.After(60 * time.Second):
		t.Fatal("actual teardown worker did not drain")
	}
	metadata, metadataStatus := sourceCMetadata(id)
	result, waited, freed := sourceCWaitAndFree(id)
	if waited != StatusOk || freed != StatusOk {
		t.Fatal("native wait/free refused", waited, freed)
	}
	if result.status == StatusOk && metadataStatus != StatusOk {
		t.Fatal("native completed metadata unavailable", metadataStatus)
	}
	return result, metadata
}

func teardownCInventory(t *testing.T, f *directoryFixture) (string, []string, s3TeardownAdmission) {
	t.Helper()
	id, status := f.start(OpOpen)
	if status != StatusOk || id == 0 {
		t.Fatal("open refused", status)
	}
	result, metadata := teardownCWait(t, id)
	if result.status != StatusOk {
		t.Fatal("open failed", result.status)
	}
	var opened struct {
		Repository struct {
			ID string `json:"id"`
		} `json:"repository"`
	}
	if json.Unmarshal(metadata, &opened) != nil || !lowerHexID(opened.Repository.ID) {
		t.Fatal("actual repository ID absent")
	}
	id, status = f.start(OpListSnapshots)
	if status != StatusOk || id == 0 {
		t.Fatal("catalogue refused", status)
	}
	result, metadata = teardownCWait(t, id)
	if result.status != StatusOk {
		t.Fatal("catalogue failed", result.status)
	}
	var catalogue struct {
		Snapshots []struct {
			ID string `json:"id"`
		} `json:"snapshots"`
	}
	if json.Unmarshal(metadata, &catalogue) != nil {
		t.Fatal("actual catalogue absent")
	}
	ids := []string{}
	for _, row := range catalogue.Snapshots {
		ids = append(ids, row.ID)
	}
	destination, _ := s3DestinationFor(*f.input.remote)
	raw, _ := json.Marshal(destination)
	fd, observed, status := teardownCObserve(int(f.state.Fd()), raw, nil)
	if status != StatusOk || fd < 0 {
		t.Fatal("actual observation refused", status, fd)
	}
	if err := unix.Close(fd); err != nil {
		t.Fatal(err)
	}
	var observation s3TeardownObservation
	if json.Unmarshal(observed, &observation) != nil || !observation.Present || observation.Active {
		t.Fatal("invalid idle observation")
	}
	return opened.Repository.ID, ids, s3TeardownAdmission{ConfigSHA: "sha256:" + strings.Repeat("a", 64), RepositoryID: opened.Repository.ID, Origin: observation.ID, Expected: observation}
}

func TestTeardownPublicCObservationAndAdmissionRefusals(t *testing.T) {
	f := newDirectoryFixture(t, true)
	f.call(t, OpInit)
	f.call(t, OpSnapshot)
	_, ids, admission := teardownCInventory(t, f)
	destination, _ := s3DestinationFor(*f.input.remote)
	raw, _ := json.Marshal(destination)
	before, err := os.ReadFile(filepath.Join(f.input.state, s3OwnerStateName))
	if err != nil {
		t.Fatal(err)
	}
	for _, bad := range []struct {
		fd                   int
		destination, pending []byte
	}{
		{-1, raw, nil}, {int(f.source.Fd()), raw, nil}, {int(f.state.Fd()), []byte(`{"unknown":true}`), nil}, {int(f.state.Fd()), raw, []byte("bad")},
	} {
		fd, _, status := teardownCObserve(bad.fd, bad.destination, bad.pending)
		if status == StatusOk || fd != -1 {
			if fd >= 0 {
				unix.Close(fd)
			}
			t.Fatal("malformed observation retained output", status, fd)
		}
	}
	held, observed, status := teardownCObserve(int(f.state.Fd()), raw, nil)
	if status != StatusOk || held < 0 {
		t.Fatal("positive observation failed", status)
	}
	if flags, err := unix.FcntlInt(uintptr(held), unix.F_GETFD, 0); err != nil || flags&unix.FD_CLOEXEC == 0 {
		t.Fatal("transferred descriptor not close-on-exec", err)
	}
	other, _, status := teardownCObserve(int(f.state.Fd()), raw, nil)
	if status != StatusBusy || other != -1 {
		if other >= 0 {
			unix.Close(other)
		}
		t.Fatal("transferred descriptor lost exclusion", status)
	}
	var observation s3TeardownObservation
	json.Unmarshal(observed, &observation)
	wrong := observation
	wrong.ID = strings.Repeat("f", 32)
	wrongRaw, _ := json.Marshal(wrong)
	if teardownCRetire(held, raw, wrongRaw) == StatusOk {
		t.Fatal("wrong record retired")
	}
	if err := unix.Close(held); err != nil {
		t.Fatal(err)
	}
	reacquired, _, status := teardownCObserve(int(f.state.Fd()), raw, nil)
	if status != StatusOk {
		t.Fatal("transferred close did not release exclusion", status)
	}
	unix.Close(reacquired)
	workers, requests := f.workers.Load(), f.requests.Load()
	good, _ := json.Marshal(admission)
	drifted := admission
	drifted.Expected.ID = strings.Repeat("f", 32)
	drifted.Origin = drifted.Expected.ID
	drift, _ := json.Marshal(drifted)
	for _, bad := range []struct {
		binding  []byte
		reserved uint32
	}{{[]byte(`{}`), 0}, {drift, 0}, {good, 1}} {
		id, status := teardownCStart(f.input, int(f.state.Fd()), ids, bad.binding, bad.reserved)
		if id != 0 {
			sourceCCancel(id)
			sourceCWaitAndFree(id)
		}
		if status == StatusOk || id != 0 || f.workers.Load() != workers || f.requests.Load() != requests {
			t.Fatal("invalid typed admission reached native work", status, id)
		}
		after, err := os.ReadFile(filepath.Join(f.input.state, s3OwnerStateName))
		if err != nil || !bytes.Equal(before, after) {
			t.Fatal("refusal changed native custody", err)
		}
	}
}

func TestTeardownPublicCPendingPruneRetryAndExactRetirement(t *testing.T) {
	f := newDirectoryFixture(t, true)
	f.input.recovery = []byte("teardown-synthetic-recovery")
	f.call(t, OpInit)
	f.call(t, OpAddRecoveryKey)
	f.call(t, OpSnapshot)
	_, ids, admission := teardownCInventory(t, f)
	if len(ids) == 0 {
		t.Fatal("populated positive absent")
	}
	destination, _ := s3DestinationFor(*f.input.remote)
	destinationRaw, _ := json.Marshal(destination)
	scope, err := admission.scope(destination, ids)
	if err != nil {
		t.Fatal(err)
	}
	retained := map[string][]byte{}
	f.peer.mu.Lock()
	for key, value := range f.peer.objects {
		if strings.Contains(key, "/keys/") || key == f.peer.cfg.prefix+"/config" {
			retained[key] = append([]byte(nil), value...)
		}
	}
	f.peer.mu.Unlock()
	if len(retained) < 3 {
		t.Fatal("config and initial keys absent")
	}
	prior := testOwnedStage
	var dropped atomic.Bool
	testOwnedStage = func(stage string, entry *ownedCall) {
		prior(stage, entry)
		if stage == "before-publication" && entry.request.operation == OpTeardown {
			entry.request.remote.factory = func(ctx context.Context, cfg s3Config) (*s3Backend, error) {
				owned, cancel := context.WithCancel(ctx)
				transport := s3TestTransport(func(request *http.Request) (*http.Response, error) {
					response, err := f.peer.RoundTrip(request)
					if err == nil && request.Method == http.MethodDelete && strings.Contains(request.URL.Path, "/data/") && dropped.CompareAndSwap(false, true) {
						if response.Body != nil {
							response.Body.Close()
						}
						return nil, errS3IO
					}
					return response, err
				})
				return &s3Backend{cfg: cfg, ctx: owned, cancel: cancel, transport: transport}, nil
			}
		}
	}
	binding, _ := json.Marshal(admission)
	id, status := teardownCStart(f.input, int(f.state.Fd()), ids, binding, 0)
	if status != StatusOk || id == 0 {
		t.Fatal("typed populated teardown refused", status)
	}
	failed, _ := teardownCWait(t, id)
	if !dropped.Load() || failed.status != StatusRemotePending {
		t.Fatal("actual lost pack-delete acknowledgement did not retain pending", dropped.Load(), failed.status, failed.diagnostic)
	}
	if fd, _, status := teardownCObserve(int(f.state.Fd()), destinationRaw, nil); status == StatusOk || fd != -1 {
		if fd >= 0 {
			unix.Close(fd)
		}
		t.Fatal("idle observation adopted pending")
	}
	held, pending, status := teardownCObserve(int(f.state.Fd()), destinationRaw, []byte(scope))
	if status != StatusOk || held < 0 {
		t.Fatal("matching pending observation refused", status)
	}
	if err := unix.Close(held); err != nil {
		t.Fatal(err)
	}
	if json.Unmarshal(pending, &admission.Expected) != nil || !admission.Expected.Active {
		t.Fatal("actual pending intent absent")
	}
	for _, drift := range []string{"config", "repository", "cohort"} {
		changed := admission
		cohort := ids
		if drift == "config" {
			changed.ConfigSHA = "sha256:" + strings.Repeat("b", 64)
		}
		if drift == "repository" {
			changed.RepositoryID = strings.Repeat("b", 64)
		}
		if drift == "cohort" {
			cohort = []string{strings.Repeat("b", 64)}
		}
		raw, _ := json.Marshal(changed)
		id, status := teardownCStart(f.input, int(f.state.Fd()), cohort, raw, 0)
		if id != 0 {
			sourceCCancel(id)
			sourceCWaitAndFree(id)
		}
		if status == StatusOk || id != 0 {
			t.Fatal("changed pending scope admitted", drift, status)
		}
	}
	testOwnedStage = prior
	binding, _ = json.Marshal(admission)
	id, status = teardownCStart(f.input, int(f.state.Fd()), ids, binding, 0)
	if status != StatusOk || id == 0 {
		t.Fatal("matching retry entry refused", status)
	}
	completed, metadata := teardownCWait(t, id)
	if completed.status != StatusOk || completed.snapshotCount != 0 {
		t.Fatal("matching retry did not reconcile required prune", completed.status, completed.diagnostic)
	}
	var proof struct {
		CompletionID string `json:"completion_id"`
		Device       uint64 `json:"state_device"`
		Inode        uint64 `json:"state_inode"`
		Scope        string `json:"scope"`
		Origin       string `json:"origin"`
	}
	if json.Unmarshal(metadata, &proof) != nil || proof.Scope != scope || proof.Origin != admission.Origin {
		t.Fatal("actual native completion metadata absent")
	}
	f.peer.mu.Lock()
	for key := range f.peer.objects {
		if strings.Contains(key, "/snapshots/") || strings.Contains(key, "/data/") || strings.Contains(key, "/index/") {
			f.peer.mu.Unlock()
			t.Fatal("current backup object retained", key)
		}
	}
	for key, value := range retained {
		if !bytes.Equal(value, f.peer.objects[key]) {
			f.peer.mu.Unlock()
			t.Fatal("retained config/key changed", key)
		}
	}
	f.peer.mu.Unlock()
	held, observed, status := teardownCObserve(int(f.state.Fd()), destinationRaw, []byte(scope))
	if status != StatusOk || held < 0 {
		t.Fatal("completion lock refused", status)
	}
	defer unix.Close(held)
	var actual s3TeardownObservation
	if json.Unmarshal(observed, &actual) != nil || actual.Active || !actual.Reconciled || actual.ID != proof.CompletionID || actual.Device != proof.Device || actual.Inode != proof.Inode {
		t.Fatal("completion file identity changed")
	}
	if status := teardownCVerify(held, destinationRaw, observed); status != StatusOk {
		t.Fatal("actual locked completion verification refused", status)
	}
	statePath := filepath.Join(f.input.state, s3OwnerStateName)
	oldPath := filepath.Join(t.TempDir(), "native-original")
	bytesBefore, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatal(err)
	}
	if os.Rename(statePath, oldPath) != nil || os.WriteFile(statePath, bytesBefore, 0600) != nil {
		t.Fatal("replacement fixture failed")
	}
	if status := teardownCVerify(held, destinationRaw, observed); status == StatusOk {
		t.Fatal("same-byte replacement passed destructive boundary guard")
	}
	if status := teardownCRetire(held, destinationRaw, observed); status == StatusOk {
		t.Fatal("replacement was retired")
	}
	if os.Remove(statePath) != nil || os.Rename(oldPath, statePath) != nil {
		t.Fatal("original restoration fixture failed")
	}
	if status := teardownCRetire(held, destinationRaw, observed); status != StatusOk {
		t.Fatal("exact completed custody did not retire", status)
	}
	if _, err := os.Lstat(filepath.Join(f.input.state, s3OwnerStateName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("native credential record remains", err)
	}
	if status := teardownCRetire(held, destinationRaw, observed); status == StatusOk {
		t.Fatal("absence manufactured original retirement")
	}
}

// A real typed request can publish a completed no-write failure. Its opcode
// and matching scope alone must never permit removing credential custody.
func TestTeardownPublicCFailedNoWriteCompletionCannotRetire(t *testing.T) {
	f := newDirectoryFixture(t, true)
	f.call(t, OpInit)
	f.call(t, OpSnapshot)
	_, ids, admission := teardownCInventory(t, f)
	destination, _ := s3DestinationFor(*f.input.remote)
	raw, _ := json.Marshal(destination)
	scope, err := admission.scope(destination, ids)
	if err != nil {
		t.Fatal(err)
	}
	f.peer.mu.Lock()
	before := make(map[string][]byte, len(f.peer.objects))
	for key, value := range f.peer.objects {
		before[key] = append([]byte(nil), value...)
	}
	f.peer.mu.Unlock()
	password := f.input.password
	f.input.password = []byte("incorrect-synthetic-teardown-password")
	binding, _ := json.Marshal(admission)
	id, status := teardownCStart(f.input, int(f.state.Fd()), ids, binding, 0)
	if status != StatusOk || id == 0 {
		t.Fatal("actual failed request was not admitted", status)
	}
	failed, _ := teardownCWait(t, id)
	f.input.password = password
	if failed.status == StatusOk {
		t.Fatal("incorrect password unexpectedly succeeded")
	}
	held, observed, status := teardownCObserve(int(f.state.Fd()), raw, []byte(scope))
	if status != StatusOk || held < 0 {
		t.Fatal("actual failed completion unavailable", status)
	}
	var actual s3TeardownObservation
	if json.Unmarshal(observed, &actual) != nil || actual.Active || actual.Operation != OpTeardown || actual.Reconciled {
		unix.Close(held)
		t.Fatal("failure was not represented distinctly")
	}
	statePath := filepath.Join(f.input.state, s3OwnerStateName)
	stateBefore, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatal(err)
	}
	if teardownCRetire(held, raw, observed) == StatusOk {
		t.Fatal("failed no-write completion retired")
	}
	stateAfter, err := os.ReadFile(statePath)
	if err != nil || !bytes.Equal(stateBefore, stateAfter) {
		t.Fatal("refusal changed native custody", err)
	}
	unix.Close(held)
	f.peer.mu.Lock()
	if len(before) != len(f.peer.objects) {
		t.Fatal("failed no-write request changed remote population")
	}
	for key, value := range before {
		if !bytes.Equal(value, f.peer.objects[key]) {
			t.Fatal("failed no-write request changed remote bytes", key)
		}
	}
	f.peer.mu.Unlock()
	// The same real native path remains retryable with the actual password.
	admission.Expected = actual
	binding, _ = json.Marshal(admission)
	id, status = teardownCStart(f.input, int(f.state.Fd()), ids, binding, 0)
	if status != StatusOk || id == 0 {
		t.Fatal("matching fresh retry refused", status)
	}
	completed, _ := teardownCWait(t, id)
	if completed.status != StatusOk || completed.snapshotCount != 0 {
		t.Fatal("actual retry failed", completed.status)
	}
}
