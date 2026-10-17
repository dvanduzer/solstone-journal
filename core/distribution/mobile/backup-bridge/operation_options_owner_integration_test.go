//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/restic/restic/internal/repository"
)

func TestOperationOptionsCancelledLockAttemptOwnsTransportUntilActualReturn(t *testing.T) {
	f := newRootedTestFixture(t, true)
	be := s3TestBackend(t, f.cfg, f.peer)
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	defer be.Close()
	if _, err := safeScanKey(context.Background(), repo, string(f.password)); err != nil {
		t.Fatal(err)
	}
	unlock, _, err := lockRepo(context.Background(), repo, true)
	if err != nil {
		t.Fatal("independent actual lock owner", err)
	}
	defer unlock()
	entered, cancelled, release := make(chan struct{}), make(chan struct{}), make(chan struct{})
	var enteredOnce, cancelledOnce, releaseOnce sync.Once
	transport := s3TestTransport(func(request *http.Request) (*http.Response, error) {
		if request.Method == http.MethodGet && strings.HasSuffix(request.URL.Query().Get("prefix"), "/locks/") {
			reply, err := f.peer.RoundTrip(request)
			if err != nil {
				return nil, err
			}
			_ = reply.Body.Close()
			enteredOnce.Do(func() { close(entered) })
			select {
			case <-request.Context().Done():
				cancelledOnce.Do(func() { close(cancelled) })
			case <-release:
				return nil, errS3IO
			}
			<-release // actual transport still owns the call after cancellation
			return nil, request.Context().Err()
		}
		return f.peer.RoundTrip(request)
	})
	id, handle := s3TestStartOperation(t, f.cfg, f.state, f.workspace, transport,
		bridgeRequest{operation: OpCheck, password: append([]byte(nil), f.password...), executionOptions: &operationOptions{retryLock: time.Minute, checkMode: checkReadAll}})
	defer func() { releaseOnce.Do(func() { close(release) }); awaitS3Test(t, handle.doneCh) }()
	awaitS3Test(t, entered)
	if cancelCall(id) != StatusOk || freeCall(id) != StatusBusy {
		t.Fatal("active native owner freed before drain")
	}
	awaitS3Test(t, cancelled)
	assertS3Pending(t, handle.doneCh)
	changed := f.cfg
	changed.secret = "replacement-synthetic-secret"
	if replacement, err := newS3Owner(s3OwnerTestDirectory(t), changed); replacement != nil || !errors.Is(err, errS3OwnerBusy) {
		if replacement != nil {
			_ = replacement.Close()
		}
		t.Fatal("credentials/new state root bypassed live cancelled transport", err)
	}
	releaseOnce.Do(func() { close(release) })
	result := s3TestWaitOperation(t, id, handle)
	if result.status == StatusOk {
		t.Fatal("cancelled lock attempt published success")
	}
	unlock()
	id, handle = s3TestStartOperation(t, f.cfg, f.state, f.workspace, f.peer,
		bridgeRequest{operation: OpCheck, password: append([]byte(nil), f.password...), executionOptions: &operationOptions{checkMode: checkReadAll}})
	if result := s3TestWaitOperation(t, id, handle); result.status != StatusOk {
		t.Fatal("same-process retry after actual transport/lock drain failed", result.status, result.diagnostic)
	}
}

func TestOperationOptionsPendingAndUncertainOwnerOutcomesArePreserved(t *testing.T) {
	for _, uncertain := range []bool{false, true} {
		t.Run(map[bool]string{false: "pending-owned-lock", true: "uncertain-completion-sync"}[uncertain], func(t *testing.T) {
			f := newRootedTestFixture(t, true)
			owner, err := newS3Owner(f.state, f.cfg)
			if err != nil {
				t.Fatal(err)
			}
			if uncertain {
				owner.fault = func(stage string) error {
					if stage == "directory-sync" && owner.record.Complete != nil {
						return errors.New("controlled completion durability refusal")
					}
					return nil
				}
			}
			var requests atomic.Int64
			transport := s3TestTransport(func(request *http.Request) (*http.Response, error) {
				requests.Add(1)
				if !uncertain && request.Method == http.MethodDelete {
					return nil, errS3IO
				}
				return f.peer.RoundTrip(request)
			})
			id, handle := s3TestStartOwnedOperation(t, f.cfg, owner, f.workspace, transport,
				bridgeRequest{operation: OpCheck, password: append([]byte(nil), f.password...), executionOptions: &operationOptions{retryLock: time.Second, checkMode: checkReadGroup, group: 1, groups: 52}})
			result := s3TestWaitOperation(t, id, handle)
			want := StatusRemotePending
			if uncertain {
				want = StatusCommittedUncertain
			}
			if result.status != want {
				t.Fatal("native options masked actual owner outcome", result.status, want)
			}
			reopened := s3OwnerTestOpen(t, f.state, f.cfg)
			if uncertain {
				if reopened.record.Complete == nil || reopened.record.Active != nil {
					t.Fatal("visible uncertain completion was discarded")
				}
				return
			}
			if reopened.record.Active == nil || !reopened.record.Active.MayHaveWritten || len(reopened.record.Active.Locks) == 0 {
				t.Fatal("pending lock custody was lost")
			}
			original := append([]string(nil), reopened.record.Active.Locks...)
			s3OwnerTestClose(t, reopened)
			changed := f.cfg
			changed.secret = "changed-synthetic-secret"
			newSource := filepath.Join(f.base, "new-journal-root")
			if err := os.Mkdir(newSource, 0700); err != nil {
				t.Fatal(err)
			}
			for _, operation := range []int32{OpCheck, OpSnapshot} {
				options := &operationOptions{retryLock: time.Minute}
				request := bridgeRequest{operation: operation, password: append([]byte(nil), f.password...), executionOptions: options}
				if operation == OpCheck {
					options.checkMode = checkReadAll
				} else {
					request.sourcePath = newSource
				}
				id, handle := s3TestStartOperation(t, changed, f.state, f.workspace, transport, request)
				// The later attempt adopts the pending lock custody. While the lock
				// still cannot be removed it stays pending and keeps that lock.
				if result := s3TestWaitOperation(t, id, handle); result.status != StatusRemotePending {
					t.Fatal("retry/changed credentials/journal root completed over an unremoved pending lock", result.status)
				}
				still := s3OwnerTestOpen(t, f.state, f.cfg)
				kept := still.record.Active != nil && still.record.Active.MayHaveWritten
				for _, id := range original {
					found := false
					for _, retained := range still.record.Active.Locks {
						found = found || retained == id
					}
					kept = kept && found
				}
				s3OwnerTestClose(t, still)
				if !kept {
					t.Fatal("adopting attempt dropped the pending lock custody")
				}
			}
		})
	}
}
