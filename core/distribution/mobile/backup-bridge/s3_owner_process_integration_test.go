//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/restic/restic/internal/backend"
)

// The child is an actual separate copy of this test executable. The environment
// selector is compiled only with integration tests; production has no hook.
func TestS3OwnerProcessDeathHelper(t *testing.T) {
	stage := os.Getenv("SOLSTONE_S3_TEST_CHILD_STAGE")
	if stage == "" {
		t.Skip("parent-controlled subprocess only")
	}
	cfg := s3TestConfig()
	owner, err := newS3Owner(os.Getenv("SOLSTONE_S3_TEST_CHILD_ROOT"), cfg)
	if err != nil {
		t.Fatal("child owner refused", err)
	}
	if err := owner.begin(OpInit, false, cfg, []byte("process-daily-fixture"), []byte("process-recovery-fixture")); err != nil {
		t.Fatal(err)
	}
	hold := func() {
		fmt.Fprintln(os.Stdout, "S3_CHILD_AT_"+stage)
		<-time.NewTimer(time.Hour).C
		t.Fatal("parent failed to terminate child")
	}
	switch stage {
	case "intent":
		hold()
	case "save":
		base := s3TestBackend(t, cfg, s3TestTransport(func(*http.Request) (*http.Response, error) {
			hold()
			return nil, errS3IO
		}))
		wrapped, err := owner.attachBackend(base)
		if err != nil {
			t.Fatal(err)
		}
		if err := wrapped.Save(context.Background(), backend.Handle{Type: backend.KeyFile, Name: s3TestID(61)}, backend.NewByteReader([]byte("controlled opaque ciphertext"), nil)); err != nil {
			t.Fatal(err)
		}
		t.Fatal("controlled Save unexpectedly returned")
	case "completion":
		if err := owner.noteSave(""); err != nil {
			t.Fatal(err)
		}
		if err := owner.drainPrimary(); err != nil {
			t.Fatal(err)
		}
		owner.fault = func(point string) error {
			if point == "directory-sync" {
				hold()
			}
			return nil
		}
		_, _ = owner.complete()
		t.Fatal("completion passed controlled pre-sync hold")
	default:
		t.Fatal("unknown child stage")
	}
}

func TestS3OwnerProcessDeathPreservesDurableCustody(t *testing.T) {
	for _, stage := range []string{"intent", "save", "completion"} {
		t.Run(stage, func(t *testing.T) {
			cfg, root := s3TestConfig(), s3OwnerTestDirectory(t)
			cmd := exec.Command(os.Args[0], "-test.run=^TestS3OwnerProcessDeathHelper$", "-test.timeout=30s")
			cmd.Env = append(os.Environ(), "SOLSTONE_S3_TEST_CHILD_STAGE="+stage, "SOLSTONE_S3_TEST_CHILD_ROOT="+root)
			output, err := cmd.StdoutPipe()
			if err != nil {
				t.Fatal(err)
			}
			var stderr bytes.Buffer
			cmd.Stderr = &stderr
			if err := cmd.Start(); err != nil {
				t.Fatal(err)
			}
			var joined sync.Once
			var waitErr error
			stop := func() {
				joined.Do(func() {
					_ = cmd.Process.Kill()
					waitErr = cmd.Wait()
					_ = output.Close()
				})
			}
			defer stop()
			witness := make(chan string, 1)
			go func() {
				line, _ := bufio.NewReader(output).ReadString('\n')
				witness <- strings.TrimSpace(line)
			}()
			select {
			case line := <-witness:
				if line != "S3_CHILD_AT_"+stage {
					stop()
					t.Fatal("wrong child witness", line, stderr.String())
				}
			case <-time.After(15 * time.Second):
				stop()
				<-witness
				t.Fatal("child did not reach actual persistence/request stage", stderr.String())
			}
			stop()
			var exited *exec.ExitError
			if !errors.As(waitErr, &exited) {
				t.Fatal("child did not actually die", waitErr)
			}
			owner := s3OwnerTestOpen(t, root, cfg)
			if len(owner.record.Passwords) != 2 || string(owner.record.Passwords[0]) != "process-daily-fixture" || string(owner.record.Passwords[1]) != "process-recovery-fixture" || len(owner.record.Credentials) != 1 || owner.record.Credentials[0].Secret != cfg.secret {
				t.Fatal("process death lost exact credential/recovery custody")
			}
			switch stage {
			case "intent":
				if owner.record.Active == nil || owner.record.Active.MayHaveWritten {
					t.Fatal("no-egress intent changed")
				}
				if err := owner.begin(OpOpen, false, cfg, []byte("process-daily-fixture")); err != nil {
					t.Fatal("no-egress restart was not usable", err)
				}
			case "save":
				if owner.record.Active == nil || !owner.record.Active.MayHaveWritten {
					t.Fatal("admitted request lost pending state")
				}
				if err := owner.begin(OpInit, false, cfg, []byte("process-daily-fixture")); !errors.Is(err, errS3OwnerPending) {
					t.Fatal("process death cleared remote uncertainty", err)
				}
			case "completion":
				if owner.record.Active != nil || owner.record.Complete == nil {
					t.Fatal("visible known completion did not follow restart rule")
				}
			}
		})
	}
}
