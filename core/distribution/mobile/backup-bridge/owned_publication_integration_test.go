//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestOwnedPublicSealPublicationOrdering(t *testing.T) {
	for _, ordering := range []string{"seal-wins", "publication-wins"} {
		t.Run(ordering, func(t *testing.T) {
			ownedRunControlChild(t, "TestOwnedPublicSealPublicationChild", "SOLSTONE_OWNED_ORDER_CHILD", ordering, "OWNED_ORDER_CONTROL_PROVED")
		})
	}
}
func TestOwnedPublicSealPublicationChild(t *testing.T) {
	ordering := os.Getenv("SOLSTONE_OWNED_ORDER_CHILD")
	if ordering == "" {
		t.Skip("disposable child only")
	}
	if ordering != "seal-wins" && ordering != "publication-wins" {
		t.Fatal("invalid private ordering")
	}
	first, second := newRootedTestFixture(t, false), newRootedTestFixture(t, false)
	first.source += "-retained"
	second.source += "-retained"
	held, err := os.Open(second.source)
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	mismatchPath := filepath.Join(first.base, "mismatch")
	if err := os.Mkdir(mismatchPath, 0700); err != nil {
		t.Fatal(err)
	}
	mismatch, err := os.Open(mismatchPath)
	if err != nil {
		t.Fatal(err)
	}
	defer mismatch.Close()
	repoIdentity, err := os.Stat(first.repo)
	if err != nil {
		t.Fatal(err)
	}
	publication, worker := make(chan struct{}), make(chan struct{})
	releasePublication, releaseWorker := make(chan struct{}), make(chan struct{})
	var pubOnce, workerOnce sync.Once
	releasePub := func() { pubOnce.Do(func() { close(releasePublication) }) }
	defer releasePub()
	releaseWork := func() { workerOnce.Do(func() { close(releaseWorker) }) }
	defer releaseWork()
	fault := make(chan *ownedCall, 1)
	var actualWorkers atomic.Int32
	var launches atomic.Int32
	testOwnedStage = func(stage string, entry *ownedCall) {
		if stage == "before-publication" && entry.request.repoPath == second.repo && ordering == "seal-wins" {
			close(publication)
			<-releasePublication
		}
		if stage == "worker-launch" && entry.request.repoPath == second.repo {
			launches.Add(1)
		}
		if stage == "fault-retained" {
			fault <- entry
		}
	}
	testGate = func(stage string, ctx context.Context) error {
		if stage == "before-restic" {
			actualWorkers.Add(1)
		}
		if stage == "before-restic" && ordering == "publication-wins" {
			close(worker)
			select {
			case <-releaseWorker:
			case <-ctx.Done():
				return ctx.Err()
			}
		}
		return nil
	}
	testOwnedClose = func(stage string, resource io.Closer, actual func() error) error {
		if stage == "repository-root" {
			root, ok := resource.(*os.Root)
			if ok {
				info, err := root.Stat(".")
				if err == nil && os.SameFile(info, repoIdentity) {
					return errors.New("synthetic first-owner root-close failure")
				}
			}
		}
		return actual()
	}
	type started struct {
		id     uint64
		status int32
	}
	secondResult := make(chan started, 1)
	go func() {
		id, status := sourceCStart(sourceCInput{repository: second.repo, source: second.source, workspace: second.workspace, password: second.password}, int(held.Fd()), []byte(`[]`), 0)
		secondResult <- started{id, status}
	}()
	var active started
	if ordering == "seal-wins" {
		select {
		case <-publication:
		case <-time.After(5 * time.Second):
			t.Fatal("valid second entry did not reach publication boundary")
		}
	} else {
		select {
		case active = <-secondResult:
		case <-time.After(5 * time.Second):
			t.Fatal("valid second native entry did not publish")
		}
		if active.status != StatusOk || active.id == 0 {
			t.Fatal("publication twin did not admit actual operation", active.status, active.id)
		}
		select {
		case <-worker:
		case <-time.After(5 * time.Second):
			t.Fatal("actual published worker did not enter held work")
		}
	}
	firstReturned := make(chan struct{})
	go func() {
		_, _ = sourceCStart(sourceCInput{repository: first.repo, source: first.source, workspace: first.workspace, password: first.password}, int(mismatch.Fd()), []byte(`[]`), 0)
		close(firstReturned)
	}()
	var retained *ownedCall
	select {
	case retained = <-fault:
	case <-time.After(5 * time.Second):
		t.Fatal("first owner's actual close fault did not seal")
	}
	ownedAwaitActualBlocked(t, false)
	select {
	case <-firstReturned:
		t.Fatal("actual fault entry returned")
	default:
	}
	if !retained.failed() {
		t.Fatal("first owner was not retained")
	}
	if ordering == "seal-wins" {
		releasePub()
		select {
		case refused := <-secondResult:
			if refused.status != StatusFailed || refused.id != 0 {
				t.Fatal("worker published after seal won", refused.status, refused.id)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("second valid entry did not finish confirmed rejection")
		}
		handlesMu.Lock()
		remainingHandles := len(handles)
		handlesMu.Unlock()
		if actualWorkers.Load() != 0 || launches.Load() != 0 || remainingHandles != 0 {
			t.Fatal("seal winner still started/published second work", actualWorkers.Load(), launches.Load(), remainingHandles)
		}
	} else {
		handlesMu.Lock()
		h := handles[active.id]
		handlesMu.Unlock()
		if actualWorkers.Load() != 1 || launches.Load() != 1 || h == nil || h.owned == nil || h.owned.failed() || sourceCFree(active.id) != StatusBusy {
			t.Fatal("seal discarded or rewrote previously published actual operation")
		}
		releaseWork()
		result, waited, freed := sourceCWaitAndFree(active.id)
		if result.status != StatusOk || waited != StatusOk || freed != StatusOk || !validS3ID(result.snapshotID) {
			t.Fatal("published-before-seal operation did not complete actual snapshot", result.status, result.diagnostic, waited, freed)
		}
	}
	ownedCalls.Lock()
	_, kept := ownedCalls.live[retained]
	sealed := ownedCalls.sealed
	count := len(ownedCalls.live)
	ownedCalls.Unlock()
	if !kept || !sealed || count != 1 {
		t.Fatal("ordered ownership retired fault or retained healthy finished operation", kept, sealed, count)
	}
	if t.Failed() {
		os.Exit(1)
	}
	fmt.Println("OWNED_ORDER_CONTROL_PROVED " + ordering)
	os.Exit(0)
}
