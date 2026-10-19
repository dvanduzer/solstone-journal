// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"testing"
)

type nativeHTTPTestBody struct {
	io.Reader
	closes int
	err    error
}

func (body *nativeHTTPTestBody) Close() error { body.closes++; return body.err }

func nativeHTTPTestTransport(t *testing.T) *nativeHTTPTransport {
	t.Helper()
	transport, err := newNativeHTTPTransport(context.Background(), "https://storage.example")
	if err != nil {
		t.Fatal(err)
	}
	return transport
}

func nativeHTTPTestRequest(t *testing.T, data string) (*http.Request, *nativeHTTPTestBody) {
	t.Helper()
	body := &nativeHTTPTestBody{Reader: strings.NewReader(data)}
	request, err := http.NewRequest(http.MethodPut, "https://storage.example/bucket/prefix/object", body)
	if err != nil {
		t.Fatal(err)
	}
	request.ContentLength = int64(len(data))
	return request, body
}

func TestNativeHTTPEndpointAuthority(t *testing.T) {
	for _, endpoint := range []string{
		"http://storage.example", "https://owner:secret@storage.example", "https://storage.example/bucket",
		"https://storage.example?ambient=1", "https://storage.example#fragment", "https://storage.example:bad",
		"https://storage.example:0", "https:///missing", "https://storage.example?",
	} {
		if _, err := newNativeHTTPTransport(context.Background(), endpoint); err == nil {
			t.Fatalf("accepted incompatible endpoint %q", endpoint)
		}
	}
	for _, endpoint := range []string{"https://storage.example", "https://STORAGE.example:443/", "https://[::1]:8443"} {
		if _, err := newNativeHTTPTransport(context.Background(), endpoint); err != nil {
			t.Fatalf("valid authority %q refused: %v", endpoint, err)
		}
	}
}

func TestNativeHTTPRejectsIncompatibleRequestBeforeBodyRead(t *testing.T) {
	changes := []struct {
		name string
		edit func(*http.Request)
	}{
		{"other authority", func(r *http.Request) { r.URL.Host = "other.example"; r.Host = r.URL.Host }},
		{"Host override", func(r *http.Request) { r.Host = "other.example" }},
		{"Host case changed", func(r *http.Request) { r.Host = "STORAGE.example" }},
		{"Host default port added", func(r *http.Request) { r.Host = "storage.example:443" }},
		{"malformed port", func(r *http.Request) { r.URL.Host = "storage.example:bad" }},
		{"opaque URL", func(r *http.Request) { r.URL.Opaque = "//other.example/secret" }},
		{"fragment", func(r *http.Request) { r.URL.Fragment = "fragment" }},
		{"userinfo", func(r *http.Request) { r.URL.User = url.UserPassword("owner", "secret") }},
		{"method", func(r *http.Request) { r.Method = http.MethodPost }},
		{"RequestURI", func(r *http.Request) { r.RequestURI = "/other" }},
		{"trailers", func(r *http.Request) { r.Trailer = make(http.Header) }},
		{"transfer", func(r *http.Request) { r.TransferEncoding = []string{"chunked"} }},
		{"unknown length", func(r *http.Request) { r.ContentLength = -1 }},
		{"oversized body", func(r *http.Request) { r.ContentLength = maxHTTPObjectBytes + 1 }},
		{"bad RawPath", func(r *http.Request) { r.URL.RawPath = "/different%2Fpath" }},
		{"bad query", func(r *http.Request) { r.URL.RawQuery = "prefix=%zz" }},
		{"ambient cookie", func(r *http.Request) { r.Header.Set("Cookie", "fixture") }},
		{"proxy credential", func(r *http.Request) { r.Header.Set("Proxy-Authorization", "fixture") }},
		{"header framing", func(r *http.Request) { r.Header.Set("X-Fixture", "one\r\ntwo") }},
		{"case-duplicate header", func(r *http.Request) {
			r.Header["X-Amz-Meta-Test"] = []string{"one"}
			r.Header["x-amz-meta-test"] = []string{"two"}
		}},
		{"header length mismatch", func(r *http.Request) { r.Header.Set("Content-Length", "4") }},
		{"many empty values", func(r *http.Request) { r.Header["X-Fixture"] = make([]string, 257) }},
	}
	for _, change := range changes {
		t.Run(change.name, func(t *testing.T) {
			request, body := nativeHTTPTestRequest(t, "fixture")
			body.Reader = &nativeHTTPRefuseRead{t: t}
			change.edit(request)
			_, _, err := nativeHTTPTestTransport(t).prepareNativeHTTPRequest(request)
			if err == nil || body.closes != 1 {
				t.Fatalf("refusal=%v, close count=%d", err, body.closes)
			}
		})
	}
}

type nativeHTTPRefuseRead struct{ t *testing.T }

func (reader *nativeHTTPRefuseRead) Read(_ []byte) (int, error) {
	reader.t.Fatal("invalid request read body before refusal")
	return 0, io.ErrUnexpectedEOF
}

func TestNativeHTTPRequestOwnsBodyAndSignedRepresentations(t *testing.T) {
	data := []byte("ciphertext fixture")
	body := &nativeHTTPTestBody{Reader: bytes.NewReader(data)}
	request, err := http.NewRequest(http.MethodPut, "https://storage.example/bucket/folder%20name%2Fpart%25?prefix=a%2Fb%2Bc%3D", body)
	if err != nil {
		t.Fatal(err)
	}
	request.ContentLength = int64(len(data))
	request.Header.Set("X-Amz-Security-Token", "fixture+/=temporary")
	request.Header.Set("Authorization", "AWS4-HMAC-SHA256 fixture")
	request.GetBody = func() (io.ReadCloser, error) { t.Fatal("unexpected replay provider"); return nil, errHTTPBody }
	wire, copied, err := nativeHTTPTestTransport(t).prepareNativeHTTPRequest(request)
	if err != nil || body.closes != 1 {
		t.Fatalf("prepare %v, close %d", err, body.closes)
	}
	data[0] = 'X'
	request.Header["X-Amz-Security-Token"][0] = "changed"
	request.URL.RawQuery = "changed=1"
	if string(copied) != "ciphertext fixture" || wire.Headers["X-Amz-Security-Token"][0] != "fixture+/=temporary" ||
		wire.URL != "https://storage.example/bucket/folder%20name%2Fpart%25?prefix=a%2Fb%2Bc%3D" {
		t.Fatal("caller mutation changed admitted bytes")
	}
}

func TestNativeHTTPBodyLengthAndCloseErrorsRefuse(t *testing.T) {
	for _, change := range []func(*http.Request, *nativeHTTPTestBody){
		func(r *http.Request, _ *nativeHTTPTestBody) { r.ContentLength-- },
		func(r *http.Request, _ *nativeHTTPTestBody) { r.ContentLength++ },
		func(_ *http.Request, b *nativeHTTPTestBody) { b.err = errors.New("private source path") },
	} {
		request, body := nativeHTTPTestRequest(t, "fixture")
		change(request, body)
		_, copied, err := nativeHTTPTestTransport(t).prepareNativeHTTPRequest(request)
		if !errors.Is(err, errHTTPBody) || copied != nil || body.closes != 1 {
			t.Fatalf("length/close failure hidden: %v, copied=%d, closed=%d", err, len(copied), body.closes)
		}
	}
}

func TestNativeHTTPBindsOperationContextDespiteBackgroundSDKRequest(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	transport, err := newNativeHTTPTransport(ctx, "https://storage.example")
	if err != nil {
		t.Fatal(err)
	}
	request, _ := http.NewRequestWithContext(context.Background(), http.MethodGet, "https://storage.example/bucket?list-type=2", nil)
	_, err = transport.RoundTrip(request)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("SDK background context escaped operation cancellation: %v", err)
	}
}

func TestNativeHTTPResponseCloseOwnsMemoryAndConcurrentReaders(t *testing.T) {
	data := []byte("owned response bytes")
	body := &nativeHTTPResponseBody{reader: bytes.NewReader(data), owned: data}
	var readers sync.WaitGroup
	for i := 0; i < 16; i++ {
		readers.Add(1)
		go func() { defer readers.Done(); _, _ = body.Read(make([]byte, 1)) }()
	}
	if err := body.Close(); err != nil {
		t.Fatal(err)
	}
	readers.Wait()
	if err := body.Close(); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(data, make([]byte, len(data))) {
		t.Fatal("owned response bytes retained after Close")
	}
	if _, err := body.Read(make([]byte, 1)); !errors.Is(err, io.ErrClosedPipe) {
		t.Fatal("closed response remains readable")
	}
}
