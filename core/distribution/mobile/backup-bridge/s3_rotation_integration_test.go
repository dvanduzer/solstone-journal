//go:build integration

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/restic/restic/internal/backend"
	"github.com/restic/restic/internal/repository"
)

func countS3Keys(peer *s3MemoryPeer) int {
	peer.mu.Lock()
	defer peer.mu.Unlock()
	count := 0
	for key := range peer.objects {
		if strings.Contains(key, "/keys/") {
			count++
		}
	}
	return count
}

func TestS3RotationNormalRetry(t *testing.T) {
	cfg := s3TestConfig()
	state, workspace := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}

	daily := []byte("daily-password-normal-retry")
	candidateA := []byte("recovery-candidate-A")
	candidateB := []byte("recovery-candidate-B")

	// 1. Init repository
	initReq := bridgeRequest{operation: OpInit, password: daily}
	id, handle := s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	initRes := s3TestWaitOperation(t, id, handle)
	if initRes.status != StatusOk || !validS3ID(initRes.keyID) {
		t.Fatalf("init failed: status=%d diag=%s", initRes.status, initRes.diagnostic)
	}

	// 2. Add candidate A, interrupting after the key PUT before complete
	var keyPutDone atomic.Bool
	var failVerification atomic.Bool
	failVerification.Store(true)

	interruptedTransport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method == http.MethodPut && strings.Contains(req.URL.Path, "/keys/") {
			resp, err := peer.RoundTrip(req)
			if err == nil && resp.StatusCode == http.StatusOK {
				keyPutDone.Store(true)
			}
			return resp, err
		}
		if keyPutDone.Load() && failVerification.Load() {
			return &http.Response{
				StatusCode: http.StatusInternalServerError,
				Body:       io.NopCloser(bytes.NewReader([]byte("synthetic verification failure"))),
			}, nil
		}
		return peer.RoundTrip(req)
	})

	addAReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidateA,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, interruptedTransport, addAReq)
	addAInterruptedRes := s3TestWaitOperation(t, id, handle)
	if addAInterruptedRes.status != StatusRemotePending || addAInterruptedRes.diagnostic != DiagRemotePending {
		t.Fatalf("expected remote pending, got status=%d diag=%s", addAInterruptedRes.status, addAInterruptedRes.diagnostic)
	}
	if !keyPutDone.Load() {
		t.Fatal("key PUT was not observed")
	}

	// Owner must still have MayHaveWritten add with candidate A
	owner := s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || owner.record.Active.Operation != OpAddRecoveryKey ||
		!owner.record.Active.MayHaveWritten || !bytes.Equal(owner.record.Active.RecoveryPassword, candidateA) ||
		owner.record.Active.KeyID == "" {
		t.Fatalf("owner active add state invalid: %+v", owner.record.Active)
	}
	candidateAKeyID := owner.record.Active.KeyID
	s3OwnerTestClose(t, owner)

	// Key object must exist in S3 peer
	peer.mu.Lock()
	if _, ok := peer.objects[cfg.prefix+"/keys/"+candidateAKeyID]; !ok {
		peer.mu.Unlock()
		t.Fatal("candidate A key object missing from peer")
	}
	peer.mu.Unlock()
	keysBefore := countS3Keys(peer)

	// 3. List with the daily password preserves that add
	failVerification.Store(false)
	listReq := bridgeRequest{
		operation: OpListRecoveryKeys,
		password:  daily,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, listReq)
	listRes := s3TestWaitOperation(t, id, handle)
	if listRes.status != StatusOk {
		t.Fatalf("list failed: status=%d diag=%s", listRes.status, listRes.diagnostic)
	}

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || owner.record.Active.Operation != OpAddRecoveryKey ||
		!owner.record.Active.MayHaveWritten || !bytes.Equal(owner.record.Active.RecoveryPassword, candidateA) ||
		owner.record.Active.KeyID != candidateAKeyID {
		t.Fatalf("list did not preserve active add: %+v", owner.record.Active)
	}
	s3OwnerTestClose(t, owner)

	// 4. Add with different candidate B returns non-OK, does not create a second key,
	// leaves Active on A with A's password and id, and result metadata is the resume document containing A not B.
	addBReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidateB,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addBReq)
	addBRes := s3TestWaitOperation(t, id, handle)
	if addBRes.status != StatusRemotePending || addBRes.diagnostic != DiagRemotePending {
		t.Fatalf("expected remote pending for mismatch candidate B, got status=%d diag=%s", addBRes.status, addBRes.diagnostic)
	}
	if len(addBRes.metadata) == 0 {
		t.Fatal("expected resume metadata on candidate mismatch, got empty")
	}

	type resumeDocument struct {
		Schema           string `json:"schema"`
		Kind             string `json:"kind"`
		RecoveryPassword string `json:"recovery_password"`
		KeyID            string `json:"key_id"`
	}
	var doc resumeDocument
	if err := json.Unmarshal(addBRes.metadata, &doc); err != nil {
		t.Fatalf("unmarshal resume document failed: %v", err)
	}
	if doc.Schema != "solstone.backup-metadata.v1" {
		t.Fatalf("unexpected schema: %s", doc.Schema)
	}
	if doc.Kind != "recovery-add-recorded" {
		t.Fatalf("unexpected kind: %s", doc.Kind)
	}
	if doc.RecoveryPassword != string(candidateA) {
		t.Fatalf("expected recorded password %s (candidate A), got %s", string(candidateA), doc.RecoveryPassword)
	}
	if doc.KeyID != candidateAKeyID {
		t.Fatalf("expected recorded key id %s, got %s", candidateAKeyID, doc.KeyID)
	}

	if countS3Keys(peer) != keysBefore {
		t.Fatalf("second key was created during candidate B add: before=%d now=%d", keysBefore, countS3Keys(peer))
	}

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || owner.record.Active.Operation != OpAddRecoveryKey ||
		!owner.record.Active.MayHaveWritten || !bytes.Equal(owner.record.Active.RecoveryPassword, candidateA) ||
		owner.record.Active.KeyID != candidateAKeyID {
		t.Fatalf("owner active add modified after candidate B refusal: %+v", owner.record.Active)
	}
	s3OwnerTestClose(t, owner)

	// 5. Add again with A returns OK for the existing id, does not create a second key,
	// and only then settles the add. Passwords still contain the daily password and A, not B.
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addAReq)
	addARes := s3TestWaitOperation(t, id, handle)
	if addARes.status != StatusOk || addARes.keyID != candidateAKeyID {
		t.Fatalf("add retry with candidate A failed: status=%d diag=%s keyID=%s expected=%s",
			addARes.status, addARes.diagnostic, addARes.keyID, candidateAKeyID)
	}

	if countS3Keys(peer) != keysBefore {
		t.Fatalf("extra key was created during settled add: before=%d now=%d", keysBefore, countS3Keys(peer))
	}

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active != nil {
		t.Fatalf("add was not settled, Active still present: %+v", owner.record.Active)
	}
	if len(owner.record.Passwords) != 2 {
		t.Fatalf("expected exactly 2 passwords (daily + A), got %d", len(owner.record.Passwords))
	}
	hasDaily, hasA, hasB := false, false, false
	for _, p := range owner.record.Passwords {
		if bytes.Equal(p, daily) {
			hasDaily = true
		}
		if bytes.Equal(p, candidateA) {
			hasA = true
		}
		if bytes.Equal(p, candidateB) {
			hasB = true
		}
	}
	if !hasDaily || !hasA || hasB {
		t.Fatalf("passwords mismatch: hasDaily=%v hasA=%v hasB=%v", hasDaily, hasA, hasB)
	}
	s3OwnerTestClose(t, owner)
}

func TestS3RotationPrePutAndAckLost(t *testing.T) {
	cfg := s3TestConfig()
	state, workspace := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}

	daily := []byte("daily-password-preput")
	candidatePrePut := []byte("recovery-candidate-preput")

	// 1. Init repository
	initReq := bridgeRequest{operation: OpInit, password: daily}
	id, handle := s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	initRes := s3TestWaitOperation(t, id, handle)
	if initRes.status != StatusOk {
		t.Fatalf("init failed: status=%d diag=%s", initRes.status, initRes.diagnostic)
	}

	// 2. Pre-PUT: recorded password, no key object, no persisted id
	owner := s3OwnerTestOpen(t, state, cfg)
	if err := owner.begin(OpAddRecoveryKey, true, cfg, daily, candidatePrePut); err != nil {
		t.Fatal(err)
	}
	if err := owner.noteSave(""); err != nil {
		t.Fatal(err)
	}
	if owner.record.Active == nil || owner.record.Active.Operation != OpAddRecoveryKey ||
		!owner.record.Active.MayHaveWritten || !bytes.Equal(owner.record.Active.RecoveryPassword, candidatePrePut) ||
		owner.record.Active.KeyID != "" {
		t.Fatalf("unexpected pre-put plant state: %+v", owner.record.Active)
	}
	s3OwnerTestClose(t, owner)

	// Retry with that same password replays AddKey and persists the id only after Save
	retryReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidatePrePut,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, retryReq)
	retryRes := s3TestWaitOperation(t, id, handle)
	if retryRes.status != StatusOk || !validS3ID(retryRes.keyID) {
		t.Fatalf("pre-put retry failed: status=%d diag=%s keyID=%s", retryRes.status, retryRes.diagnostic, retryRes.keyID)
	}

	peer.mu.Lock()
	if _, ok := peer.objects[cfg.prefix+"/keys/"+retryRes.keyID]; !ok {
		peer.mu.Unlock()
		t.Fatal("persisted key object missing after pre-put retry")
	}
	peer.mu.Unlock()

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active != nil {
		t.Fatal("active add not settled after pre-put retry")
	}
	s3OwnerTestClose(t, owner)

	state, workspace = s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer = &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	if r := s3TestWaitOperation(t, id, handle); r.status != StatusOk {
		t.Fatalf("independent uncertainty fixture init failed: %d %s", r.status, r.diagnostic)
	}

	// 3. Planted ID with no ciphertext stays non-OK, creates no object, keeps recorded ID
	candidateAckLost := []byte("recovery-candidate-acklost")
	missingRecordedID := s3TestID(88)

	owner = s3OwnerTestOpen(t, state, cfg)
	if err := owner.begin(OpAddRecoveryKey, true, cfg, daily, candidateAckLost); err != nil {
		t.Fatal(err)
	}
	if err := owner.noteSave(""); err != nil {
		t.Fatal(err)
	}
	if err := owner.recordKeyID(missingRecordedID); err != nil {
		t.Fatal(err)
	}
	s3OwnerTestClose(t, owner)

	// Confirm missingRecordedID does NOT exist in S3 peer
	peer.mu.Lock()
	delete(peer.objects, cfg.prefix+"/keys/"+missingRecordedID)
	peer.mu.Unlock()

	ackLostReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidateAckLost,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, ackLostReq)
	ackLostRes := s3TestWaitOperation(t, id, handle)
	if ackLostRes.status == StatusOk {
		t.Fatalf("expected non-OK for recorded ID without ciphertext, got StatusOk")
	}

	peer.mu.Lock()
	if _, ok := peer.objects[cfg.prefix+"/keys/"+missingRecordedID]; ok {
		peer.mu.Unlock()
		t.Fatal("missing key object unexpectedly created in peer")
	}
	peer.mu.Unlock()

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || owner.record.Active.KeyID != missingRecordedID {
		t.Fatalf("active recorded ID was altered or cleared: %+v", owner.record.Active)
	}
	s3OwnerTestClose(t, owner)

	state, workspace = s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer = &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	if r := s3TestWaitOperation(t, id, handle); r.status != StatusOk {
		t.Fatalf("independent uncertainty fixture init failed: %d %s", r.status, r.diagnostic)
	}

	// 4. Listing or open that fails is not treated as absence and does not clear the add
	candidateFail := []byte("recovery-candidate-fail")
	failKeyID := s3TestID(89)

	owner = s3OwnerTestOpen(t, state, cfg)
	if err := owner.begin(OpAddRecoveryKey, true, cfg, daily, candidateFail); err != nil {
		t.Fatal(err)
	}
	if err := owner.noteSave(""); err != nil {
		t.Fatal(err)
	}
	if err := owner.recordKeyID(failKeyID); err != nil {
		t.Fatal(err)
	}
	s3OwnerTestClose(t, owner)

	failTransport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if strings.Contains(req.URL.Path, "/keys/") {
			return &http.Response{
				StatusCode: http.StatusInternalServerError,
				Body:       io.NopCloser(bytes.NewReader([]byte("synthetic key listing failure"))),
			}, nil
		}
		return peer.RoundTrip(req)
	})

	failReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidateFail,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, failTransport, failReq)
	failRes := s3TestWaitOperation(t, id, handle)
	if failRes.status == StatusOk {
		t.Fatal("expected failure on I/O error during key listing, got StatusOk")
	}

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || owner.record.Active.Operation != OpAddRecoveryKey ||
		!owner.record.Active.MayHaveWritten || !bytes.Equal(owner.record.Active.RecoveryPassword, candidateFail) ||
		owner.record.Active.KeyID != failKeyID {
		t.Fatalf("I/O failure erroneously modified or cleared the add: %+v", owner.record.Active)
	}
	s3OwnerTestClose(t, owner)
}

func TestS3RotationMoreThan16ResolvedRotations(t *testing.T) {
	cfg := s3TestConfig()
	state, workspace := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}

	daily := []byte("daily-password-many-rotations")

	// Init
	initReq := bridgeRequest{operation: OpInit, password: daily}
	id, handle := s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	initRes := s3TestWaitOperation(t, id, handle)
	if initRes.status != StatusOk {
		t.Fatalf("init failed: %d", initRes.status)
	}

	// Ordinary setup creates the original recovery key without a rotation hold.
	setupPassword := []byte("original-recovery-before-rotations")
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, bridgeRequest{
		operation: OpAddRecoveryKey, password: daily, recoveryPassword: setupPassword,
	})
	setup := s3TestWaitOperation(t, id, handle)
	if setup.status != StatusOk || !validS3ID(setup.keyID) || setup.keyID == initRes.keyID {
		t.Fatalf("ordinary recovery setup failed: %d %s", setup.status, setup.diagnostic)
	}
	prevKeyID, prevPassword := setup.keyID, setupPassword
	for i := 1; i <= 18; i++ {
		candidate := []byte(fmt.Sprintf("recovery-candidate-rotation-%d", i))
		id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, bridgeRequest{
			operation: OpAddRecoveryKey, rotationCustody: true, password: daily, recoveryPassword: candidate,
		})
		added := s3TestWaitOperation(t, id, handle)
		if added.status != StatusOk || !validS3ID(added.keyID) || added.keyID == prevKeyID || added.keyID == initRes.keyID {
			t.Fatalf("rotation %d did not create a distinct recovery key: %d %s", i, added.status, added.diagnostic)
		}
		id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, bridgeRequest{
			operation: OpOpen, password: candidate,
		})
		if r := s3TestWaitOperation(t, id, handle); r.status != StatusOk {
			t.Fatalf("rotation %d candidate cannot open repository: %d %s", i, r.status, r.diagnostic)
		}
		id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, bridgeRequest{
			operation: OpRemoveRecoveryKey, maintenanceRotation: true, password: daily, removeKeyID: prevKeyID,
		})
		if r := s3TestWaitOperation(t, id, handle); r.status != StatusOk {
			t.Fatalf("rotation %d removal failed: %d %s", i, r.status, r.diagnostic)
		}
		peer.mu.Lock()
		_, oldExists := peer.objects[cfg.prefix+"/keys/"+prevKeyID]
		_, dailyExists := peer.objects[cfg.prefix+"/keys/"+initRes.keyID]
		_, candidateExists := peer.objects[cfg.prefix+"/keys/"+added.keyID]
		peer.mu.Unlock()
		if oldExists || !dailyExists || !candidateExists || countS3Keys(peer) != 2 {
			t.Fatalf("rotation %d did not preserve exactly daily and replacement key", i)
		}
		id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, bridgeRequest{operation: OpOpen, password: prevPassword})
		if r := s3TestWaitOperation(t, id, handle); r.status != StatusFailed || r.diagnostic != DiagAuthFailed {
			t.Fatalf("rotation %d old password not refused: %d %s", i, r.status, r.diagnostic)
		}
		id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, bridgeRequest{operation: OpOpen, password: daily})
		if r := s3TestWaitOperation(t, id, handle); r.status != StatusOk {
			t.Fatalf("rotation %d daily password no longer works: %d %s", i, r.status, r.diagnostic)
		}
		owner := s3OwnerTestOpen(t, state, cfg)
		if owner.record.Active != nil || owner.record.RecoveryHold != nil || len(owner.record.Passwords) > maxS3OwnerPasswords {
			t.Fatalf("rotation %d left unresolved native custody", i)
		}
		s3OwnerTestClose(t, owner)
		prevKeyID, prevPassword = added.keyID, candidate
	}

	// One unresolved interrupted add still holds its candidate and still refuses to be replaced by a different password
	unresolvedCandidate := []byte("unresolved-candidate-x")
	owner := s3OwnerTestOpen(t, state, cfg)
	if err := owner.begin(OpAddRecoveryKey, true, cfg, daily, unresolvedCandidate); err != nil {
		t.Fatal(err)
	}
	if err := owner.noteSave(""); err != nil {
		t.Fatal(err)
	}
	s3OwnerTestClose(t, owner)

	diffCandidate := []byte("different-candidate-y")
	diffReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: diffCandidate,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, diffReq)
	diffRes := s3TestWaitOperation(t, id, handle)
	if diffRes.status != StatusRemotePending || diffRes.diagnostic != DiagRemotePending {
		t.Fatalf("expected remote pending, got status=%d diag=%s", diffRes.status, diffRes.diagnostic)
	}

	type resumeDocument struct {
		Schema           string `json:"schema"`
		Kind             string `json:"kind"`
		RecoveryPassword string `json:"recovery_password"`
	}
	var doc resumeDocument
	if err := json.Unmarshal(diffRes.metadata, &doc); err != nil {
		t.Fatalf("unmarshal resume document failed: %v", err)
	}
	if doc.RecoveryPassword != string(unresolvedCandidate) {
		t.Fatalf("expected held candidate %s, got %s", string(unresolvedCandidate), doc.RecoveryPassword)
	}
}

func TestS3RemovalLockAndContext(t *testing.T) {
	cfg := s3TestConfig()
	state, workspace := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}

	daily := []byte("daily-password-removal-lock")
	recovery := []byte("recovery-password-removal-lock")

	// Init
	initReq := bridgeRequest{operation: OpInit, password: daily}
	id, handle := s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	initRes := s3TestWaitOperation(t, id, handle)
	if initRes.status != StatusOk {
		t.Fatalf("init failed: %d", initRes.status)
	}
	dailyKeyID := initRes.keyID

	// Add recovery key
	addReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: recovery,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addReq)
	addRes := s3TestWaitOperation(t, id, handle)
	if addRes.status != StatusOk {
		t.Fatalf("add failed: %d", addRes.status)
	}
	recoveryKeyID := addRes.keyID

	// 1. Contender holding exclusive lock makes removal REPO_LOCKED and target key remains
	contenderBe := s3TestBackend(t, cfg, s3TestTransport(peer.RoundTrip))
	contenderRepo, err := repository.New(guardedBackend{contenderBe}, repository.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := safeScanKey(context.Background(), contenderRepo, string(daily)); err != nil {
		t.Fatal(err)
	}
	unlockContender, _, err := lockRepo(context.Background(), contenderRepo, true)
	if err != nil {
		t.Fatalf("failed to acquire contender lock: %v", err)
	}

	remReq := bridgeRequest{
		operation:           OpRemoveRecoveryKey,
		password:            daily,
		removeKeyID:         recoveryKeyID,
		maintenanceRotation: true,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, remReq)
	lockedRes := s3TestWaitOperation(t, id, handle)
	if lockedRes.status != StatusFailed || lockedRes.diagnostic != DiagLocked {
		t.Fatalf("expected REPO_LOCKED, got status=%d diag=%s", lockedRes.status, lockedRes.diagnostic)
	}

	peer.mu.Lock()
	if _, exists := peer.objects[cfg.prefix+"/keys/"+recoveryKeyID]; !exists {
		peer.mu.Unlock()
		t.Fatal("target key was deleted while contender held exclusive lock")
	}
	peer.mu.Unlock()

	unlockContender()
	_ = contenderBe.Close()

	// 2. The remove-locked hook unlocks so lockCtx cancels while parent context stays active,
	// no delete is sent and key remains
	hookCalled := false
	testCheckPhase = func(phase string, acquired context.Context, unlock func()) {
		if phase == "remove-locked" {
			hookCalled = true
			unlock()
		}
	}
	defer func() { testCheckPhase = nil }()

	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, remReq)
	hookRes := s3TestWaitOperation(t, id, handle)
	if !hookCalled {
		t.Fatal("remove-locked phase hook was not called")
	}
	if hookRes.status != StatusCancelled || hookRes.diagnostic != DiagCancelled {
		t.Fatalf("expected cancelled on lockCtx unlock, got status=%d diag=%s", hookRes.status, hookRes.diagnostic)
	}

	peer.mu.Lock()
	if _, exists := peer.objects[cfg.prefix+"/keys/"+recoveryKeyID]; !exists {
		peer.mu.Unlock()
		t.Fatal("target key was deleted after remove-locked hook unlocked")
	}
	peer.mu.Unlock()
	testCheckPhase = nil

	// 3. Normal exclusive removal deletes only the requested id and leaves another key in place
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, remReq)
	normRes := s3TestWaitOperation(t, id, handle)
	if normRes.status != StatusOk {
		t.Fatalf("normal removal failed: status=%d diag=%s", normRes.status, normRes.diagnostic)
	}

	peer.mu.Lock()
	if _, exists := peer.objects[cfg.prefix+"/keys/"+recoveryKeyID]; exists {
		peer.mu.Unlock()
		t.Fatal("target key was not deleted by normal removal")
	}
	if _, exists := peer.objects[cfg.prefix+"/keys/"+dailyKeyID]; !exists {
		peer.mu.Unlock()
		t.Fatal("daily key was deleted by normal removal of recovery key")
	}
	peer.mu.Unlock()

	// 4. Append-only Remove of a key file stays denied
	appendCfg := cfg
	appendCfg.appendOnly = true
	appendBe := s3TestBackend(t, appendCfg, s3TestTransport(peer.RoundTrip))
	defer appendBe.Close()
	err = appendBe.Remove(context.Background(), backend.Handle{Type: backend.KeyFile, Name: dailyKeyID})
	if !errors.Is(err, errS3Denied) {
		t.Fatalf("expected errS3Denied for append-only Remove of key file, got %v", err)
	}
}

func TestS3RotationCompletedAddRestartOrder(t *testing.T) {
	cfg := s3TestConfig()
	state, workspace := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}

	daily := []byte("daily-password-restart-order")
	candidateA := []byte("recovery-candidate-A-order")
	candidateB := []byte("recovery-candidate-B-order")
	candidateC := []byte("recovery-candidate-C-order")

	// 1. Init repository
	initReq := bridgeRequest{operation: OpInit, password: daily}
	id, handle := s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	initRes := s3TestWaitOperation(t, id, handle)
	if initRes.status != StatusOk || !validS3ID(initRes.keyID) {
		t.Fatalf("init failed: status=%d diag=%s", initRes.status, initRes.diagnostic)
	}
	initKeyID := initRes.keyID

	// 2. Add candidate A (succeeds)
	addAReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidateA,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addAReq)
	addARes := s3TestWaitOperation(t, id, handle)
	if addARes.status != StatusOk || !validS3ID(addARes.keyID) {
		t.Fatalf("add A failed: status=%d diag=%s", addARes.status, addARes.diagnostic)
	}
	keyA := addARes.keyID

	// Verify owner holds RecoveryHold for A
	owner := s3OwnerTestOpen(t, state, cfg)
	if owner.record.RecoveryHold == nil || owner.record.RecoveryHold.KeyID != keyA ||
		!bytes.Equal(owner.record.RecoveryHold.RecoveryPassword, candidateA) ||
		len(owner.record.RecoveryHold.Ciphertext) == 0 {
		t.Fatalf("recovery hold invalid after add A: %+v", owner.record.RecoveryHold)
	}
	s3OwnerTestClose(t, owner)

	// 3. List preserves hold
	listReq := bridgeRequest{operation: OpListRecoveryKeys, password: daily}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, listReq)
	listRes := s3TestWaitOperation(t, id, handle)
	if listRes.status != StatusOk {
		t.Fatalf("list failed: status=%d diag=%s", listRes.status, listRes.diagnostic)
	}

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.RecoveryHold == nil || owner.record.RecoveryHold.KeyID != keyA ||
		!bytes.Equal(owner.record.RecoveryHold.RecoveryPassword, candidateA) {
		t.Fatalf("recovery hold was not preserved across list: %+v", owner.record.RecoveryHold)
	}
	s3OwnerTestClose(t, owner)

	// 4. Add B returns REMOTE_PENDING with resume doc for A
	addBReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidateB,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addBReq)
	addBRes := s3TestWaitOperation(t, id, handle)
	if addBRes.status != StatusRemotePending || addBRes.diagnostic != DiagRemotePending {
		t.Fatalf("expected remote pending for add B with active hold, got status=%d diag=%s", addBRes.status, addBRes.diagnostic)
	}
	if len(addBRes.metadata) == 0 || addBRes.metadataStatus != StatusOk {
		t.Fatalf("expected metadata resume doc, got status=%d len=%d", addBRes.metadataStatus, len(addBRes.metadata))
	}
	var resumeDoc struct {
		Schema           string `json:"schema"`
		Kind             string `json:"kind"`
		RecoveryPassword string `json:"recovery_password"`
		KeyID            string `json:"key_id"`
	}
	if err := json.Unmarshal(addBRes.metadata, &resumeDoc); err != nil {
		t.Fatalf("failed to unmarshal resume doc: %v", err)
	}
	if resumeDoc.Schema != "solstone.backup-metadata.v1" || resumeDoc.Kind != "recovery-add-recorded" ||
		resumeDoc.RecoveryPassword != string(candidateA) || resumeDoc.KeyID != keyA {
		t.Fatalf("unexpected resume doc content: %+v", resumeDoc)
	}

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.RecoveryHold == nil || owner.record.RecoveryHold.KeyID != keyA ||
		!bytes.Equal(owner.record.RecoveryHold.RecoveryPassword, candidateA) {
		t.Fatalf("recovery hold altered after add B: %+v", owner.record.RecoveryHold)
	}
	s3OwnerTestClose(t, owner)

	peer.mu.Lock()
	if _, ok := peer.objects[cfg.prefix+"/keys/"+keyA]; !ok {
		peer.mu.Unlock()
		t.Fatal("key A missing from peer after add B")
	}
	for objKey := range peer.objects {
		if strings.Contains(objKey, "/keys/") &&
			objKey != cfg.prefix+"/keys/"+initKeyID &&
			objKey != cfg.prefix+"/keys/"+keyA {
			peer.mu.Unlock()
			t.Fatalf("unexpected new key in peer after add B: %s", objKey)
		}
	}
	peer.mu.Unlock()

	// 5. Add A succeeds
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addAReq)
	addA2Res := s3TestWaitOperation(t, id, handle)
	if addA2Res.status != StatusOk || addA2Res.keyID != keyA {
		t.Fatalf("add A retry failed: status=%d diag=%s keyID=%s (expected %s)", addA2Res.status, addA2Res.diagnostic, addA2Res.keyID, keyA)
	}

	// 6. Maintenance-remove init key while opening with A clears hold
	remReq := bridgeRequest{
		operation:           OpRemoveRecoveryKey,
		password:            candidateA,
		removeKeyID:         initKeyID,
		maintenanceRotation: true,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, remReq)
	remRes := s3TestWaitOperation(t, id, handle)
	if remRes.status != StatusOk {
		t.Fatalf("remove init key failed: status=%d diag=%s", remRes.status, remRes.diagnostic)
	}

	peer.mu.Lock()
	if _, exists := peer.objects[cfg.prefix+"/keys/"+initKeyID]; exists {
		peer.mu.Unlock()
		t.Fatal("init key object was not deleted by remove")
	}
	if _, exists := peer.objects[cfg.prefix+"/keys/"+keyA]; !exists {
		peer.mu.Unlock()
		t.Fatal("key A was deleted by remove of init key")
	}
	peer.mu.Unlock()

	owner = s3OwnerTestOpen(t, state, cfg)
	if owner.record.RecoveryHold != nil {
		t.Fatalf("recovery hold was not cleared by successful remove: %+v", owner.record.RecoveryHold)
	}
	s3OwnerTestClose(t, owner)

	// 7. Add C succeeds with new ID
	addCReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         candidateA,
		recoveryPassword: candidateC,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addCReq)
	addCRes := s3TestWaitOperation(t, id, handle)
	if addCRes.status != StatusOk || !validS3ID(addCRes.keyID) || addCRes.keyID == keyA || addCRes.keyID == initKeyID {
		t.Fatalf("add C failed: status=%d diag=%s keyID=%s", addCRes.status, addCRes.diagnostic, addCRes.keyID)
	}
}

func TestS3RotationAckLostExactPayload(t *testing.T) {
	cfg := s3TestConfig()
	state, workspace := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}

	daily := []byte("daily-password-ack-payload")
	candidate := []byte("recovery-candidate-ack-payload")

	// 1. Init repository
	initReq := bridgeRequest{operation: OpInit, password: daily}
	id, handle := s3TestStartOperation(t, cfg, state, workspace, peer, initReq)
	initRes := s3TestWaitOperation(t, id, handle)
	if initRes.status != StatusOk || !validS3ID(initRes.keyID) {
		t.Fatalf("init failed: status=%d diag=%s", initRes.status, initRes.diagnostic)
	}

	// 2. Add recovery key with transport that fails key PUT with 500
	var putFailed atomic.Bool
	failingTransport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		if req.Method == http.MethodPut && strings.Contains(req.URL.Path, "/keys/") {
			putFailed.Store(true)
			return &http.Response{
				StatusCode: http.StatusInternalServerError,
				Body:       io.NopCloser(bytes.NewReader([]byte("synthetic S3 500 on key PUT"))),
			}, nil
		}
		return peer.RoundTrip(req)
	})

	addReq := bridgeRequest{
		operation:        OpAddRecoveryKey,
		rotationCustody:  true,
		password:         daily,
		recoveryPassword: candidate,
	}
	id, handle = s3TestStartOperation(t, cfg, state, workspace, failingTransport, addReq)
	addRes := s3TestWaitOperation(t, id, handle)
	if addRes.status == StatusOk {
		t.Fatal("expected failure on key PUT 500, got StatusOk")
	}
	if !putFailed.Load() {
		t.Fatal("key PUT was not attempted")
	}

	// 3. Verify active has key ID and ciphertext while peer lacks object
	owner := s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active == nil || owner.record.Active.Operation != OpAddRecoveryKey ||
		!owner.record.Active.MayHaveWritten || owner.record.Active.KeyID == "" ||
		len(owner.record.Active.Ciphertext) == 0 {
		t.Fatalf("expected active with KeyID and Ciphertext, got: %+v", owner.record.Active)
	}
	savedKeyID := owner.record.Active.KeyID
	savedCiphertext := append([]byte(nil), owner.record.Active.Ciphertext...)
	s3OwnerTestClose(t, owner)

	peer.mu.Lock()
	if _, exists := peer.objects[cfg.prefix+"/keys/"+savedKeyID]; exists {
		peer.mu.Unlock()
		t.Fatal("key object exists in peer despite 500 failure")
	}
	peer.mu.Unlock()

	// 4. Retry same password succeeds with same ID and exact ciphertext bytes
	id, handle = s3TestStartOperation(t, cfg, state, workspace, peer, addReq)
	retryRes := s3TestWaitOperation(t, id, handle)
	if retryRes.status != StatusOk || retryRes.keyID != savedKeyID {
		t.Fatalf("retry failed: status=%d diag=%s keyID=%s (expected %s)", retryRes.status, retryRes.diagnostic, retryRes.keyID, savedKeyID)
	}

	peer.mu.Lock()
	objBytes, ok := peer.objects[cfg.prefix+"/keys/"+savedKeyID]
	peer.mu.Unlock()
	if !ok {
		t.Fatal("replayed key object missing from peer")
	}
	if !bytes.Equal(objBytes, savedCiphertext) {
		t.Fatalf("replayed bytes differ from buffered ciphertext: got %d bytes, expected %d bytes", len(objBytes), len(savedCiphertext))
	}
}

func TestS3RotationRemovalAckLostRestartPreservesExactIntent(t *testing.T) {
	cfg := s3TestConfig()
	state, workspace := s3OwnerTestDirectory(t), s3OwnerTestDirectory(t)
	peer := &s3MemoryPeer{cfg: cfg, objects: make(map[string][]byte)}
	daily, original, candidate := []byte("daily-removal-restart"), []byte("original-removal-restart"), []byte("candidate-removal-restart")
	call := func(transport http.RoundTripper, req bridgeRequest) bridgeResult {
		id, handle := s3TestStartOperation(t, cfg, state, workspace, transport, req)
		return s3TestWaitOperation(t, id, handle)
	}
	mustOK := func(result bridgeResult) bridgeResult {
		t.Helper()
		if result.status != StatusOk {
			t.Fatalf("operation failed: %d %s", result.status, result.diagnostic)
		}
		return result
	}
	mustOK(call(peer, bridgeRequest{operation: OpInit, password: daily}))
	old := mustOK(call(peer, bridgeRequest{operation: OpAddRecoveryKey, password: daily, recoveryPassword: original}))
	added := mustOK(call(peer, bridgeRequest{operation: OpAddRecoveryKey, rotationCustody: true, password: daily, recoveryPassword: candidate}))
	if !validS3ID(old.keyID) || !validS3ID(added.keyID) || old.keyID == added.keyID {
		t.Fatal("distinct keys not established")
	}
	remove := bridgeRequest{operation: OpRemoveRecoveryKey, maintenanceRotation: true, password: daily, removeKeyID: old.keyID}
	var lost atomic.Bool
	transport := s3TestTransport(func(req *http.Request) (*http.Response, error) {
		resp, err := peer.RoundTrip(req)
		if req.Method == http.MethodDelete && strings.HasSuffix(req.URL.Path, "/keys/"+old.keyID) && err == nil {
			if resp.Body != nil {
				_ = resp.Body.Close()
			}
			lost.Store(true)
			return nil, errors.New("synthetic lost removal acknowledgement")
		}
		return resp, err
	})
	result := call(transport, remove)
	if result.status == StatusOk || !lost.Load() {
		t.Fatal("missing acknowledged-loss control", result.status, result.diagnostic)
	}
	assertPending := func() {
		t.Helper()
		owner := s3OwnerTestOpen(t, state, cfg)
		if owner.record.Active == nil || owner.record.Active.Operation != OpRemoveRecoveryKey || !owner.record.Active.MayHaveWritten || owner.record.Active.KeyID != old.keyID || owner.record.Complete != nil {
			t.Fatal("exact possibly-written removal custody was lost")
		}
		s3OwnerTestClose(t, owner)
	}
	assertPending()
	peer.mu.Lock()
	_, exists := peer.objects[cfg.prefix+"/keys/"+old.keyID]
	peer.mu.Unlock()
	if exists {
		t.Fatal("control did not actually delete original key")
	}
	// Actual preliminary normal-owner calls must not consume the pending removal.
	mustOK(call(peer, bridgeRequest{operation: OpOpen, password: candidate}))
	assertPending()
	mustOK(call(peer, bridgeRequest{operation: OpListRecoveryKeys, password: daily}))
	assertPending()
	wrong := remove
	wrong.removeKeyID = s3TestID(99)
	if result := call(peer, wrong); result.status == StatusOk {
		t.Fatal("different target admitted")
	}
	assertPending()
	// A failed retry with the same target (wrong password, before any delete)
	// must likewise retain the already uncertain original operation.
	wrong = remove
	wrong.password = []byte("wrong-password-for-pending-remove")
	if result := call(peer, wrong); result.status == StatusOk {
		t.Fatal("wrong password admitted")
	}
	assertPending()
	mustOK(call(peer, remove)) // exact absent-target DELETE is idempotent
	owner := s3OwnerTestOpen(t, state, cfg)
	if owner.record.Active != nil || owner.record.Complete == nil || owner.record.Complete.Operation != OpRemoveRecoveryKey || owner.record.RecoveryHold != nil {
		t.Fatal("confirmed retry did not settle native removal")
	}
	s3OwnerTestClose(t, owner)
	mustOK(call(peer, bridgeRequest{operation: OpOpen, password: daily}))
	mustOK(call(peer, bridgeRequest{operation: OpOpen, password: candidate}))
	if result := call(peer, bridgeRequest{operation: OpOpen, password: original}); result.status != StatusFailed || result.diagnostic != DiagAuthFailed {
		t.Fatal("original key still usable", result.status, result.diagnostic)
	}
	// An ordinary next rotation is admitted instead of being wedged by a
	// surviving native Remove intent after Rust cleared its own pending record.
	mustOK(call(peer, bridgeRequest{operation: OpAddRecoveryKey, rotationCustody: true, password: daily, recoveryPassword: []byte("next-recovery-after-settled-remove")}))
}
