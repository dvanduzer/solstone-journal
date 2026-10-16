//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"sync/atomic"
	"testing"
	"time"
)

func TestOwnedPublicPreValidationCapAndRetirement(t *testing.T) {
	const expectedReservations = 16
	f := newRootedTestFixture(t, false)
	input := sourceCInput{repository: f.repo, password: f.password}
	type reservation struct {
		entry   *ownedCall
		proceed chan struct{}
	}
	reserved := make(chan reservation, expectedReservations)
	var sequence, copies, opens, workers atomic.Int32
	testOwnedStage = func(stage string, entry *ownedCall) {
		switch stage {
		case "reserved":
			if sequence.Add(1) <= expectedReservations {
				proceed := make(chan struct{})
				reserved <- reservation{entry, proceed}
				<-proceed
			}
		case "copied":
			copies.Add(1)
		case "resource-open":
			opens.Add(1)
		}
	}
	testGate = func(stage string, _ context.Context) error {
		if stage == "before-restic" {
			workers.Add(1)
		}
		return nil
	}
	t.Cleanup(func() { testOwnedStage = nil; testGate = nil })
	type started struct {
		id     uint64
		status int32
	}
	results := make(chan started, expectedReservations)
	for n := 0; n < expectedReservations; n++ {
		go func() {
			id, status := sourceCVersionedStart(input, OpOpen, false, -1, nil, 0)
			results <- started{id, status}
		}()
	}
	var all []reservation
	for n := 0; n < expectedReservations; n++ {
		select {
		case r := <-reserved:
			all = append(all, r)
		case <-time.After(5 * time.Second):
			t.Fatal("actual public entries did not saturate pre-validation reservations")
		}
	}
	handlesMu.Lock()
	handleCount := len(handles)
	handlesMu.Unlock()
	ownedCalls.Lock()
	count := len(ownedCalls.live)
	ownedCalls.Unlock()
	if handleCount != 0 || count != expectedReservations || copies.Load() != 0 || opens.Load() != 0 || workers.Load() != 0 {
		t.Fatal("control reached old running/handle cap or credential/resource work", handleCount, count, copies.Load(), opens.Load(), workers.Load())
	}
	id, status := sourceCVersionedStart(input, OpOpen, false, -1, nil, 0)
	if status != StatusBusy || id != 0 || copies.Load() != 0 || opens.Load() != 0 || workers.Load() != 0 {
		if id != 0 {
			_ = sourceCCancel(id)
			_, _, _ = sourceCWaitAndFree(id)
		}
		t.Fatal("full owned reservation cap did not refuse before work", status, id)
	}
	close(all[0].proceed)
	var first started
	select {
	case first = <-results:
	case <-time.After(5 * time.Second):
		t.Fatal("released healthy reservation did not finish admission")
	}
	if first.status != StatusOk || first.id == 0 {
		t.Fatal("released healthy public entry refused", first.status, first.id)
	}
	result, waited, freed := sourceCWaitAndFree(first.id)
	if waited != StatusOk || freed != StatusOk || result.status != StatusOk {
		t.Fatal("real released public open failed", result.status, result.diagnostic, waited, freed)
	}
	ownedCalls.Lock()
	count = len(ownedCalls.live)
	ownedCalls.Unlock()
	if count != expectedReservations-1 {
		t.Fatal("actual healthy Free did not retire reservation", count)
	}
	id, status = sourceCVersionedStart(input, OpOpen, false, -1, nil, 0)
	if status != StatusOk || id == 0 {
		t.Fatal("valid twin after actual retirement refused", status, id)
	}
	result, waited, freed = sourceCWaitAndFree(id)
	if waited != StatusOk || freed != StatusOk || result.status != StatusOk || copies.Load() < 2 || opens.Load() < 2 || workers.Load() < 2 {
		t.Fatal("post-retirement twin did not calibrate actual work", result.status, waited, freed, copies.Load(), opens.Load(), workers.Load())
	}
	for _, r := range all[1:] {
		close(r.proceed)
	}
	for range all[1:] {
		var done started
		select {
		case done = <-results:
		case <-time.After(10 * time.Second):
			t.Fatal("remaining actual admission did not retire")
		}
		if done.status == StatusOk && done.id != 0 {
			result, waited, freed = sourceCWaitAndFree(done.id)
			if waited != StatusOk || freed != StatusOk || result.status != StatusOk {
				t.Fatal("remaining actual open failed", result.status, waited, freed)
			}
		} else if done.status != StatusBusy || done.id != 0 {
			t.Fatal("remaining admission changed classification", done.status, done.id)
		}
	}
	ownedCalls.Lock()
	count = len(ownedCalls.live)
	sealed := ownedCalls.sealed
	ownedCalls.Unlock()
	handlesMu.Lock()
	handleCount = len(handles)
	handlesMu.Unlock()
	if count != 0 || sealed || handleCount != 0 {
		t.Fatal("healthy cap controls leaked ownership or sealed", count, sealed, handleCount)
	}
}
