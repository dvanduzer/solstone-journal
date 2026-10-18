// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"net/http"
	"strings"
	"testing"

	"github.com/restic/restic/internal/backend"
)

// The signed host must be the Host Foundation sends: no default port, while a
// non-default port stays part of the authority.
func TestS3SignedHostOmitsOnlyTheDefaultPort(t *testing.T) {
	for _, c := range []struct{ endpoint, host string }{
		{"https://storage.example:443", "storage.example"},
		{"https://storage.example", "storage.example"},
		{"https://storage.example:8443", "storage.example:8443"},
		{"https://[2001:db8::1]:443", "[2001:db8::1]"},
	} {
		cfg := s3TestConfig()
		cfg.endpoint = c.endpoint
		var seen string
		be := s3TestBackend(t, cfg, s3TestTransport(func(req *http.Request) (*http.Response, error) {
			seen = req.URL.Host
			if req.Host != "" && req.Host != req.URL.Host || !strings.Contains(req.Header.Get("Authorization"), "SignedHeaders=host;") {
				t.Fatal("signed host differs from the sent host", c.endpoint, req.Host, req.URL.Host)
			}
			return s3TestResponse(404, "<Error><Code>NoSuchKey</Code></Error>", nil), nil
		}))
		_, _ = be.Stat(be.ctx, backend.Handle{Type: backend.ConfigFile})
		if seen != c.host {
			t.Fatal("unexpected signed host", c.endpoint, seen)
		}
	}
}
