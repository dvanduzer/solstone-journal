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
)

type nativeHTTPBlockedBody struct {
	entered    chan struct{}
	release    chan struct{}
	blockClose bool
}

func (body *nativeHTTPBlockedBody) Read(_ []byte) (int, error) {
	if !body.blockClose {
		close(body.entered)
		<-body.release
	}
	return 0, io.EOF
}
func (body *nativeHTTPBlockedBody) Close() error {
	if body.blockClose {
		close(body.entered)
		<-body.release
	}
	return nil
}

func TestNativeHTTPSourceReadAndCloseRemainOwnedAfterCancellation(t *testing.T) {
	for _, blockClose := range []bool{false, true} {
		t.Run(map[bool]string{false: "read", true: "close"}[blockClose], func(t *testing.T) {
			body := &nativeHTTPBlockedBody{entered: make(chan struct{}), release: make(chan struct{}), blockClose: blockClose}
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			transport, err := newNativeHTTPTransport(ctx, "https://storage.example")
			if err != nil {
				t.Fatal(err)
			}
			request, _ := http.NewRequest(http.MethodPut, "https://storage.example/object", body)
			request.ContentLength = 0
			finished := make(chan error, 1)
			joined := make(chan struct{})
			var release sync.Once
			go func() { defer close(joined); _, err := transport.RoundTrip(request); finished <- err }()
			t.Cleanup(func() {
				cancel()
				release.Do(func() { close(body.release) })
				select {
				case <-joined:
				case <-time.After(10 * time.Second):
					panic("body-source fixture could not join actual owner")
				}
			})
			select {
			case <-body.entered:
			case <-time.After(10 * time.Second):
				t.Fatal("source fixture did not reach its barrier")
			}
			cancel()
			select {
			case err := <-finished:
				t.Fatalf("source ownership released before actual I/O completed: %v", err)
			case <-time.After(50 * time.Millisecond):
			}
			release.Do(func() { close(body.release) })
			select {
			case err := <-finished:
				if !errors.Is(err, context.Canceled) {
					t.Fatalf("cancelled source proceeded to egress: %v", err)
				}
			case <-time.After(10 * time.Second):
				t.Fatal("released source owner did not return")
			}
		})
	}
}
