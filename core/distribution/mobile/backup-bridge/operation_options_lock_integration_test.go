//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"sync"
	"testing"
	"time"

	"github.com/restic/restic/internal/backend"
	"github.com/restic/restic/internal/repository"
)

type optionsLockObservation struct {
	backend.Backend
	mu         sync.Mutex
	lists      int
	beforeList func(context.Context, int) error
}

func (be *optionsLockObservation) List(ctx context.Context, kind backend.FileType, consume func(backend.FileInfo) error) error {
	if kind == backend.LockFile {
		be.mu.Lock()
		be.lists++
		n := be.lists
		be.mu.Unlock()
		if be.beforeList != nil {
			if err := be.beforeList(ctx, n); err != nil {
				return err
			}
		}
	}
	return be.Backend.List(ctx, kind, consume)
}

func optionsLockRepository(t *testing.T, req bridgeRequest, before func(context.Context, int) error) *repository.Repository {
	t.Helper()
	if err := pinRequestPaths(&req); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = closeRequestPaths(&req) })
	be, err := operationBackend(context.Background(), req, false)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = be.Close() })
	repo, err := repository.New(guardedBackend{&optionsLockObservation{Backend: be, beforeList: before}}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := safeScanKey(context.Background(), repo, string(req.password)); err != nil {
		t.Fatal(err)
	}
	return repo
}

func TestOperationOptionsActualLockRetryBudgetAndCancellation(t *testing.T) {
	req := newOptionsCheckFixture(t)
	owner := optionsLockRepository(t, req, nil)
	hold := func() func() {
		unlock, _, err := lockRepo(context.Background(), owner, true)
		if err != nil {
			t.Fatal("independent repository owner could not acquire", err)
		}
		t.Cleanup(unlock)
		return unlock
	}
	// A real conflicting owner remains held throughout the complete SDK budget,
	// including the final attempt. Backend enumeration proves the actual retry.
	t.Run("zero-and-held-budget", func(t *testing.T) {
		unlock := hold()
		defer unlock()
		var mu sync.Mutex
		lists := 0
		repo := optionsLockRepository(t, req, func(_ context.Context, _ int) error { mu.Lock(); lists++; mu.Unlock(); return nil })
		if release, _, err := lockRepo(context.Background(), repo, false); err == nil {
			release()
			t.Fatal("zero-retry accepted actual conflicting lock")
		}
		mu.Lock()
		lists = 0
		mu.Unlock()
		budget := 80 * time.Millisecond
		began := time.Now()
		if release, _, err := lockRepoWithRetry(context.Background(), repo, false, budget); err == nil {
			release()
			t.Fatal("held lock incorrectly acquired")
		}
		mu.Lock()
		observed := lists
		mu.Unlock()
		if time.Since(began) < budget || observed < 2 {
			t.Fatal("retry budget or actual final attempt omitted", observed)
		}
	})
	t.Run("release-at-final-backend-attempt", func(t *testing.T) {
		unlock := hold()
		defer unlock()
		entered, proceed := make(chan struct{}), make(chan struct{})
		repo := optionsLockRepository(t, req, func(ctx context.Context, n int) error {
			if n == 2 {
				close(entered)
				select {
				case <-proceed:
				case <-ctx.Done():
					return ctx.Err()
				}
			}
			return nil
		})
		budget := 80 * time.Millisecond
		type outcome struct {
			release  func()
			acquired context.Context
			err      error
		}
		done := make(chan outcome, 1)
		began := time.Now()
		go func() {
			release, acquired, err := lockRepoWithRetry(context.Background(), repo, false, budget)
			done <- outcome{release, acquired, err}
		}()
		select {
		case <-entered:
		case <-time.After(5 * time.Second):
			t.Fatal("final actual backend attempt did not start")
		}
		if time.Since(began) < budget {
			t.Error("second backend attempt preceded retry budget")
		}
		select {
		case result := <-done:
			if result.release != nil {
				result.release()
			}
			t.Fatal("blocked final backend attempt returned early")
		default:
		}
		unlock() // only now can the actual final backend enumeration see no owner
		close(proceed)
		var result outcome
		select {
		case result = <-done:
		case <-time.After(5 * time.Second):
			t.Fatal("final retry did not actually finish")
		}
		if result.err != nil || result.release == nil || result.acquired.Err() != nil {
			t.Fatal("SDK final-attempt semantics changed", result.err)
		}
		defer result.release()
		// Work begun after the retry budget remains governed by the acquired and
		// parent contexts. A retry timer must not cancel an already-acquired lock.
		if _, err := repo.LoadRaw(result.acquired, backend.ConfigFile, [32]byte{}); err != nil {
			t.Fatal("actual post-budget work was cancelled", err)
		}
		if result.acquired.Err() != nil {
			t.Fatal("acquired context was converted into an acquisition deadline")
		}
		result.release()
		if result.acquired.Err() == nil {
			t.Fatal("actual unlock did not cancel and join refresher")
		}
	})
	t.Run("parent-cancel-during-held-retry", func(t *testing.T) {
		unlock := hold()
		defer unlock()
		entered := make(chan struct{})
		repo := optionsLockRepository(t, req, func(_ context.Context, n int) error {
			if n == 1 {
				close(entered)
			}
			return nil
		})
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		done := make(chan error, 1)
		go func() {
			release, _, err := lockRepoWithRetry(ctx, repo, false, time.Minute)
			if release != nil {
				release()
			}
			done <- err
		}()
		select {
		case <-entered:
		case <-time.After(5 * time.Second):
			t.Fatal("actual lock enumeration did not begin")
		}
		cancel()
		select {
		case err := <-done:
			if !errors.Is(err, context.Canceled) {
				t.Fatal("actual parent cancellation lost", err)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("cancelled retry did not finish")
		}
		unlock()
		fresh := optionsLockRepository(t, req, nil)
		release, _, err := lockRepoWithRetry(context.Background(), fresh, false, 0)
		if err != nil {
			t.Fatal("fresh acquisition after actual drain failed", err)
		}
		release()
	})
}
