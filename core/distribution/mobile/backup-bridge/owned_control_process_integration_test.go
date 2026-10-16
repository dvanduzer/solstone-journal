//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bufio"
	"context"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"
)

func ownedRunControlChild(t *testing.T, testName, environment, raw, proof string) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, os.Args[0], "-test.run=^"+testName+"$", "-test.timeout=55s")
	cmd.Env = append(os.Environ(), environment+"="+raw, "TMPDIR="+t.TempDir())
	var stderr, stdoutText strings.Builder
	cmd.Stderr = &stderr
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 4096), 1024*1024)
	proved := false
	for scanner.Scan() {
		if stdoutText.Len() < 256*1024 {
			stdoutText.WriteString(scanner.Text() + "\n")
		}
		if scanner.Text() == proof+" "+raw {
			proved = true
		}
	}
	scanErr := scanner.Err()
	waitErr := cmd.Wait()
	if scanErr != nil || waitErr != nil || !proved || ctx.Err() != nil || strings.Contains(stderr.String()+stdoutText.String(), "DATA RACE") {
		t.Fatal("disposable actual-native control failed", scanErr, waitErr, ctx.Err(), stderr.String(), stdoutText.String())
	}
}
