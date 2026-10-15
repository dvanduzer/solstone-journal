// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"hash"
	"net/http"
	"net/netip"
	"net/url"
	"path"
	"strings"
	"sync"
	"unicode/utf8"

	"github.com/restic/restic/internal/backend"
	"github.com/restic/restic/internal/backend/layout"
)

var (
	errS3Config        = errors.New("S3_CONFIGURATION_INVALID")
	errS3Handle        = errors.New("S3_HANDLE_INVALID")
	errS3Closed        = errors.New("S3_BACKEND_CLOSED")
	errS3Protocol      = errors.New("S3_RESPONSE_INVALID")
	errS3Range         = errors.New("S3_RANGE_INVALID")
	errS3IO            = errors.New("S3_IO_FAILED")
	errS3Denied        = errors.New("S3_ACCESS_DENIED")
	errS3NotFound      = errors.New("S3_OBJECT_NOT_FOUND")
	errS3BucketMissing = errors.New("S3_BUCKET_NOT_FOUND")
	errS3Ambiguous404  = errors.New("S3_ABSENCE_UNCONFIRMED")
	errS3Callback      = errors.New("S3_CONSUMER_FAILED")
	errS3Unavailable   = errors.New("S3_PLATFORM_UNAVAILABLE")
)

// This is private owner-managed storage, not an operated/append-only authority.
// No remote operation or credential reader is exposed through the public ABI.
type s3Config struct {
	endpoint, region, bucket, prefix string
	access, secret, token            string
}

func printableS3Value(value string, maximum int, optional bool) bool {
	if len(value) > maximum || value == "" && !optional {
		return false
	}
	for _, ch := range value {
		if ch < 33 || ch > 126 {
			return false
		}
	}
	return true
}

func (cfg s3Config) validate() error {
	endpoint, err := url.Parse(cfg.endpoint)
	if err != nil || len(cfg.endpoint) > maxHTTPURLBytes || endpoint.Path != "" && endpoint.Path != "/" || endpoint.RawQuery != "" || endpoint.ForceQuery {
		return errS3Config
	}
	if _, err := httpAuthority(endpoint); err != nil {
		return errS3Config
	}
	if !printableS3Value(cfg.region, 64, false) || !printableS3Value(cfg.access, 256, false) || !printableS3Value(cfg.secret, 1024, false) || !printableS3Value(cfg.token, 16384, true) {
		return errS3Config
	}
	for _, ch := range cfg.region {
		if !(ch >= 'a' && ch <= 'z' || ch >= '0' && ch <= '9' || ch == '-') {
			return errS3Config
		}
	}
	if len(cfg.bucket) < 3 || len(cfg.bucket) > 63 || strings.HasPrefix(cfg.bucket, "xn--") || strings.HasSuffix(cfg.bucket, "--x-s3") || strings.HasSuffix(cfg.bucket, "-s3alias") || strings.HasSuffix(cfg.bucket, "--ol-s3") || strings.HasSuffix(cfg.bucket, "--table-s3") {
		return errS3Config
	}
	if _, err := netip.ParseAddr(cfg.bucket); err == nil {
		return errS3Config
	}
	for _, label := range strings.Split(cfg.bucket, ".") {
		if label == "" || label[0] == '-' || label[len(label)-1] == '-' {
			return errS3Config
		}
		for _, ch := range label {
			if !(ch >= 'a' && ch <= 'z' || ch >= '0' && ch <= '9' || ch == '-') {
				return errS3Config
			}
		}
	}
	if cfg.prefix == "" || len(cfg.prefix) > 512 || !utf8.ValidString(cfg.prefix) || strings.Contains(cfg.prefix, "\\") {
		return errS3Config
	}
	for _, ch := range cfg.prefix {
		if ch < 32 || ch == 127 {
			return errS3Config
		}
	}
	for _, part := range strings.Split(cfg.prefix, "/") {
		if part == "" || part == "." || part == ".." {
			return errS3Config
		}
	}
	return nil
}

type s3Backend struct {
	cfg       s3Config
	transport http.RoundTripper
	ctx       context.Context
	cancel    context.CancelFunc
	mu        sync.Mutex
	sealed    bool
	active    sync.WaitGroup
}

var _ backend.Backend = (*s3Backend)(nil)

func newS3Backend(ctx context.Context, cfg s3Config) (*s3Backend, error) {
	if ctx == nil || cfg.validate() != nil {
		return nil, errS3Config
	}
	owned, cancel := context.WithCancel(ctx)
	transport, err := newNativeHTTPTransport(owned, cfg.endpoint)
	if err != nil {
		cancel()
		return nil, errS3Config
	}
	return &s3Backend{cfg: cfg, transport: transport, ctx: owned, cancel: cancel}, nil
}

// Close is called outside method callbacks. Sealing and admission share mu, so
// Wait cannot race an Add after it observes an empty group.
func (be *s3Backend) enter(ctx context.Context) (context.Context, func(), error) {
	if ctx == nil {
		return nil, nil, errS3Config
	}
	be.mu.Lock()
	if be.sealed {
		be.mu.Unlock()
		return nil, nil, errS3Closed
	}
	be.active.Add(1)
	be.mu.Unlock()
	owned, cancel := context.WithCancel(ctx)
	cancelled := make(chan struct{})
	stop := context.AfterFunc(be.ctx, func() { cancel(); close(cancelled) })
	done := func() {
		if !stop() {
			<-cancelled
		}
		cancel()
		be.active.Done()
	}
	if be.ctx.Err() != nil {
		cancel()
	}
	return owned, done, nil
}

func (be *s3Backend) Close() error {
	be.mu.Lock()
	be.sealed = true
	be.cancel()
	be.mu.Unlock()
	be.active.Wait()
	return nil
}

func (*s3Backend) Properties() backend.Properties {
	return backend.Properties{Connections: 2, HasAtomicReplace: true}
}
func (*s3Backend) Hasher() hash.Hash         { return nil }
func (*s3Backend) IsNotExist(err error) bool { return errors.Is(err, errS3NotFound) }
func (be *s3Backend) IsPermanentError(err error) bool {
	return be.IsNotExist(err) || errors.Is(err, errS3Config) || errors.Is(err, errS3Handle) || errors.Is(err, errS3Protocol) || errors.Is(err, errS3Range) || errors.Is(err, errResourceLimit) || errors.Is(err, errS3Denied) || errors.Is(err, errS3BucketMissing)
}

func validS3ID(name string) bool {
	if len(name) != 64 {
		return false
	}
	for _, ch := range name {
		if !(ch >= '0' && ch <= '9' || ch >= 'a' && ch <= 'f') {
			return false
		}
	}
	return true
}

func (be *s3Backend) filename(handle backend.Handle) (string, error) {
	switch handle.Type {
	case backend.ConfigFile:
		if handle.Name != "" && handle.Name != strings.Repeat("0", 64) {
			return "", errS3Handle
		}
	case backend.KeyFile, backend.LockFile, backend.SnapshotFile, backend.IndexFile, backend.PackFile:
		if !validS3ID(handle.Name) {
			return "", errS3Handle
		}
	default:
		return "", errS3Handle
	}
	return layout.NewDefaultLayout(be.cfg.prefix, path.Join).Filename(handle), nil
}

func (be *s3Backend) Warmup(ctx context.Context, handles []backend.Handle) ([]backend.Handle, error) {
	owned, done, err := be.enter(ctx)
	if err != nil {
		return nil, err
	}
	defer done()
	for _, handle := range handles {
		if _, err := be.filename(handle); err != nil {
			return nil, err
		}
	}
	return nil, owned.Err()
}
func (be *s3Backend) WarmupWait(ctx context.Context, handles []backend.Handle) error {
	_, err := be.Warmup(ctx, handles)
	return err
}
func (*s3Backend) Delete(context.Context) error { return errS3Denied }
