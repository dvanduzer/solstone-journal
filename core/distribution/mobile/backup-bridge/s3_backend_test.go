// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"testing"

	"github.com/restic/restic/internal/backend"
)

type s3TestTransport func(*http.Request) (*http.Response, error)

func (transport s3TestTransport) RoundTrip(request *http.Request) (*http.Response, error) {
	return transport(request)
}

func s3TestConfig() s3Config {
	return s3Config{endpoint: "https://storage.example", region: "test-region-1", bucket: "owner-bucket", prefix: "owned repo/+percent%/雪", access: "fixture-access", secret: "fixture-secret", token: "fixture+/=%token"}
}

// Injected transport construction exists only in tests, never an app callback.
func s3TestBackend(t *testing.T, cfg s3Config, transport http.RoundTripper) *s3Backend {
	t.Helper()
	if err := cfg.validate(); err != nil {
		t.Fatal("invalid controlled configuration")
	}
	ctx, cancel := context.WithCancel(context.Background())
	be := &s3Backend{cfg: cfg, transport: transport, ctx: ctx, cancel: cancel}
	t.Cleanup(func() { _ = be.Close() })
	return be
}

func s3TestResponse(status int, body string, headers http.Header) *http.Response {
	if headers == nil {
		headers = make(http.Header)
	}
	if headers.Get("Content-Length") == "" {
		headers.Set("Content-Length", strconv.Itoa(len(body)))
	}
	return &http.Response{StatusCode: status, Header: headers, Body: io.NopCloser(strings.NewReader(body)), ContentLength: int64(len(body))}
}

func s3TestID(n int) string { return fmt.Sprintf("%064x", n) }

func TestS3ConfigurationAndHandleConfinement(t *testing.T) {
	valid := s3TestConfig()
	if err := valid.validate(); err != nil {
		t.Fatal(err)
	}
	for _, change := range []func(*s3Config){
		func(c *s3Config) { c.endpoint = "http://storage.example" }, func(c *s3Config) { c.endpoint = "https://user@storage.example" },
		func(c *s3Config) { c.endpoint += "/path" }, func(c *s3Config) { c.endpoint += "?query=1" }, func(c *s3Config) { c.endpoint += "#fragment" },
		func(c *s3Config) { c.region = "" }, func(c *s3Config) { c.region = "UPPER" }, func(c *s3Config) { c.access = "" }, func(c *s3Config) { c.secret = "" },
		func(c *s3Config) { c.token = "bad\nheader" }, func(c *s3Config) { c.bucket = "192.0.2.1" }, func(c *s3Config) { c.bucket = "Upper" },
		func(c *s3Config) { c.bucket = "-bad" }, func(c *s3Config) { c.bucket = "bad..bucket" }, func(c *s3Config) { c.bucket = "test--use1-az1--x-s3" },
		func(c *s3Config) { c.bucket = "test-s3alias" }, func(c *s3Config) { c.bucket = "test--ol-s3" },
		func(c *s3Config) { c.prefix = "" }, func(c *s3Config) { c.prefix = "/escape" }, func(c *s3Config) { c.prefix = "trailing/" },
		func(c *s3Config) { c.prefix = "a/../b" }, func(c *s3Config) { c.prefix = "a//b" }, func(c *s3Config) { c.prefix = "a\\b" },
		func(c *s3Config) { c.prefix = "a\x00b" }, func(c *s3Config) { c.prefix = string([]byte{0xff}) }, func(c *s3Config) { c.prefix = strings.Repeat("x", 513) },
	} {
		cfg := valid
		change(&cfg)
		if _, err := newS3Backend(context.Background(), cfg); !errors.Is(err, errS3Config) {
			t.Fatal("invalid configuration admitted")
		}
	}
	calls := 0
	be := s3TestBackend(t, valid, s3TestTransport(func(*http.Request) (*http.Response, error) { calls++; return nil, errors.New("must not egress") }))
	for _, name := range []string{"", strings.Repeat("0", 64)} {
		key, err := be.filename(backend.Handle{Type: backend.ConfigFile, Name: name})
		if err != nil || key != valid.prefix+"/config" {
			t.Fatal("restic config forms diverged")
		}
	}
	for _, handle := range []backend.Handle{
		{Type: backend.ConfigFile, Name: s3TestID(1)}, {Type: backend.KeyFile, Name: "../escape"}, {Type: backend.KeyFile, Name: strings.Repeat("A", 64)},
		{Type: backend.PackFile, Name: s3TestID(1) + "/extra"}, {Type: backend.FileType(255), Name: s3TestID(1)},
	} {
		if err := be.Save(context.Background(), handle, backend.NewByteReader([]byte("x"), nil)); !errors.Is(err, errS3Handle) {
			t.Fatal("invalid handle admitted")
		}
	}
	if calls != 0 {
		t.Fatal("invalid handle reached transport")
	}
	if name, err := be.listedName(backend.PackFile, valid.prefix+"/data/00/"+s3TestID(1)); err != nil || name != s3TestID(1) {
		t.Fatal("valid shard refused")
	}
	for _, key := range []string{valid.prefix + "-sibling/data/00/" + s3TestID(1), valid.prefix + "/data/ab/" + s3TestID(1), valid.prefix + "/data/00/" + s3TestID(1) + "/extra"} {
		if _, err := be.listedName(backend.PackFile, key); err == nil {
			t.Fatal("malformed listed layout admitted")
		}
	}
}

func TestS3SigningAndExactObjectBytes(t *testing.T) {
	for _, endpoint := range []string{"https://storage.example", "https://s3.us-east-1.amazonaws.com"} {
		cfg := s3TestConfig()
		cfg.endpoint = endpoint
		payload := []byte("exact object bytes\x00\xff")
		key := cfg.prefix + "/keys/" + s3TestID(1)
		calls := 0
		be := s3TestBackend(t, cfg, s3TestTransport(func(req *http.Request) (*http.Response, error) {
			calls++
			if req.URL.Scheme+"://"+req.URL.Host != cfg.endpoint || req.URL.Path != "/"+cfg.bucket+"/"+key || req.URL.EscapedPath() != "/owner-bucket/owned%20repo/+percent%25/%E9%9B%AA/keys/"+s3TestID(1) {
				t.Fatal("signed object authority/path changed")
			}
			if req.URL.RawQuery != "" || req.Header.Get("X-Amz-Security-Token") != cfg.token || !strings.Contains(req.Header.Get("Authorization"), "Credential="+cfg.access+"/") {
				t.Fatal("explicit signing scope changed")
			}
			if req.Method == http.MethodPut {
				data, err := io.ReadAll(req.Body)
				if err != nil || !bytes.Equal(data, payload) || req.ContentLength != int64(len(payload)) {
					t.Fatal("signed body changed")
				}
				digest := sha256.Sum256(payload)
				if req.Header.Get("X-Amz-Content-Sha256") != hex.EncodeToString(digest[:]) {
					t.Fatal("payload digest differs")
				}
				return s3TestResponse(200, "", nil), nil
			}
			if req.Method == http.MethodHead {
				return s3TestResponse(200, "", http.Header{"Content-Length": {strconv.Itoa(len(payload))}}), nil
			}
			if req.Method != http.MethodGet {
				t.Fatal("unexpected SDK/admin method")
			}
			return s3TestResponse(200, string(payload), nil), nil
		}))
		h := backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}
		if err := be.Save(context.Background(), h, backend.NewByteReader(payload, nil)); err != nil {
			t.Fatal(err)
		}
		if info, err := be.Stat(context.Background(), h); err != nil || info.Size != int64(len(payload)) {
			t.Fatal("HEAD size differs")
		}
		if err := be.Load(context.Background(), h, 0, 0, func(reader io.Reader) error {
			data, err := io.ReadAll(reader)
			if err != nil || !bytes.Equal(data, payload) {
				return errors.New("bytes differ")
			}
			return nil
		}); err != nil {
			t.Fatal(err)
		}
		if calls != 3 {
			t.Fatal("unexpected retries or discovery")
		}
	}
}

func TestS3LoadRangeValidation(t *testing.T) {
	for _, tc := range []struct {
		name              string
		length            int
		offset            int64
		status            int
		rangeHeader, body string
		valid             bool
	}{
		{"full", 0, 0, 200, "", "abcdef", true}, {"empty", 0, 0, 200, "", "", true}, {"slice", 3, 2, 206, "bytes 2-4/6", "cde", true}, {"tail", 0, 2, 206, "bytes 2-5/6", "cdef", true},
		{"ignored", 3, 2, 200, "", "abcdef", false}, {"shifted", 3, 2, 206, "bytes 1-3/6", "bcd", false}, {"short", 3, 2, 206, "bytes 2-4/6", "cd", false},
		{"wildcard", 3, 2, 206, "bytes 2-4/*", "cde", false}, {"early-tail", 0, 2, 206, "bytes 2-4/6", "cde", false}, {"unexpected206", 0, 0, 206, "bytes 0-5/6", "abcdef", false},
		{"negative", -1, 0, 200, "", "", false}, {"overflow", 1, int64(^uint64(0) >> 1), 200, "", "", false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(req *http.Request) (*http.Response, error) {
				return s3TestResponse(tc.status, tc.body, http.Header{"Content-Range": {tc.rangeHeader}}), nil
			}))
			called := false
			err := be.Load(context.Background(), backend.Handle{Type: backend.PackFile, Name: s3TestID(1)}, tc.length, tc.offset, func(reader io.Reader) error {
				called = true
				data, _ := io.ReadAll(reader)
				if string(data) != tc.body {
					t.Fatal("range bytes changed")
				}
				return nil
			})
			if (err == nil) != tc.valid || called != tc.valid {
				t.Fatal("range validation/callback disposition differs")
			}
			if !tc.valid && !be.IsPermanentError(err) {
				t.Fatal("range refusal is not permanent")
			}
		})
	}
}

func TestS3StatAbsenceNeedsExplicitSameObjectEvidence(t *testing.T) {
	for _, tc := range []struct {
		name    string
		status  int
		body    string
		headers http.Header
		want    error
		size    int64
	}{
		{"missing", 404, "<Error><Code>NoSuchKey</Code></Error>", nil, errS3NotFound, 0},
		{"bucket", 404, "<Error><Code>NoSuchBucket</Code></Error>", nil, errS3BucketMissing, 0},
		{"ambiguous", 404, "", nil, errS3Protocol, 0},
		{"incoherent", 500, "<Error><Code>NoSuchKey</Code></Error>", nil, errS3IO, 0},
		{"present", 200, "now present", nil, nil, 11},
		{"partial", 206, "one", http.Header{"Content-Range": {"bytes 0-2/9"}}, errS3Protocol, 0},
	} {
		t.Run(tc.name, func(t *testing.T) {
			calls := 0
			path := ""
			be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(req *http.Request) (*http.Response, error) {
				calls++
				if calls == 1 {
					path = req.URL.String()
					if req.Method != http.MethodHead {
						t.Fatal("Stat did not use HEAD")
					}
					return s3TestResponse(404, "", http.Header{"Content-Length": {"137"}}), nil
				}
				if calls != 2 || req.Method != http.MethodGet || req.URL.String() != path {
					t.Fatal("clarification exceeded exact object")
				}
				return s3TestResponse(tc.status, tc.body, tc.headers), nil
			}))
			info, err := be.Stat(context.Background(), backend.Handle{Type: backend.ConfigFile})
			if !errors.Is(err, tc.want) || (tc.want == nil && info.Size != tc.size) || calls != 2 {
				t.Fatal("Stat clarification differs")
			}
			if be.IsNotExist(err) != (tc.want == errS3NotFound) {
				t.Fatal("uncertain failure became object absence")
			}
		})
	}
}

func s3TestPage(bucket, prefix string, keys []string, truncated bool, token string) string {
	escape := func(s string) string { var out bytes.Buffer; _ = xml.EscapeText(&out, []byte(s)); return out.String() }
	var out strings.Builder
	fmt.Fprintf(&out, "<ListBucketResult><Name>%s</Name><Prefix>%s</Prefix><KeyCount>%d</KeyCount><MaxKeys>256</MaxKeys><IsTruncated>%t</IsTruncated>", escape(bucket), escape(prefix), len(keys), truncated)
	if token != "" {
		fmt.Fprintf(&out, "<NextContinuationToken>%s</NextContinuationToken>", escape(token))
	}
	for _, key := range keys {
		fmt.Fprintf(&out, "<Contents><Key>%s</Key><Size>32</Size></Contents>", escape(key))
	}
	out.WriteString("</ListBucketResult>")
	return out.String()
}

func TestS3ListPagesAndMalformedReplies(t *testing.T) {
	cfg := s3TestConfig()
	prefix := cfg.prefix + "/keys/"
	first, second := prefix+s3TestID(1), prefix+s3TestID(2)
	for _, tc := range []struct {
		name, one, two string
		valid          bool
		wantCount      int
	}{
		{"two-pages", s3TestPage(cfg.bucket, prefix, []string{first}, true, "next+/=%"), s3TestPage(cfg.bucket, prefix, []string{second}, false, ""), true, 2},
		{"empty", s3TestPage(cfg.bucket, prefix, nil, false, ""), "", true, 0},
		{"missing-truncation", strings.Replace(s3TestPage(cfg.bucket, prefix, []string{first}, false, ""), "<IsTruncated>false</IsTruncated>", "", 1), "", false, 0},
		{"duplicate-truncation", strings.Replace(s3TestPage(cfg.bucket, prefix, []string{first}, false, ""), "</ListBucketResult>", "<IsTruncated>false</IsTruncated></ListBucketResult>", 1), "", false, 0},
		{"wrong-prefix", s3TestPage(cfg.bucket, prefix+"sibling", []string{first}, false, ""), "", false, 0},
		{"duplicate-key", s3TestPage(cfg.bucket, prefix, []string{first, first}, false, ""), "", false, 0},
		{"cross-page-duplicate", s3TestPage(cfg.bucket, prefix, []string{first}, true, "next+/=%"), s3TestPage(cfg.bucket, prefix, []string{first}, false, ""), false, 1},
		{"token-loop", s3TestPage(cfg.bucket, prefix, []string{first}, true, "next+/=%"), s3TestPage(cfg.bucket, prefix, []string{second}, true, "next+/=%"), false, 1},
		{"missing-token", s3TestPage(cfg.bucket, prefix, []string{first}, true, ""), "", false, 0},
		{"trailing-document", s3TestPage(cfg.bucket, prefix, []string{first}, false, "") + "<other/>", "", false, 0},
		{"sibling-key", s3TestPage(cfg.bucket, prefix, []string{cfg.prefix + "-sibling/keys/" + s3TestID(1)}, false, ""), "", false, 0},
		{"root-level-key", s3TestPage(cfg.bucket, prefix, []string{s3TestID(1)}, false, ""), "", false, 0},
	} {
		t.Run(tc.name, func(t *testing.T) {
			calls := 0
			be := s3TestBackend(t, cfg, s3TestTransport(func(req *http.Request) (*http.Response, error) {
				calls++
				if req.Method != http.MethodGet || req.URL.Path != "/"+cfg.bucket || req.URL.Query().Get("prefix") != prefix || req.URL.Query().Get("list-type") != "2" || req.URL.Query().Get("max-keys") != "256" {
					t.Fatal("list authority changed")
				}
				if calls == 1 {
					return s3TestResponse(200, tc.one, nil), nil
				}
				if calls != 2 || tc.two == "" || req.URL.Query().Get("continuation-token") != "next+/=%" {
					t.Fatal("unexpected list continuation")
				}
				return s3TestResponse(200, tc.two, nil), nil
			}))
			count := 0
			err := be.List(context.Background(), backend.KeyFile, func(info backend.FileInfo) error {
				count++
				if info.Size != 32 || !validS3ID(info.Name) {
					t.Fatal("list entry differs")
				}
				return nil
			})
			if (err == nil) != tc.valid || count != tc.wantCount {
				t.Fatal("list result/partial callbacks differ")
			}
		})
	}
}

func TestS3SanitizedErrorsPreserveGuardClassifications(t *testing.T) {
	cfg := s3TestConfig()
	prefix := cfg.prefix + "/keys/"
	sentinel := "private-password-and-/owner/path"
	be := s3TestBackend(t, cfg, s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.URL.Query().Get("list-type") == "2" {
			keys := make([]string, maxKeyFiles+1)
			for i := range keys {
				keys[i] = prefix + s3TestID(i+1)
			}
			return s3TestResponse(200, s3TestPage(cfg.bucket, prefix, keys, false, ""), nil), nil
		}
		return s3TestResponse(200, "{}", nil), nil
	}))
	guard := guardedBackend{be}
	for _, err := range []error{
		guard.List(context.Background(), backend.KeyFile, func(backend.FileInfo) error { return nil }),
		be.Load(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, 0, 0, func(io.Reader) error { return fmt.Errorf("%s: %w", sentinel, errResourceLimit) }),
	} {
		if !errors.Is(err, errResourceLimit) || refusalResult(err, DiagAuthFailed).status != StatusCapExceeded || strings.Contains(err.Error(), sentinel) {
			t.Fatal("resource refusal classification was lost")
		}
	}
	err := be.Load(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, 0, 0, func(io.Reader) error { return errors.New(sentinel) })
	if !errors.Is(err, errS3Callback) || strings.Contains(err.Error(), sentinel) {
		t.Fatal("consumer error escaped")
	}
	if !errors.Is(s3ConsumerError(context.Background(), fmt.Errorf("%s: %w", sentinel, context.Canceled)), context.Canceled) {
		t.Fatal("cancellation identity lost")
	}
	if err := be.Remove(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}); !errors.Is(err, errS3Denied) {
		t.Fatal("non-lock deletion admitted")
	}
	if err := be.Delete(context.Background()); !errors.Is(err, errS3Denied) {
		t.Fatal("delete-all admitted")
	}
}

func TestS3EveryListSingletonRequiresExactlyOneValue(t *testing.T) {
	cfg := s3TestConfig()
	prefix := cfg.prefix + "/keys/"
	page := s3TestPage(cfg.bucket, prefix, []string{prefix + s3TestID(1)}, false, "")
	for _, tag := range []string{"Name", "Prefix", "KeyCount", "MaxKeys", "IsTruncated", "Key", "Size"} {
		start := strings.Index(page, "<"+tag+">")
		end := strings.Index(page, "</"+tag+">") + len("</"+tag+">")
		field := page[start:end]
		for _, altered := range []string{strings.Replace(page, field, "", 1), strings.Replace(page, field, field+field, 1)} {
			be := s3TestBackend(t, cfg, s3TestTransport(func(*http.Request) (*http.Response, error) { return s3TestResponse(200, altered, nil), nil }))
			called := false
			err := be.List(context.Background(), backend.KeyFile, func(backend.FileInfo) error { called = true; return nil })
			if !errors.Is(err, errS3Protocol) || called {
				t.Fatalf("absent or duplicated %s became a complete page", tag)
			}
		}
	}
}

func TestS3GuardedKeyResourcesRetainCapClassification(t *testing.T) {
	encoded, err := json.Marshal(map[string]any{"kdf": "scrypt", "N": 1 << 30, "r": 8, "p": 1, "salt": bytes.Repeat([]byte{1}, 64), "data": bytes.Repeat([]byte{1}, 32)})
	if err != nil {
		t.Fatal(err)
	}
	be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(*http.Request) (*http.Response, error) { return s3TestResponse(200, string(encoded), nil), nil }))
	called := false
	err = (guardedBackend{be}).Load(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, 0, 0, func(io.Reader) error { called = true; return nil })
	if !errors.Is(err, errResourceLimit) || called || refusalResult(err, DiagAuthFailed).status != StatusCapExceeded {
		t.Fatal("guarded KDF resource refusal was lost")
	}
}

type s3TestReader struct {
	io.Reader
	length int64
}

func (*s3TestReader) Rewind() error        { return errors.New("unused rewind") }
func (reader *s3TestReader) Length() int64 { return reader.length }
func (*s3TestReader) Hash() []byte         { return nil }

type s3ErrorReader struct{ err error }

func (reader s3ErrorReader) Read([]byte) (int, error) { return 0, reader.err }

type s3CloseErrorBody struct {
	io.Reader
	err error
}

func (body s3CloseErrorBody) Close() error { return body.err }

func TestS3InputAndResponseFailuresStayVisibleAndScrubbed(t *testing.T) {
	sentinel := "private-credential-/owner/private-path"
	for _, tc := range []struct {
		name   string
		reader backend.RewindReader
		want   error
	}{
		{"negative", &s3TestReader{strings.NewReader(""), -1}, errResourceLimit},
		{"oversized", &s3TestReader{strings.NewReader(""), maxHTTPObjectBytes + 1}, errResourceLimit},
		{"short", &s3TestReader{strings.NewReader("x"), 2}, errS3Protocol},
		{"extra", &s3TestReader{strings.NewReader("xx"), 1}, errS3Protocol},
		{"source-error", &s3TestReader{s3ErrorReader{errors.New(sentinel)}, 1}, errS3IO},
	} {
		t.Run(tc.name, func(t *testing.T) {
			calls := 0
			be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(*http.Request) (*http.Response, error) { calls++; return s3TestResponse(200, "", nil), nil }))
			err := be.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, tc.reader)
			if !errors.Is(err, tc.want) || calls != 0 || strings.Contains(err.Error(), sentinel) {
				t.Fatal("invalid source escaped or reached egress")
			}
		})
	}
	for _, tc := range []struct {
		name     string
		response *http.Response
		external error
		want     error
	}{
		{"transport", nil, errors.New(sentinel), errS3IO},
		{"provider500", s3TestResponse(500, "<Error><Code>NoSuchKey</Code><Message>"+sentinel+"</Message></Error>", nil), nil, errS3IO},
		{"provider-denied", s3TestResponse(403, sentinel, nil), nil, errS3Denied},
		{"oversize-error", s3TestResponse(500, strings.Repeat("x", maxHTTPErrorBytes+1), nil), nil, errResourceLimit},
		{"close-error", &http.Response{StatusCode: 200, Header: http.Header{"Content-Length": {"0"}}, Body: s3CloseErrorBody{strings.NewReader(""), errors.New(sentinel)}}, nil, errS3IO},
	} {
		t.Run(tc.name, func(t *testing.T) {
			calls := 0
			be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(req *http.Request) (*http.Response, error) {
				calls++
				if req.Method != http.MethodPut {
					t.Fatal("compensating request issued")
				}
				return tc.response, tc.external
			}))
			err := be.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, backend.NewByteReader([]byte("x"), nil))
			if !errors.Is(err, tc.want) || calls != 1 || strings.Contains(err.Error(), sentinel) {
				t.Fatal("write failure was hidden, retried or leaked")
			}
		})
	}
}

func TestS3ResponseFramingAndXMLRefusals(t *testing.T) {
	for _, tc := range []struct {
		name     string
		response *http.Response
		want     error
	}{
		{"missing-length", &http.Response{StatusCode: 200, Header: make(http.Header), Body: io.NopCloser(strings.NewReader("x")), ContentLength: -1}, nil},
		{"false-length", s3TestResponse(200, "x", http.Header{"Content-Length": {"2"}}), errS3Protocol},
		{"duplicate-length", s3TestResponse(200, "x", http.Header{"Content-Length": {"1", "1"}}), errS3Protocol},
		{"encoding", s3TestResponse(200, "x", http.Header{"Content-Encoding": {"gzip"}}), errS3Protocol},
		{"multipart", s3TestResponse(200, "x", http.Header{"Content-Type": {"multipart/byteranges"}}), errS3Protocol},
	} {
		t.Run(tc.name, func(t *testing.T) {
			be := s3TestBackend(t, s3TestConfig(), s3TestTransport(func(*http.Request) (*http.Response, error) { return tc.response, nil }))
			called := false
			err := be.Load(context.Background(), backend.Handle{Type: backend.PackFile, Name: s3TestID(1)}, 0, 0, func(io.Reader) error { called = true; return nil })
			if !errors.Is(err, tc.want) || called != (tc.want == nil) {
				t.Fatal("response framing disposition differs")
			}
		})
	}
	for _, body := range []string{"<!DOCTYPE Error><Error><Code>NoSuchKey</Code></Error>", "<Error><!DOCTYPE other><Code>NoSuchKey</Code></Error>", "<Error><Code>NoSuchKey</Code><Code>NoSuchKey</Code></Error>", "<Error/>", "<Error><Code>NoSuchKey</Code></Error><other/>"} {
		if err := s3ErrorResponse(http.MethodGet, 404, []byte(body)); !errors.Is(err, errS3Protocol) {
			t.Fatal("malformed error became object absence")
		}
	}
}

func TestS3BackendTeardownScopeRemovals(t *testing.T) {
	deleted := make(map[string]bool)
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method == http.MethodDelete {
			deleted[req.URL.Path] = true
			return s3TestResponse(http.StatusNoContent, "", nil), nil
		}
		return s3TestResponse(http.StatusOK, "", nil), nil
	})
	be := s3TestBackend(t, s3TestConfig(), transport)
	ctx := context.Background()
	teardownCtx := withTeardownScope(ctx)

	// Remove of pack without scope is errS3Denied
	if err := be.Remove(ctx, backend.Handle{Type: backend.PackFile, Name: s3TestID(1)}); !errors.Is(err, errS3Denied) {
		t.Fatalf("expected errS3Denied without scope, got: %v", err)
	}

	// With teardown scope, snapshot, pack, and index succeed
	for _, handleType := range []backend.FileType{backend.SnapshotFile, backend.PackFile, backend.IndexFile} {
		handle := backend.Handle{Type: handleType, Name: s3TestID(2)}
		if err := be.Remove(teardownCtx, handle); err != nil {
			t.Fatalf("expected Remove of %v with teardown scope to succeed, got: %v", handleType, err)
		}
	}

	// Config, key, and Delete stay errS3Denied with the scope on
	for _, handleType := range []backend.FileType{backend.ConfigFile, backend.KeyFile} {
		handle := backend.Handle{Type: handleType, Name: s3TestID(3)}
		if handleType == backend.ConfigFile {
			handle.Name = "" // Config handles have no object-ID name in the pinned backend.
		}
		if err := be.Remove(teardownCtx, handle); !errors.Is(err, errS3Denied) {
			t.Fatalf("expected Remove of %v with scope to stay errS3Denied, got: %v", handleType, err)
		}
	}
	if err := be.Delete(teardownCtx); !errors.Is(err, errS3Denied) {
		t.Fatalf("expected Delete with scope to stay errS3Denied, got: %v", err)
	}
}

func TestS3OperationBackendRemoveLatch(t *testing.T) {
	teardownCtx := withTeardownScope(context.Background())

	newOpBackend := func(t *testing.T, roundTrip func(*http.Request) (*http.Response, error)) (*s3OperationBackend, *s3Owner) {
		owner := &s3Owner{
			lockAcks: make(map[string]bool),
		}
		base := s3TestBackend(t, s3TestConfig(), s3TestTransport(roundTrip))
		return &s3OperationBackend{s3Backend: base, owner: owner}, owner
	}

	// 1. Pack DELETE with RoundTrip error sets writeFailed and returns non-nil error
	{
		be, owner := newOpBackend(t, func(*http.Request) (*http.Response, error) {
			return nil, errors.New("round-trip network drop")
		})
		err := be.Remove(teardownCtx, backend.Handle{Type: backend.PackFile, Name: s3TestID(1)})
		if err == nil {
			t.Fatal("expected pack DELETE with RoundTrip error to return non-nil error")
		}
		if !owner.hasWriteFailed() {
			t.Fatal("expected pack DELETE RoundTrip error to latch writeFailed")
		}
	}

	// 2. Pack DELETE with HTTP 500 sets writeFailed and returns non-nil error
	{
		be, owner := newOpBackend(t, func(*http.Request) (*http.Response, error) {
			return s3TestResponse(http.StatusInternalServerError, "internal server error", nil), nil
		})
		err := be.Remove(teardownCtx, backend.Handle{Type: backend.PackFile, Name: s3TestID(2)})
		if err == nil {
			t.Fatal("expected pack DELETE with HTTP 500 to return non-nil error")
		}
		if !owner.hasWriteFailed() {
			t.Fatal("expected pack DELETE HTTP 500 to latch writeFailed")
		}
	}

	// 3. Pack DELETE with 204 does not set writeFailed
	{
		be, owner := newOpBackend(t, func(*http.Request) (*http.Response, error) {
			return s3TestResponse(http.StatusNoContent, "", nil), nil
		})
		err := be.Remove(teardownCtx, backend.Handle{Type: backend.PackFile, Name: s3TestID(3)})
		if err != nil {
			t.Fatalf("expected pack DELETE with 204 to succeed, got: %v", err)
		}
		if owner.hasWriteFailed() {
			t.Fatal("expected pack DELETE 204 not to set writeFailed")
		}
	}

	// 4. Snapshot and index errors do the same latch
	for _, handleType := range []backend.FileType{backend.SnapshotFile, backend.IndexFile} {
		be, owner := newOpBackend(t, func(*http.Request) (*http.Response, error) {
			return s3TestResponse(http.StatusInternalServerError, "internal server error", nil), nil
		})
		err := be.Remove(teardownCtx, backend.Handle{Type: handleType, Name: s3TestID(4)})
		if err == nil {
			t.Fatalf("expected %v DELETE with HTTP 500 to return non-nil error", handleType)
		}
		if !owner.hasWriteFailed() {
			t.Fatalf("expected %v DELETE HTTP 500 to latch writeFailed", handleType)
		}
	}

	// 5. Lock-file Remove does not use latch
	{
		lockID := s3TestID(5)
		be, owner := newOpBackend(t, func(*http.Request) (*http.Response, error) {
			return s3TestResponse(http.StatusInternalServerError, "lock delete failed", nil), nil
		})
		owner.record.Active = &s3OwnerIntent{
			Locks: []string{lockID},
		}
		err := be.Remove(teardownCtx, backend.Handle{Type: backend.LockFile, Name: lockID})
		if err == nil {
			t.Fatal("expected lock-file DELETE with HTTP 500 to return non-nil error")
		}
		if owner.hasWriteFailed() {
			t.Fatal("lock-file Remove error must not latch writeFailed")
		}
	}

	// 6. Config and key Remove stay denied
	for _, handleType := range []backend.FileType{backend.ConfigFile, backend.KeyFile} {
		be, owner := newOpBackend(t, func(*http.Request) (*http.Response, error) {
			return s3TestResponse(http.StatusNoContent, "", nil), nil
		})
		err := be.Remove(teardownCtx, backend.Handle{Type: handleType, Name: s3TestID(6)})
		if !errors.Is(err, errS3Denied) {
			t.Fatalf("expected Remove of %v to stay errS3Denied, got: %v", handleType, err)
		}
		if owner.hasWriteFailed() {
			t.Fatalf("denied %v Remove must not latch writeFailed", handleType)
		}
	}
}
