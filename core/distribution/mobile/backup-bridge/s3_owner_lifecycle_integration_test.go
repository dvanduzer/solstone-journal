//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/restic/restic/internal/backend"
	"golang.org/x/sys/unix"
)

func TestS3OwnerDrainIncludesWrapperOutcomeBookkeeping(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if owner.begin(OpInit, cfg, []byte("fixture-password")) != nil {
		t.Fatal("setup failed")
	}
	base := s3TestBackend(t, cfg, s3TestTransport(func(*http.Request) (*http.Response, error) { return nil, errS3IO }))
	wrapped, err := owner.attachBackend(base)
	if err != nil {
		t.Fatal(err)
	}
	entered, release, stopped, drained := make(chan struct{}), make(chan struct{}), make(chan struct{}), make(chan struct{})
	var once sync.Once
	drainStarted := false
	wrapped.afterSave = func() { close(entered); <-release }
	go func() {
		defer close(stopped)
		_ = wrapped.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(1)}, backend.NewByteReader([]byte("ciphertext"), nil))
	}()
	defer func() {
		once.Do(func() { close(release) })
		awaitS3Test(t, stopped)
		if drainStarted {
			awaitS3Test(t, drained)
		}
	}()
	awaitS3Test(t, entered)
	drainStarted = true
	go func() { _ = owner.drainPrimary(); close(drained) }()
	assertS3Pending(t, drained)
	if !errors.Is(owner.Close(), errS3OwnerBusy) {
		t.Fatal("owner released while outcome bookkeeping was blocked")
	}
	replacement := cfg
	replacement.secret = "replacement-secret"
	if other, err := newS3Owner(s3OwnerTestDirectory(t), replacement); !errors.Is(err, errS3OwnerBusy) || other != nil {
		if other != nil {
			_ = other.Close()
		}
		t.Fatal("replacement bypassed pending wrapper")
	}
	once.Do(func() { close(release) })
	awaitS3Test(t, stopped)
	awaitS3Test(t, drained)
	if visible, err := owner.complete(); visible || !errors.Is(err, errS3OwnerPending) {
		t.Fatal("lost outcome bookkeeping became READY")
	}
}

func TestS3OwnerReplacedTempFIFORefusesWithoutWriter(t *testing.T) {
	cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
	owner := s3OwnerTestOpen(t, root, cfg)
	if owner.begin(OpInit, cfg, []byte("fixture-password")) != nil {
		t.Fatal("setup failed")
	}
	prepared := make(chan string, 1)
	owner.fault = func(stage string) error {
		if stage != "after-file-sync" {
			return nil
		}
		entries, err := os.ReadDir(root)
		if err != nil {
			return err
		}
		for _, entry := range entries {
			if !strings.HasPrefix(entry.Name(), s3OwnerTempPrefix) {
				continue
			}
			path := filepath.Join(root, entry.Name())
			if err := os.Remove(path); err != nil {
				return err
			}
			if err := unix.Mkfifo(path, 0600); err != nil {
				return err
			}
			prepared <- path
			return nil
		}
		return errS3OwnerIO
	}
	stopped := make(chan struct{})
	var operationErr error
	go func() { defer close(stopped); operationErr = owner.noteSave("") }()
	var fifo string
	select {
	case fifo = <-prepared:
	case <-stopped:
		t.Fatal("fixture never replaced temp")
	case <-time.After(5 * time.Second):
		t.Fatal("fixture preparation did not return")
	}
	defer func() {
		fd, err := unix.Open(fifo, unix.O_WRONLY|unix.O_NONBLOCK|unix.O_CLOEXEC, 0)
		if err == nil {
			_ = unix.Close(fd)
		}
		awaitS3Test(t, stopped)
	}()
	deadline := time.NewTimer(time.Second)
	defer deadline.Stop()
	tick := time.NewTicker(time.Millisecond)
	defer tick.Stop()
	for {
		select {
		case <-stopped:
			if operationErr == nil {
				t.Fatal("FIFO identity admitted")
			}
			return
		case <-deadline.C:
			t.Fatal("FIFO identity needed an external writer")
		case <-tick.C:
			fd, err := unix.Open(fifo, unix.O_WRONLY|unix.O_NONBLOCK|unix.O_CLOEXEC, 0)
			if err == nil {
				_ = unix.Close(fd)
				awaitS3Test(t, stopped)
				t.Fatal("identity open waited for an external FIFO writer")
			}
			if !errors.Is(err, unix.ENXIO) {
				t.Fatal("fixture witness open failed")
			}
		}
	}
}
