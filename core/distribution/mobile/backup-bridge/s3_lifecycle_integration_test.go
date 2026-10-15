//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"io"
	"net/http"
	"sync"
	"testing"
	"time"

	"github.com/restic/restic/internal/backend"
)

type s3ReaderFunc func([]byte) (int, error)

func (reader s3ReaderFunc) Read(out []byte) (int, error) { return reader(out) }

func awaitS3Test(t *testing.T, signal <-chan struct{}) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(5 * time.Second):
		t.Fatal("controlled worker did not reach its witness")
	}
}

func assertS3Pending(t *testing.T, finished <-chan struct{}) {
	t.Helper()
	select {
	case <-finished:
		t.Fatal("operation returned while its admitted work was blocked")
	case <-time.After(30 * time.Millisecond):
	}
}

func TestS3CloseRetainsNoncooperativeSourceReader(t *testing.T) {
	entered, release, stopped, closed := make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{})
	var releaseOnce sync.Once
	closeStarted := false
	be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(*http.Request) (*http.Response, error) {
		t.Error("cancelled source reached egress")
		return nil, errS3IO
	}))
	reader := &s3TestReader{s3ReaderFunc(func(out []byte) (int, error) { close(entered); <-release; out[0] = 'x'; return 1, io.EOF }), 1}
	var operationErr error
	go func() {
		defer close(stopped)
		operationErr = be.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, reader)
	}()
	defer func() {
		releaseOnce.Do(func() { close(release) })
		awaitS3Test(t, stopped)
		if closeStarted {
			awaitS3Test(t, closed)
		}
	}()
	awaitS3Test(t, entered)
	closeStarted = true
	go func() { _ = be.Close(); close(closed) }()
	awaitS3Test(t, be.ctx.Done())
	assertS3Pending(t, closed)
	if err := be.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(2)}, backend.NewByteReader([]byte("x"), nil)); !errors.Is(err, errS3Closed) {
		t.Fatal("replacement entered sealed backend")
	}
	releaseOnce.Do(func() { close(release) })
	awaitS3Test(t, stopped)
	awaitS3Test(t, closed)
	if !errors.Is(operationErr, context.Canceled) {
		t.Fatal("source cancellation was lost")
	}
}

func TestS3MethodContextsReachActualRequestsAndCloseJoins(t *testing.T) {
	handle := backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}
	for _, method := range []string{"save", "load", "list", "stat-clarification"} {
		for _, stopKind := range []string{"caller-cancel", "backend-close"} {
			t.Run(method+"/"+stopKind, func(t *testing.T) {
				entered, cancelled, release, stopped, closed := make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{})
				var releaseOnce sync.Once
				closeStarted := false
				cfg := s3TestConfig()
				headPath := ""
				be := s3TestBackend(t, cfg, s3TestTransport(func(req *http.Request) (*http.Response, error) {
					if method == "stat-clarification" && req.Method == http.MethodHead {
						headPath = req.URL.String()
						return s3TestResponse(404, "", http.Header{"Content-Length": {"137"}}), nil
					}
					if method == "stat-clarification" && (req.Method != http.MethodGet || req.URL.String() != headPath) {
						t.Error("Stat clarification changed object")
					}
					close(entered)
					select {
					case <-req.Context().Done():
						close(cancelled)
					case <-release:
						return nil, errS3IO
					}
					<-release
					return nil, req.Context().Err()
				}))
				ctx, cancel := context.WithCancel(context.Background())
				defer cancel()
				var operationErr error
				go func() {
					defer close(stopped)
					switch method {
					case "save":
						operationErr = be.Save(ctx, handle, backend.NewByteReader([]byte("x"), nil))
					case "load":
						operationErr = be.Load(ctx, handle, 0, 0, func(io.Reader) error { return nil })
					case "list":
						operationErr = be.List(ctx, backend.KeyFile, func(backend.FileInfo) error { return nil })
					case "stat-clarification":
						_, operationErr = be.Stat(ctx, handle)
					}
				}()
				defer func() {
					releaseOnce.Do(func() { close(release) })
					awaitS3Test(t, stopped)
					if closeStarted {
						awaitS3Test(t, closed)
					}
				}()
				awaitS3Test(t, entered)
				closeStarted = true
				if stopKind == "caller-cancel" {
					cancel()
					close(closed)
				} else {
					go func() { _ = be.Close(); close(closed) }()
				}
				awaitS3Test(t, cancelled)
				assertS3Pending(t, stopped)
				if stopKind == "backend-close" {
					assertS3Pending(t, closed)
				}
				releaseOnce.Do(func() { close(release) })
				awaitS3Test(t, stopped)
				awaitS3Test(t, closed)
				if !errors.Is(operationErr, context.Canceled) {
					t.Fatal("actual request cancellation was lost")
				}
			})
		}
	}
}

func TestS3CloseRetainsConsumer(t *testing.T) {
	entered, release, stopped, closed := make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{})
	var releaseOnce sync.Once
	closeStarted := false
	be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(*http.Request) (*http.Response, error) { return s3TestResponse(200, "owned bytes", nil), nil }))
	go func() {
		defer close(stopped)
		_ = be.Load(context.Background(), backend.Handle{Type: backend.PackFile, Name: s3TestID(1)}, 0, 0, func(reader io.Reader) error {
			close(entered)
			<-release
			_, err := io.Copy(io.Discard, reader)
			return err
		})
	}()
	defer func() {
		releaseOnce.Do(func() { close(release) })
		awaitS3Test(t, stopped)
		if closeStarted {
			awaitS3Test(t, closed)
		}
	}()
	awaitS3Test(t, entered)
	closeStarted = true
	go func() { _ = be.Close(); close(closed) }()
	awaitS3Test(t, be.ctx.Done())
	assertS3Pending(t, closed)
	releaseOnce.Do(func() { close(release) })
	awaitS3Test(t, stopped)
	awaitS3Test(t, closed)
}

func TestS3ResponseCapsBeforeConsumerOrXML(t *testing.T) {
	for _, list := range []bool{false, true} {
		limit := maxHTTPObjectBytes
		if list {
			limit = maxHTTPListBytes
		}
		be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(*http.Request) (*http.Response, error) {
			return &http.Response{StatusCode: 200, Header: make(http.Header), Body: io.NopCloser(io.LimitReader(s3ReaderFunc(func(out []byte) (int, error) {
				for i := range out {
					out[i] = 'x'
				}
				return len(out), nil
			}), int64(limit)+1)), ContentLength: -1}, nil
		}))
		called := false
		var err error
		if list {
			err = be.List(context.Background(), backend.KeyFile, func(backend.FileInfo) error { called = true; return nil })
		} else {
			err = be.Load(context.Background(), backend.Handle{Type: backend.PackFile, Name: s3TestID(1)}, 0, 0, func(io.Reader) error { called = true; return nil })
		}
		if !errors.Is(err, errResourceLimit) || called {
			t.Fatal("oversized reply reached consumer/decoder")
		}
	}
}
