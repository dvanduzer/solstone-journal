//go:build integration && (darwin || ios)

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"flag"
	"io"
	"net/url"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/minio/minio-go/v7"
	"github.com/minio/minio-go/v7/pkg/credentials"
)

var nativeHTTPFixtureFile = flag.String("native-http-credential-file", "", "explicit owned S3 scoped credential fixture for live native HTTP check")

func TestNativeHTTPLiveScopedSigning(t *testing.T) {
	if *nativeHTTPFixtureFile == "" {
		t.Skip("live native HTTP check requires an explicit owned credential fixture")
	}
	var cfg struct {
		Endpoint string `json:"endpoint"`
		Region   string `json:"region"`
		Bucket   string `json:"bucket"`
		Prefix   string `json:"prefix"`
		Access   string `json:"access_key_id"`
		Secret   string `json:"secret_access_key"`
		Token    string `json:"session_token"`
	}
	encoded, err := os.ReadFile(*nativeHTTPFixtureFile)
	if err != nil || len(encoded) > 32*1024 || json.Unmarshal(encoded, &cfg) != nil {
		t.Fatal("owned credential fixture unavailable or malformed")
	}
	zeroBytes(encoded)
	if cfg.Bucket == "" || cfg.Prefix == "" || cfg.Access == "" || cfg.Secret == "" || cfg.Region == "" {
		t.Fatal("owned credential fixture is incomplete")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	transport, err := newNativeHTTPTransport(ctx, cfg.Endpoint)
	if err != nil {
		t.Fatal("native authority refused")
	}
	endpoint, _ := url.Parse(cfg.Endpoint)
	client, err := minio.NewCore(endpoint.Host, &minio.Options{
		Creds: credentials.NewStaticV4(cfg.Access, cfg.Secret, cfg.Token), Secure: true,
		Region: cfg.Region, BucketLookup: minio.BucketLookupPath, Transport: transport, MaxRetries: 1,
	})
	if err != nil {
		t.Fatal("explicit scoped client unavailable")
	}
	nonce := make([]byte, 16)
	data := make([]byte, 64*1024)
	if _, err := rand.Read(nonce); err != nil {
		t.Fatal("fixture entropy unavailable")
	}
	if _, err := rand.Read(data); err != nil {
		t.Fatal("fixture entropy unavailable")
	}
	name := strings.TrimSuffix(cfg.Prefix, "/") + "/native-" + hex.EncodeToString(nonce) + "/signed space+percent%/sample"
	if _, err := client.PutObject(ctx, cfg.Bucket, name, bytes.NewReader(data), int64(len(data)), "", "",
		minio.PutObjectOptions{ContentType: "application/octet-stream"}); err != nil {
		t.Fatalf("native scoped PUT refused: %T", err)
	}
	reader, _, _, err := client.GetObject(ctx, cfg.Bucket, name, minio.GetObjectOptions{})
	if err != nil {
		t.Fatalf("native scoped GET refused: %T", err)
	}
	stored, readErr := io.ReadAll(io.LimitReader(reader, int64(len(data))+1))
	closeErr := reader.Close()
	if readErr != nil || closeErr != nil || !bytes.Equal(stored, data) {
		t.Fatal("native scoped readback differs or failed")
	}
	info, err := client.StatObject(ctx, cfg.Bucket, name, minio.StatObjectOptions{})
	if err != nil || info.Size != int64(len(data)) {
		t.Fatalf("native HEAD metadata refused or differs: %T", err)
	}
	digest := sha256.Sum256(data)
	t.Logf("owned native HTTP canary retained: key=%s bytes=%d sha256=%s; versions/resource cleanup remains caller-owned", name, len(data), hex.EncodeToString(digest[:]))
}
