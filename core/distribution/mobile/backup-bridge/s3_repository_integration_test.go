//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/xml"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/backend"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
	"github.com/restic/restic/internal/restorer"
	"github.com/restic/restic/internal/ui/progress"
)

// Controlled protocol storage, not a TLS/provider or independent SigV4 oracle.
// Actual provider signing and native HTTP are separately executed by the caller.
type s3MemoryPeer struct {
	cfg     s3Config
	mu      sync.Mutex
	objects map[string][]byte
}

func (peer *s3MemoryPeer) RoundTrip(req *http.Request) (*http.Response, error) {
	if err := req.Context().Err(); err != nil {
		return nil, err
	}
	if req.URL.Scheme+"://"+req.URL.Host != peer.cfg.endpoint || !strings.Contains(req.Header.Get("Authorization"), "Credential="+peer.cfg.access+"/") || req.Header.Get("X-Amz-Security-Token") != peer.cfg.token {
		return s3TestResponse(403, "<Error><Code>AccessDenied</Code></Error>", nil), nil
	}
	peer.mu.Lock()
	defer peer.mu.Unlock()
	bucketPath := "/" + peer.cfg.bucket
	if req.Method == http.MethodGet && req.URL.Path == bucketPath && req.URL.Query().Get("list-type") == "2" {
		prefix := req.URL.Query().Get("prefix")
		var keys []string
		for key := range peer.objects {
			if strings.HasPrefix(key, prefix) {
				keys = append(keys, key)
			}
		}
		sort.Strings(keys)
		start := 0
		if token := req.URL.Query().Get("continuation-token"); token != "" {
			n, err := strconv.Atoi(token)
			if err != nil || n < 0 || n > len(keys) {
				return s3TestResponse(400, "<Error><Code>InvalidToken</Code></Error>", nil), nil
			}
			start = n
		}
		end := min(start+s3PageKeys, len(keys))
		var out bytes.Buffer
		out.WriteString("<ListBucketResult><Name>")
		_ = xml.EscapeText(&out, []byte(peer.cfg.bucket))
		out.WriteString("</Name><Prefix>")
		_ = xml.EscapeText(&out, []byte(prefix))
		fmt.Fprintf(&out, "</Prefix><KeyCount>%d</KeyCount><MaxKeys>%d</MaxKeys><IsTruncated>%t</IsTruncated>", end-start, s3PageKeys, end < len(keys))
		if end < len(keys) {
			fmt.Fprintf(&out, "<NextContinuationToken>%d</NextContinuationToken>", end)
		}
		for _, key := range keys[start:end] {
			out.WriteString("<Contents><Key>")
			_ = xml.EscapeText(&out, []byte(key))
			fmt.Fprintf(&out, "</Key><Size>%d</Size></Contents>", len(peer.objects[key]))
		}
		out.WriteString("</ListBucketResult>")
		return s3TestResponse(200, out.String(), nil), nil
	}
	if !strings.HasPrefix(req.URL.Path, bucketPath+"/") {
		return s3TestResponse(404, "<Error><Code>NoSuchBucket</Code></Error>", nil), nil
	}
	key := strings.TrimPrefix(req.URL.Path, bucketPath+"/")
	switch req.Method {
	case http.MethodPut:
		value, err := io.ReadAll(req.Body)
		if err != nil {
			return nil, err
		}
		peer.objects[key] = append([]byte(nil), value...)
		return s3TestResponse(200, "", nil), nil
	case http.MethodDelete:
		delete(peer.objects, key)
		return s3TestResponse(204, "", nil), nil
	case http.MethodHead, http.MethodGet:
		value, ok := peer.objects[key]
		if !ok {
			if req.Method == http.MethodHead {
				return s3TestResponse(404, "", http.Header{"Content-Length": {"137"}}), nil
			}
			return s3TestResponse(404, "<Error><Code>NoSuchKey</Code></Error>", nil), nil
		}
		if req.Method == http.MethodHead {
			return s3TestResponse(200, "", http.Header{"Content-Length": {strconv.Itoa(len(value))}}), nil
		}
		if header := req.Header.Get("Range"); header != "" {
			startText, endText, ok := strings.Cut(strings.TrimPrefix(header, "bytes="), "-")
			start, e1 := strconv.Atoi(startText)
			end := len(value) - 1
			var e2 error
			if endText != "" {
				end, e2 = strconv.Atoi(endText)
			}
			if !ok || e1 != nil || e2 != nil || start < 0 || end < start || end >= len(value) {
				return s3TestResponse(416, "", nil), nil
			}
			return s3TestResponse(206, string(value[start:end+1]), http.Header{"Content-Range": {fmt.Sprintf("bytes %d-%d/%d", start, end, len(value))}}), nil
		}
		return s3TestResponse(200, string(value), nil), nil
	}
	return s3TestResponse(400, "<Error><Code>InvalidRequest</Code></Error>", nil), nil
}

func TestS3EncryptedResticRepositoryRoundTrip(t *testing.T) {
	ctx := context.Background()
	cfg := s3TestConfig()
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	open := func(password string) (*repository.Repository, *s3Backend, restic.ID, error) {
		be := s3TestBackend(t, cfg, peer)
		repo, err := repository.New(guardedBackend{be}, repository.Options{})
		if err != nil {
			return nil, be, restic.ID{}, err
		}
		id, err := safeScanKey(ctx, repo, password)
		return repo, be, id, err
	}
	be := s3TestBackend(t, cfg, peer)
	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	daily, recovery := "daily-repository-fixture", "independent-recovery-fixture"
	err = func() error {
		addKeyMu.Lock()
		defer addKeyMu.Unlock()
		return repo.Init(ctx, restic.StableRepoVersion, daily, nil)
	}()
	if err != nil {
		t.Fatal("encrypted S3 init failed", err)
	}
	dailyID := repo.KeyID()
	repo, _, openedID, err := open(daily)
	if err != nil || openedID != dailyID {
		t.Fatal("independent daily open failed")
	}
	newKey, err := func() (*repository.Key, error) {
		addKeyMu.Lock()
		defer addKeyMu.Unlock()
		return repository.AddKey(ctx, repo, recovery, "solstone", "solstone", repo.Key())
	}()
	if err != nil {
		t.Fatal("recovery add failed", err)
	}
	recoveryRepo, _, recoveryID, err := open(recovery)
	if err != nil || recoveryID != newKey.ID() || recoveryID == dailyID {
		t.Fatal("independent recovery reopen failed")
	}
	if _, _, _, err := open("wrong-password"); err == nil {
		t.Fatal("wrong password opened repository")
	}
	before := len(peer.objects)
	if err := repo.Init(ctx, restic.StableRepoVersion, "replacement-password", nil); err == nil || len(peer.objects) != before {
		t.Fatal("existing repository was initialized again")
	}
	source := t.TempDir()
	plain := []byte("private journal payload must survive encrypted S3 storage")
	if err := os.WriteFile(filepath.Join(source, "journal.txt"), plain, 0600); err != nil {
		t.Fatal(err)
	}
	sourceRoot, err := os.OpenRoot(source)
	if err != nil {
		t.Fatal(err)
	}
	defer sourceRoot.Close()
	arch := archiver.New(repo, newRootFS(sourceRoot, "source", nil), archiver.Options{})
	arch.Error = func(_ string, err error) error { return err }
	_, snapshotID, _, err := arch.Snapshot(ctx, []string{"source"}, archiver.SnapshotOptions{Hostname: "solstone", Time: time.Now()})
	if err != nil || snapshotID.IsNull() {
		t.Fatal("encrypted snapshot failed", err)
	}
	if err := recoveryRepo.LoadIndex(ctx, &progress.NoopPrinter{}); err != nil {
		t.Fatal("independent recovery index load failed", err)
	}
	snapshot, err := data.LoadSnapshot(ctx, recoveryRepo, snapshotID)
	if err != nil {
		t.Fatal("encrypted snapshot reopen failed", err)
	}
	destination := t.TempDir()
	root, err := os.OpenRoot(destination)
	if err != nil {
		t.Fatal(err)
	}
	defer root.Close()
	restore := restorer.NewRestorer(recoveryRepo, snapshot, restorer.Options{})
	if _, err := restore.RestoreToRoot(ctx, root); err != nil {
		t.Fatal("independent encrypted restore failed", err)
	}
	found := false
	err = filepath.Walk(destination, func(name string, info os.FileInfo, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if info.Name() == "journal.txt" {
			restored, err := os.ReadFile(name)
			if err != nil || !bytes.Equal(restored, plain) {
				return errS3Protocol
			}
			found = true
		}
		return nil
	})
	if err != nil || !found {
		t.Fatal("restored bytes absent or changed")
	}
	var ids []string
	if err := recoveryRepo.List(ctx, restic.SnapshotFile, func(id restic.ID, _ int64) error { ids = append(ids, id.String()); return nil }); err != nil || len(ids) != 1 || ids[0] != snapshotID.String() {
		t.Fatal("real encrypted snapshot listing differs")
	}
	for key, value := range peer.objects {
		if bytes.Contains(value, plain) {
			t.Fatal("plaintext reached object storage")
		}
		if strings.Contains(key, "/data/") {
			digest := sha256.Sum256(value)
			if !strings.HasSuffix(key, hex.EncodeToString(digest[:])) {
				t.Fatal("pack content address changed")
			}
		}
	}
	// Exercise encrypted lock compatibility, observing the actual controlled
	// object removal. This is not remote-lock error/uncertainty qualification.
	unlock, _, err := lockRepo(ctx, repo, false)
	if err != nil {
		t.Fatal("real restic lock failed", err)
	}
	lockCount := func() int {
		peer.mu.Lock()
		defer peer.mu.Unlock()
		count := 0
		for key := range peer.objects {
			if strings.HasPrefix(key, cfg.prefix+"/locks/") {
				count++
			}
		}
		return count
	}
	if lockCount() != 1 {
		unlock()
		t.Fatal("encrypted lock object absent")
	}
	unlock()
	if lockCount() != 0 {
		t.Fatal("acknowledged controlled lock remained")
	}
	wrongCfg := cfg
	wrongCfg.access = "invalid-fixture-access"
	wrong := s3TestBackend(t, wrongCfg, peer)
	if _, err := wrong.Stat(ctx, backend.Handle{Type: backend.ConfigFile}); !wrong.IsPermanentError(err) || wrong.IsNotExist(err) {
		t.Fatal("wrong credential became repository absence")
	}
}
