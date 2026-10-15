//go:build integration && (darwin || ios)

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"testing"
	"time"
)

type nativeHTTPTestOutcome struct {
	response *http.Response
	err      error
}

func TestNativeHTTPCancelBeforeStartAndRepeatedNativeSignals(t *testing.T) {
	if !nativeHTTPNullBoundaries() {
		t.Fatal("private native NULL boundary failed")
	}
	if !installNativeHTTPFixture(17) {
		t.Fatal("native fixture installation refused")
	}
	t.Cleanup(func() {
		releaseNativeHTTPFixture(15)
		if !drainNativeHTTPFixture() {
			panic("cancel-before-start callbacks did not join")
		}
		uninstallNativeHTTPFixture()
	})
	if code := nativeHTTPCancelBeforeStart(); code != 3 {
		t.Fatalf("cancel before start lost its terminal result: %d", code)
	}
}

func startNativeHTTPFixture(t *testing.T, mode int, method, target string, payload ...[]byte) (context.CancelFunc, <-chan nativeHTTPTestOutcome) {
	t.Helper()
	if !installNativeHTTPFixture(mode) {
		t.Fatal("native fixture installation refused")
	}
	ctx, cancel := context.WithCancel(context.Background())
	transport, err := newNativeHTTPTransport(ctx, "https://storage.example")
	if err != nil {
		t.Fatal(err)
	}
	var source io.Reader
	if len(payload) == 1 {
		source = bytes.NewReader(payload[0])
	}
	request, err := http.NewRequest(method, target, source)
	if err != nil {
		t.Fatal(err)
	}
	request.Header.Set("X-Amz-Security-Token", "fixture+/=temporary")
	request.Header.Set("Authorization", "AWS4-HMAC-SHA256 fixture")
	finished := make(chan nativeHTTPTestOutcome, 1)
	joined := make(chan struct{})
	go func() {
		defer close(joined)
		response, err := transport.RoundTrip(request)
		finished <- nativeHTTPTestOutcome{response, err}
	}()
	t.Cleanup(func() {
		cancel()
		releaseNativeHTTPFixture(15)
		if !drainNativeHTTPFixture() {
			panic("native fixture callbacks did not actually leave")
		}
		select {
		case <-joined:
		case <-time.After(10 * time.Second):
			// Keep the hooks and their state alive if native ownership failed.
			// Continuing the process could turn a failing lifetime test into a UAF.
			panic("native fixture cleanup could not join actual request")
		}
		select {
		case outcome := <-finished:
			if outcome.response != nil {
				_ = outcome.response.Body.Close()
			}
		default:
		}
		uninstallNativeHTTPFixture()
	})
	return cancel, finished
}

func nativeHTTPAwait(t *testing.T, finished <-chan nativeHTTPTestOutcome) nativeHTTPTestOutcome {
	t.Helper()
	select {
	case outcome := <-finished:
		return outcome
	case <-time.After(10 * time.Second):
		t.Fatal("native fixture did not actually finish")
		return nativeHTTPTestOutcome{}
	}
}

func assertNativeHTTPPending(t *testing.T, finished <-chan nativeHTTPTestOutcome) {
	t.Helper()
	select {
	case outcome := <-finished:
		if outcome.response != nil {
			_ = outcome.response.Body.Close()
		}
		t.Fatalf("native request returned before owned activity joined: %v", outcome.err)
	case <-time.After(50 * time.Millisecond):
	}
}

func TestNativeHTTPCompletionInvalidationAndCancelCallbackOwnership(t *testing.T) {
	for _, test := range []struct {
		mode          int
		stage         uint32
		cancel        bool
		wantCancelled bool
	}{
		{1, 1, true, false}, // successful completion committed, callback still running
		{2, 2, true, false}, // invalidation observed, delegate operation still running
		{3, 4, true, true},  // task drained, Go cancellation callback itself still running
	} {
		t.Run(string(rune('0'+test.mode)), func(t *testing.T) {
			cancel, finished := startNativeHTTPFixture(t, test.mode, http.MethodGet, "https://storage.example/bucket/object")
			if test.mode == 3 {
				if !waitNativeHTTPFixture(8) {
					t.Fatal("native task never entered controlled peer")
				}
				cancel()
			}
			if !waitNativeHTTPFixture(test.stage) {
				t.Fatal("native activity barrier not reached")
			}
			if test.mode == 3 && !waitNativeHTTPFixture(16) {
				t.Fatal("native wait did not actually finish while cancel callback remained paused")
			}
			if test.cancel && test.mode != 3 {
				cancel()
			}
			assertNativeHTTPPending(t, finished)
			releaseNativeHTTPFixture(test.stage)
			outcome := nativeHTTPAwait(t, finished)
			if test.wantCancelled {
				if !errors.Is(outcome.err, context.Canceled) || outcome.response != nil {
					t.Fatalf("cancellation not retained: %v", outcome.err)
				}
			} else {
				if outcome.err != nil || outcome.response == nil {
					t.Fatalf("late cancel rewrote native success: %v", outcome.err)
				}
				data, err := io.ReadAll(outcome.response.Body)
				_ = outcome.response.Body.Close()
				if err != nil || string(data) != "fixture" {
					t.Fatal("completed native body differs")
				}
			}
		})
	}
}

func TestNativeHTTPControlledResponseBoundsAndBytes(t *testing.T) {
	for _, test := range []struct {
		mode   int
		target string
		method string
		want   error
	}{
		{4, "https://storage.example/bucket?list-type=2", http.MethodGet, errHTTPLimit},
		{5, "https://storage.example/bucket/object", http.MethodGet, errHTTPLimit},
		{6, "https://storage.example/bucket/object", http.MethodGet, errHTTPProtocol},
		{7, "https://storage.example/bucket/object", http.MethodGet, errHTTPProtocol},
		{9, "https://storage.example/bucket/object", http.MethodGet, errHTTPLimit},
		{10, "https://storage.example/bucket/object", http.MethodHead, nil},
		{11, "https://storage.example/bucket/object", http.MethodGet, errHTTPRedirect},
		{12, "https://storage.example/bucket/object", http.MethodGet, errHTTPAuth},
		{13, "https://storage.example/bucket/object", http.MethodGet, errHTTPProtocol},
		{14, "https://storage.example/bucket/object", http.MethodGet, errHTTPLimit},
		{15, "https://storage.example/bucket/object", http.MethodGet, errHTTPProtocol},
		{16, "https://storage.example/bucket/object", http.MethodGet, nil},
	} {
		t.Run(string(rune('a'+test.mode)), func(t *testing.T) {
			_, finished := startNativeHTTPFixture(t, test.mode, test.method, test.target)
			outcome := nativeHTTPAwait(t, finished)
			if !errors.Is(outcome.err, test.want) {
				t.Fatalf("native response bound: got %v want %v", outcome.err, test.want)
			}
			if test.want != nil && outcome.response != nil {
				t.Fatal("refused native response became readable")
			}
			if test.want == nil && outcome.response == nil {
				t.Fatal("successful native HEAD lost its response")
			}
			if outcome.response != nil {
				data, readErr := io.ReadAll(outcome.response.Body)
				_ = outcome.response.Body.Close()
				wantLength := int64(0)
				if test.method == http.MethodHead {
					wantLength = 100000000
				}
				if readErr != nil || len(data) != 0 || outcome.response.ContentLength != wantLength {
					t.Fatal("HEAD did not preserve empty body and object-length metadata")
				}
			}
			var observed struct {
				Token         string `json:"token"`
				Stores        bool   `json:"stores_closed"`
				NativeCode    int    `json:"native_code"`
				PolicyRefused bool   `json:"policy_refused"`
			}
			if json.Unmarshal(nativeHTTPFixtureSnapshot(), &observed) != nil || !observed.Stores || observed.Token != "fixture+/=temporary" {
				t.Fatal("native configuration or explicit temporary token changed")
			}
			wantCode := int32(0)
			switch test.want {
			case errHTTPLimit:
				wantCode = 2
			case errHTTPProtocol:
				// False Content-Length is checked against the completed owned
				// bytes by Go; the other protocol checks must refuse natively.
				if test.mode != 7 {
					wantCode = 8
				}
			case errHTTPRedirect:
				wantCode = 4
			case errHTTPAuth:
				wantCode = 5
			}
			if observed.NativeCode != int(wantCode) {
				t.Fatalf("refusal was deferred past native bounds: got %d want %d", observed.NativeCode, wantCode)
			}
			if (test.mode == 11 || test.mode == 12) && !observed.PolicyRefused {
				t.Fatal("native policy delegate did not refuse")
			}
		})
	}
}

func TestNativeHTTPControlledSignedRequestRepresentations(t *testing.T) {
	target := "https://storage.example/bucket/folder%20name%2Fpart%25?prefix=a%2Fb%2Bc%3D&x=one%20two"
	data := []byte("\x00ciphertext+/=\xff")
	_, finished := startNativeHTTPFixture(t, 18, http.MethodPut, target, data)
	outcome := nativeHTTPAwait(t, finished)
	if outcome.err != nil || outcome.response == nil {
		t.Fatalf("controlled native PUT failed: %v", outcome.err)
	}
	if err := outcome.response.Body.Close(); err != nil {
		t.Fatal(err)
	}
	var observed struct {
		URL           string `json:"url"`
		Method        string `json:"method"`
		Token         string `json:"token"`
		Authorization string `json:"authorization"`
		Body          string `json:"body_base64"`
	}
	if json.Unmarshal(nativeHTTPFixtureSnapshot(), &observed) != nil || observed.URL != target || observed.Method != http.MethodPut ||
		observed.Token != "fixture+/=temporary" || observed.Authorization != "AWS4-HMAC-SHA256 fixture" ||
		observed.Body != base64.StdEncoding.EncodeToString(data) {
		t.Fatal("native URLProtocol observed changed signed request representations")
	}
}
