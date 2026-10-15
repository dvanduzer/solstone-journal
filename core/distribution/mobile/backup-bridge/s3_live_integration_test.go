//go:build integration && (darwin || ios)

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"io"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/restic/restic/internal/backend"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
)

var s3BackendFixtureFile = flag.String("s3-backend-credential-file", "", "explicit owned disposable S3 fixture with passwords retained before egress")

func TestS3LiveNativeRepositoryAndPagination(t *testing.T) {
	if *s3BackendFixtureFile == "" {
		t.Skip("live native S3 check requires explicit owned fixture")
	}
	info, err := os.Stat(*s3BackendFixtureFile)
	if err != nil || !info.Mode().IsRegular() || info.Mode().Perm() != 0600 || info.Size() > 32768 {
		t.Fatal("private owned fixture is unavailable")
	}
	encoded, err := os.ReadFile(*s3BackendFixtureFile)
	if err != nil {
		t.Fatal("private fixture read failed")
	}
	defer zeroBytes(encoded)
	var fixture struct {
		Endpoint   string `json:"endpoint"`
		Region     string `json:"region"`
		Bucket     string `json:"bucket"`
		Prefix     string `json:"repository_prefix"`
		Pagination string `json:"pagination_prefix"`
		Access     string `json:"access_key_id"`
		Secret     string `json:"secret_access_key"`
		Token      string `json:"session_token"`
		Daily      string `json:"daily_password"`
		Recovery   string `json:"recovery_password"`
	}
	if json.Unmarshal(encoded, &fixture) != nil || len(fixture.Daily) < 32 || len(fixture.Recovery) < 32 || fixture.Daily == fixture.Recovery {
		t.Fatal("durable credential/password fixture is incomplete")
	}
	cfg := s3Config{endpoint: fixture.Endpoint, region: fixture.Region, bucket: fixture.Bucket, prefix: fixture.Prefix, access: fixture.Access, secret: fixture.Secret, token: fixture.Token}
	if cfg.validate() != nil {
		t.Fatal("repository configuration refused")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()
	open := func(password string) (*repository.Repository, *s3Backend, restic.ID, error) {
		be, err := newS3Backend(ctx, cfg)
		if err != nil {
			return nil, nil, restic.ID{}, err
		}
		t.Cleanup(func() { _ = be.Close() })
		repo, err := repository.New(guardedBackend{be}, repository.Options{})
		if err != nil {
			return nil, be, restic.ID{}, err
		}
		id, err := safeScanKey(ctx, repo, password)
		return repo, be, id, err
	}
	be, err := newS3Backend(ctx, cfg)
	if err != nil {
		t.Fatal("native backend unavailable")
	}
	defer be.Close()
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal("restic repository construction failed")
	}
	err = func() error {
		addKeyMu.Lock()
		defer addKeyMu.Unlock()
		return repo.Init(ctx, restic.StableRepoVersion, fixture.Daily, nil)
	}()
	if err != nil {
		t.Fatalf("live encrypted initialization failed: %T", err)
	}
	dailyID := repo.KeyID()
	repo, _, openedID, err := open(fixture.Daily)
	if err != nil || openedID != dailyID {
		t.Fatal("live independent daily open failed")
	}
	key, err := func() (*repository.Key, error) {
		addKeyMu.Lock()
		defer addKeyMu.Unlock()
		return repository.AddKey(ctx, repo, fixture.Recovery, "solstone", "solstone", repo.Key())
	}()
	if err != nil {
		t.Fatal("live recovery-key add failed")
	}
	_, _, recoveryID, err := open(fixture.Recovery)
	if err != nil || recoveryID != key.ID() || recoveryID == dailyID {
		t.Fatal("live independent recovery open failed")
	}
	if _, _, _, err := open("wrong-password-control"); err == nil {
		t.Fatal("live wrong-password control opened repository")
	}
	if err := repo.Init(ctx, restic.StableRepoVersion, "wrong-reinitialization-control", nil); err == nil {
		t.Fatal("existing live repository was reinitialized")
	}
	if _, _, id, err := open(fixture.Daily); err != nil || id != dailyID {
		t.Fatal("refused initialization changed original key")
	}
	if _, _, id, err := open(fixture.Recovery); err != nil || id != recoveryID {
		t.Fatal("refused initialization changed recovery key")
	}
	var ciphertext []byte
	configHandle := backend.Handle{Type: backend.ConfigFile}
	if err := be.Load(ctx, configHandle, 0, 0, func(reader io.Reader) error { var err error; ciphertext, err = io.ReadAll(reader); return err }); err != nil || len(ciphertext) < 16 {
		t.Fatal("live encrypted config read failed")
	}
	defer zeroBytes(ciphertext)
	if err := be.Load(ctx, configHandle, 7, 3, func(reader io.Reader) error {
		data, err := io.ReadAll(reader)
		if err != nil || !bytes.Equal(data, ciphertext[3:10]) {
			return errS3Protocol
		}
		return nil
	}); err != nil {
		t.Fatal("live exact ciphertext range failed")
	}
	invalid := cfg
	invalid.access = "invalid-disposable-access"
	invalidBackend, err := newS3Backend(ctx, invalid)
	if err != nil {
		t.Fatal("invalid credential control could not start")
	}
	defer invalidBackend.Close()
	if _, err := invalidBackend.Stat(ctx, configHandle); !errors.Is(err, errS3Denied) || invalidBackend.IsNotExist(err) {
		t.Fatal("live invalid credential was not an access refusal")
	}
	pagination := cfg
	pagination.prefix = fixture.Pagination
	if pagination.validate() != nil || pagination.prefix == cfg.prefix || strings.HasPrefix(pagination.prefix, cfg.prefix+"/") || strings.HasPrefix(cfg.prefix, pagination.prefix+"/") {
		t.Fatal("distinct owned pagination prefix is required")
	}
	pages, err := newS3Backend(ctx, pagination)
	if err != nil {
		t.Fatal("native pagination backend unavailable")
	}
	defer pages.Close()
	payload := sha256.Sum256([]byte("opaque storage protocol pagination control"))
	for n := 0; n < s3PageKeys+1; n++ {
		name := sha256.Sum256([]byte(fixture.Pagination + "/" + string(rune(n))))
		if err := pages.Save(ctx, backend.Handle{Type: backend.PackFile, Name: hex.EncodeToString(name[:])}, backend.NewByteReader(payload[:], nil)); err != nil {
			t.Fatalf("live pagination PUT failed at ordinal %d", n)
		}
	}
	count := 0
	if err := pages.List(ctx, backend.PackFile, func(info backend.FileInfo) error {
		count++
		if info.Size != int64(len(payload)) {
			return errS3Protocol
		}
		return nil
	}); err != nil || count != s3PageKeys+1 {
		t.Fatal("live multi-page listing differed or failed")
	}
	digest := sha256.Sum256(ciphertext)
	t.Logf("native S3 backend retained repository=%s pagination=%s daily_key=%s recovery_key=%s config_sha256=%s; pagination population constructed as page limit+1; passwords/credentials remain in explicit private fixture; no public operation or lock/rotation qualification", cfg.prefix, pagination.prefix, dailyID.String(), recoveryID.String(), hex.EncodeToString(digest[:]))
}
