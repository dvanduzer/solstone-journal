// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"crypto/subtle"
	"errors"
	"fmt"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/restic/restic/internal/archiver"
	"github.com/restic/restic/internal/backend"
	"github.com/restic/restic/internal/checker"
	"github.com/restic/restic/internal/crypto"
	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/repository"
	"github.com/restic/restic/internal/restic"
	"github.com/restic/restic/internal/restorer"
	"github.com/restic/restic/internal/ui/progress"
	"github.com/restic/restic/internal/ui/restore"
)

// Restic accepts immutable strings and may make internal copies. Only the bridge's
// owned mutable credential buffers are explicitly cleared; no complete memory-wipe guarantee is made.
func passwordString(b []byte) (string, func()) { return string(b), func() {} }

// clipSnapshotIDs bounds snapshot list against maxSnapshotCount.
func clipSnapshotIDs(ids []string) bridgeResult {
	if len(ids) > maxSnapshotCount {
		return bridgeResult{
			status:        StatusCapExceeded,
			snapshotCount: 0,
			snapshotIDs:   nil,
			diagnostic:    DiagCapExceeded,
		}
	}
	return bridgeResult{
		status:        StatusOk,
		snapshotCount: uint32(len(ids)),
		snapshotIDs:   ids,
	}
}

// Process-wide mutex guarding KDF parameters in upstream AddKey.
// Upstream AddKey stores params *crypto.Params with no mutex, calibrated
// on first run with KDFTimeout = 500ms and KDFMemory = 60.
var addKeyMu sync.Mutex

const maxKeyFiles = 16

// lockRepo acquires a non-exclusive or exclusive restic repository lock.
// retryLock is 0 so the 5s retry sleep loop is never entered.
// printRetry and logger must be non-nil functions that discard their arguments.
// Unlocker.Unlock cancels the lock context and Waits for refresh goroutines.
// lock.Refresh(context.TODO()) ignores the operation context.
// Default refresh interval is 5 minutes.
func lockRepo(ctx context.Context, repo *repository.Repository, exclusive bool) (func(), context.Context, error) {
	printRetry := func(_ string) {}
	logger := func(_ string, _ ...interface{}) {}
	unlocker, lockCtx, err := repository.Lock(ctx, repo, exclusive, 0, printRetry, logger)
	if err != nil {
		return nil, ctx, err
	}
	return unlocker.Unlock, lockCtx, nil
}

// safeScanKey lists all keys in the repository and finds the first key that decrypts.
// Upstream OpenKey slices k.Data[:k.user.NonceSize()] with no length check (NonceSize = 16).
// We check len(k.Data) >= crypto.Extension (32 bytes = 16 IV + 16 MAC) before calling OpenKey.
// A panic, ErrUnauthenticated, or any other error continues the scan.
// The first successful decrypt returns immediately.
// Package SearchKey is called only once with a keyHint of the already-validated ID,
// setting r.key and loading config without triggering a second scan.
func safeScanKey(ctx context.Context, repo *repository.Repository, password string) (restic.ID, error) {
	var winningID restic.ID
	found := false
	resourceRefused := false
	keyCount := 0

	err := repo.List(ctx, restic.KeyFile, func(id restic.ID, _ int64) error {
		if found {
			return nil
		}
		keyCount++
		if keyCount > maxKeyFiles {
			resourceRefused = true
			return errResourceLimit
		}
		k, err := repository.LoadKey(ctx, repo, id)
		if err != nil {
			if errors.Is(err, errResourceLimit) {
				resourceRefused = true
			}
			return nil
		}
		if len(k.Data) < crypto.Extension {
			// Skip short or corrupted key data without calling OpenKey
			return nil
		}

		var openKey *repository.Key
		var openErr error
		func() {
			defer func() {
				if r := recover(); r != nil {
					openErr = fmt.Errorf("panic: %v", r)
				}
			}()
			openKey, openErr = repository.OpenKey(ctx, repo, id, password)
		}()

		if openErr == nil && openKey != nil {
			winningID = id
			found = true
			return nil
		}
		return nil
	})

	if err != nil && err != context.Canceled {
		if errors.Is(err, errResourceLimit) {
			return restic.ID{}, errResourceLimit
		}
		return restic.ID{}, err
	}
	if !found {
		if resourceRefused {
			return restic.ID{}, errResourceLimit
		}
		return restic.ID{}, errors.New("wrong password or no key found")
	}

	// Install the validated key and load config.
	// Package SearchKey returns before List when the hint opens.
	err = repo.SearchKey(ctx, password, 1, winningID.String())
	if err != nil {
		return restic.ID{}, err
	}
	return winningID, nil
}

// keyRefusal classifies a failed key scan. As restic's own open does, a
// prefix without a config holds no repository; that is not a rejected password.
func keyRefusal(ctx context.Context, be backend.Backend, err error) bridgeResult {
	if !errors.Is(err, errResourceLimit) {
		if _, statErr := be.Stat(ctx, backend.Handle{Type: restic.ConfigFile}); be.IsNotExist(statErr) {
			return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}
		}
	}
	return refusalResult(err, DiagAuthFailed)
}

func mapPinError(op int32, err error) bridgeResult {
	diag := DiagInvalidArgument
	if op == OpRestore {
		diag = DiagRestoreDestinationWrong
		if errors.Is(err, errRestoreExists) {
			diag = DiagRestoreDestinationExists
		} else if errors.Is(err, errRestoreNonEmpty) {
			diag = DiagRestoreNonEmpty
		}
	}
	return bridgeResult{status: StatusInvalidArgument, diagnostic: diag}
}

// executeInit initializes a new repository.
func executeInit(ctx context.Context, req bridgeRequest, setWrote func()) (bridgeResult, error) {
	if !req.pathsPinned {
		if err := pinRequestPaths(&req); err != nil {
			return mapPinError(req.operation, err), err
		}
		defer closeRequestPaths(&req)
	}

	pw, clearPw := passwordString(req.password)
	defer clearPw()

	setWrote()
	if testGate != nil {
		if err := testGate("mutation-entered", ctx); err != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, err
		}
	}
	if ctx.Err() != nil {
		return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, ctx.Err()
	}

	be, err := operationBackend(ctx, req, true)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, err
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, err
	}

	// (*Local).Save discards its context.
	// crypto.KDF takes no context.
	// AddKey stores params *crypto.Params with no mutex.
	// (*Repository).Init calls createMasterKey, which calls AddKey with empty username and hostname,
	// so the first key file records os.Hostname and user.Current. Do not delete that key.
	err = func() error {
		addKeyMu.Lock()
		defer addKeyMu.Unlock()
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return repo.Init(ctx, restic.StableRepoVersion, pw, nil)
	}()

	if err != nil {
		keyID := repo.KeyID()
		if req.remote == nil && !keyID.IsNull() {
			// If Init returned an error, repo.KeyID() is set, and the config file is absent,
			// remove that one key file through the *local.Local backend so a later init is not wedged.
			_, statErr := be.Stat(ctx, backend.Handle{Type: restic.ConfigFile})
			if be.IsNotExist(statErr) {
				// (*Local).Remove discards its context.
				_ = be.Remove(ctx, backend.Handle{Type: restic.KeyFile, Name: keyID.String()})
			}
		}
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, ctx.Err()
		}
		diagnostic := DiagRepoExists
		if req.remote != nil {
			diagnostic = s3InitFailureDiagnostic(err)
		}
		return bridgeResult{status: StatusFailed, diagnostic: diagnostic}, err
	}

	return bridgeResult{status: StatusOk, keyID: repo.KeyID().String()}, nil
}

// executeOpen opens a repository and verifies the password.
func executeOpen(ctx context.Context, req bridgeRequest) (bridgeResult, error) {
	if !req.pathsPinned {
		if err := pinRequestPaths(&req); err != nil {
			return mapPinError(req.operation, err), err
		}
		defer closeRequestPaths(&req)
	}

	pw, clearPw := passwordString(req.password)
	defer clearPw()

	be, err := operationBackend(ctx, req, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}, err
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, err
	}

	keyID, err := safeScanKey(ctx, repo, pw)
	if err != nil {
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, ctx.Err()
		}
		return keyRefusal(ctx, be, err), err
	}
	unlock, lockCtx, err := lockRepoForRequest(ctx, req, repo, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagLocked}, err
	}
	defer unlock()
	_ = lockCtx

	metadata, metadataStatus := repositoryMetadata(repo.Config())
	return bridgeResult{status: StatusOk, keyID: keyID.String(), metadata: metadata, metadataStatus: metadataStatus}, nil
}

// executeAddRecoveryKey adds a recovery key wrapping the existing master key.
func executeAddRecoveryKey(ctx context.Context, req bridgeRequest, setWrote func()) (bridgeResult, error) {
	if !req.pathsPinned {
		if err := pinRequestPaths(&req); err != nil {
			return mapPinError(req.operation, err), err
		}
		defer closeRequestPaths(&req)
	}

	if len(req.password) == len(req.recoveryPassword) &&
		subtle.ConstantTimeCompare(req.password, req.recoveryPassword) == 1 {
		return bridgeResult{status: StatusInvalidArgument, diagnostic: DiagInvalidArgument}, errors.New("passwords must not be equal")
	}

	dailyPw, clearDaily := passwordString(req.password)
	defer clearDaily()
	recPw, clearRec := passwordString(req.recoveryPassword)
	defer clearRec()

	be, err := operationBackend(ctx, req, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}, err
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, err
	}

	_, err = safeScanKey(ctx, repo, dailyPw)
	if err != nil {
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, ctx.Err()
		}
		return keyRefusal(ctx, be, err), err
	}
	unlock, lockCtx, err := lockRepoForRequest(ctx, req, repo, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagLocked}, err
	}
	defer unlock()
	_ = lockCtx

	setWrote()
	if testGate != nil {
		if err := testGate("mutation-entered", ctx); err != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, err
		}
	}

	// AddKey stores params *crypto.Params with no mutex.
	// Recovery AddKey passes username and hostname "solstone".
	// crypto.KDF takes no context.
	newKey, err := func() (*repository.Key, error) {
		addKeyMu.Lock()
		defer addKeyMu.Unlock()
		if lockCtx.Err() != nil {
			return nil, lockCtx.Err()
		}
		return repository.AddKey(lockCtx, repo, recPw, "solstone", "solstone", repo.Key())
	}()

	if err != nil {
		if lockCtx.Err() != nil {
			return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, lockCtx.Err()
		}
		return bridgeResult{status: StatusFailed, diagnostic: scrubError(err, req)}, err
	}

	newID := newKey.ID()

	// Verification is a separate backend open and safe scan with only the recovery password.
	verified := false
	beVerify, err := operationBackend(lockCtx, req, false)
	if err == nil {
		repoVerify, err := repository.New(guardedBackend{beVerify}, repository.Options{})
		if err == nil {
			verifyID, err := safeScanKey(lockCtx, repoVerify, recPw)
			if err == nil && verifyID == newID {
				verified = true
			}
		}
		_ = beVerify.Close()
	}

	if !verified {
		// (*Local).Remove discards its context.
		// RemoveKey refuses when id == repo.KeyID(), which is why it must be the daily-opened repo.
		if req.remote == nil {
			_ = repository.RemoveKey(lockCtx, repo, newID)
		}
		if lockCtx.Err() != nil {
			return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, lockCtx.Err()
		}
		return refusalResult(err, DiagAuthFailed), errors.New("recovery key verification failed")
	}

	if lockCtx.Err() != nil {
		return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, lockCtx.Err()
	}

	return bridgeResult{status: StatusOk, keyID: newID.String()}, nil
}

// executeSnapshot creates a snapshot of sourcePath.
func executeSnapshot(ctx context.Context, req bridgeRequest, setWrote func()) (bridgeResult, error) {
	if !req.pathsPinned {
		if err := pinRequestPaths(&req); err != nil {
			return mapPinError(req.operation, err), err
		}
		defer closeRequestPaths(&req)
	}

	pw, clearPw := passwordString(req.password)
	defer clearPw()

	be, err := operationBackend(ctx, req, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}, err
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, err
	}

	_, err = safeScanKey(ctx, repo, pw)
	if err != nil {
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, ctx.Err()
		}
		return keyRefusal(ctx, be, err), err
	}
	unlock, lockCtx, err := lockRepoForRequest(ctx, req, repo, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagLocked}, err
	}
	defer unlock()
	_ = lockCtx

	// archiver.Snapshot calls data.NewSnapshot (fillUserInfo writes the process user)
	// and data.SaveSnapshot before it returns. Set SnapshotOptions.Hostname to "solstone".
	// Do not save a second snapshot to overwrite Username.
	// (*Local).Save discards its context.
	setWrote()
	if testGate != nil {
		if err := testGate("mutation-entered", ctx); err != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, err
		}
	}

	if err := repo.LoadIndex(lockCtx, &progress.NoopPrinter{}); err != nil {
		return refusalResult(err, DiagRepoCorrupt), err
	}
	sourceName := req.sourceName
	if sourceName == "" {
		sourceName = filepath.Base(req.sourcePath)
	}
	rootFS := newRootFS(req.sourceRoot, sourceName, req.metadataRoot)
	rootFS.owned = req.owned
	arch := archiver.New(repo, rootFS, archiver.Options{})
	if req.snapshotSelection != nil {
		arch.SelectByName = func(item string) bool { return req.snapshotSelection.permits(lockCtx, rootFS, item) }
	}
	// Upstream worker paths call Error for non-context failures. Returning the
	// original error both aborts on first item failure and avoids a nil callback
	// if cancellation races an in-flight filesystem operation.
	arch.Error = func(_ string, err error) error { return err }

	opts := archiver.SnapshotOptions{
		Hostname: "solstone",
		Time:     time.Now(),
	}

	_, snID, _, err := arch.Snapshot(lockCtx, []string{sourceName}, opts)
	if req.snapshotSelection != nil {
		err = errors.Join(err, req.snapshotSelection.failure())
	}
	if err != nil {
		if strings.Contains(err.Error(), "snapshot is empty") {
			return bridgeResult{status: StatusFailed, diagnostic: DiagSnapshotEmpty}, err
		}
		if lockCtx.Err() != nil {
			return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, lockCtx.Err()
		}
		return bridgeResult{status: StatusFailed, diagnostic: scrubError(err, req)}, err
	}

	return bridgeResult{status: StatusOk, snapshotID: snID.String()}, nil
}

// executeListSnapshots lists all snapshots in the repository.
func executeListSnapshots(ctx context.Context, req bridgeRequest) (bridgeResult, error) {
	if !req.pathsPinned {
		if err := pinRequestPaths(&req); err != nil {
			return mapPinError(req.operation, err), err
		}
		defer closeRequestPaths(&req)
	}

	pw, clearPw := passwordString(req.password)
	defer clearPw()

	be, err := operationBackend(ctx, req, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}, err
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, err
	}

	_, err = safeScanKey(ctx, repo, pw)
	if err != nil {
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, ctx.Err()
		}
		return keyRefusal(ctx, be, err), err
	}
	unlock, lockCtx, err := lockRepoForRequest(ctx, req, repo, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagLocked}, err
	}
	defer unlock()
	_ = lockCtx

	var ids []string
	var idsMu sync.Mutex
	var metadata snapshotMetadataCollector
	overflow := false
	err = data.ForAllSnapshots(lockCtx, repo, repo, nil, func(id restic.ID, snapshot *data.Snapshot, err error) error {
		idsMu.Lock()
		defer idsMu.Unlock()
		if err != nil {
			return err
		}
		if len(ids) >= maxSnapshotCount {
			overflow = true
			return errResourceLimit
		}
		ids = append(ids, id.String())
		metadata.add(id, snapshot)
		return nil
	})
	if overflow {
		return bridgeResult{status: StatusCapExceeded, diagnostic: DiagCapExceeded}, errResourceLimit
	}
	if err != nil {
		if lockCtx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, lockCtx.Err()
		}
		return refusalResult(err, scrubError(err, req)), err
	}
	sort.Strings(ids)
	result := clipSnapshotIDs(ids)
	result.metadata, result.metadataStatus = metadata.finish()
	return result, nil
}

// silentRestoreProgressPrinter implements restore.ProgressPrinter without output.
type silentRestoreProgressPrinter struct {
	progress.NoopPrinter
}

func (silentRestoreProgressPrinter) Update(_ restore.State, _ time.Duration)               {}
func (silentRestoreProgressPrinter) Error(_ string, err error) error                       { return err }
func (silentRestoreProgressPrinter) CompleteItem(_ restore.ItemAction, _ string, _ uint64) {}
func (silentRestoreProgressPrinter) Finish(_ restore.State, _ time.Duration)               {}

// executeRestore restores snapshotID to restorePath.
func executeRestore(ctx context.Context, req *bridgeRequest, setWrote func()) (bridgeResult, error) {
	return executeRestoreWithPublisher(ctx, req, setWrote, promoteRestoreStage)
}

func executeRestoreWithPublisher(ctx context.Context, req *bridgeRequest, setWrote func(), publish func(context.Context, *bridgeRequest) error) (bridgeResult, error) {
	if !req.pathsPinned {
		if err := pinRequestPaths(req); err != nil {
			return mapPinError(req.operation, err), err
		}
		defer closeRequestPaths(req)
	}

	pw, clearPw := passwordString(req.password)
	defer clearPw()

	be, err := operationBackend(ctx, *req, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}, err
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, err
	}

	_, err = safeScanKey(ctx, repo, pw)
	if err != nil {
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, ctx.Err()
		}
		return keyRefusal(ctx, be, err), err
	}
	unlock, lockCtx, err := lockRepo(ctx, repo, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagLocked}, err
	}
	defer unlock()
	_ = lockCtx

	if err := repo.LoadIndex(lockCtx, &progress.NoopPrinter{}); err != nil {
		return refusalResult(err, DiagRepoCorrupt), err
	}
	snID, err := restic.ParseID(req.snapshotID)
	if err != nil {
		return bridgeResult{status: StatusInvalidArgument, diagnostic: DiagInvalidArgument}, err
	}

	sn, err := data.LoadSnapshot(lockCtx, repo, snID)
	if err != nil {
		return refusalResult(err, DiagRepoCorrupt), err
	}

	// openReader discards its context.
	setWrote()
	if testGate != nil {
		if err := testGate("mutation-entered", ctx); err != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, err
		}
	}

	prog := restore.NewProgress(&silentRestoreProgressPrinter{}, time.Hour)
	defer prog.Finish()

	res := restorer.NewRestorer(repo, sn, restorer.Options{Progress: prog})
	writeGate := testGate
	var writeOnce sync.Once
	var writeErr error
	res.BeforeWrite = func() error {
		writeOnce.Do(func() {
			if writeGate != nil {
				writeErr = writeGate("restore-write", lockCtx)
			}
		})
		if writeErr != nil {
			return writeErr
		}
		if req.pathsPinned {
			return recheckRestoreParent(req)
		}
		return nil
	}

	_, err = res.RestoreToRoot(lockCtx, req.restoreRoot)
	if err != nil {
		if lockCtx.Err() != nil {
			return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, lockCtx.Err()
		}
		return bridgeResult{status: StatusFailed, diagnostic: scrubError(err, *req)}, err
	}
	if req.pathsPinned {
		if err := publish(ctx, req); err != nil {
			if errors.Is(err, context.Canceled) {
				return bridgeResult{status: StatusCancelledUncertain, diagnostic: DiagCancelledUncertain}, err
			}
			if errors.Is(err, errRestoreCommittedUncertain) {
				return bridgeResult{status: StatusCommittedUncertain, diagnostic: DiagCommittedUncertain}, err
			}
			return bridgeResult{status: StatusFailed, diagnostic: DiagIoFailed}, err
		}
	}

	return bridgeResult{status: StatusOk}, nil
}

// executeCheck runs repository integrity verification.
func executeCheck(ctx context.Context, req bridgeRequest) (bridgeResult, error) {
	if !req.pathsPinned {
		if err := pinRequestPaths(&req); err != nil {
			return mapPinError(req.operation, err), err
		}
		defer closeRequestPaths(&req)
	}

	pw, clearPw := passwordString(req.password)
	defer clearPw()

	be, err := operationBackend(ctx, req, false)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoMissing}, err
	}
	defer be.Close()

	repo, err := repository.New(guardedBackend{be}, repository.Options{})
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagInternal}, err
	}

	// check uses an exclusive lock.
	// lock.Refresh(context.TODO()) ignores the operation context.
	_, err = safeScanKey(ctx, repo, pw)
	if err != nil {
		if ctx.Err() != nil {
			return bridgeResult{status: StatusCancelled, diagnostic: DiagCancelled}, ctx.Err()
		}
		return keyRefusal(ctx, be, err), err
	}
	unlock, lockCtx, err := lockRepoForRequest(ctx, req, repo, true)
	if err != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagLocked}, err
	}
	defer unlock()

	chkr := checker.New(repo, false)
	err = chkr.LoadSnapshots(lockCtx, &data.SnapshotFilter{}, nil)
	if result, contextErr := checkContextsAfterPhase(ctx, lockCtx, "snapshots", unlock); contextErr != nil {
		return result, contextErr
	}
	if err != nil {
		return refusalResult(err, DiagRepoCorrupt), err
	}

	hints, errs := chkr.LoadIndex(lockCtx, &progress.NoopPrinter{})
	if result, contextErr := checkContextsAfterPhase(ctx, lockCtx, "index", unlock); contextErr != nil {
		return result, contextErr
	}
	if len(errs) > 0 {
		return refusalResult(errs[0], DiagRepoCorrupt), errs[0]
	}
	for _, hint := range hints {
		var dupErr *repository.ErrDuplicatePacks
		var mixErr *repository.ErrMixedPack
		var incErr *repository.ErrIncompletePackEntry
		if errors.As(hint, &dupErr) || errors.As(hint, &mixErr) {
			continue
		}
		if errors.As(hint, &incErr) {
			return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, hint
		}
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, hint
	}

	// Packs check
	packErrChan := make(chan error)
	go chkr.Packs(lockCtx, packErrChan)
	var hardPackErr error
	for err := range packErrChan {
		if err != nil && hardPackErr == nil {
			if packErr, ok := err.(*repository.PackError); ok && packErr.Orphaned {
				continue // Orphaned packs are non-critical
			}
			hardPackErr = err
		}
	}
	if result, contextErr := checkContextsAfterPhase(ctx, lockCtx, "packs", unlock); contextErr != nil {
		return result, contextErr
	}
	if hardPackErr != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, hardPackErr
	}

	// Structure check
	if testCheckPhase != nil {
		testCheckPhase("before-structure", lockCtx, unlock)
	}
	structCounter := progress.NewCounter(time.Hour, 0, nil)
	structErrChan := make(chan error)
	go chkr.Structure(lockCtx, structCounter, structErrChan)
	var hardStructErr error
	for err := range structErrChan {
		if err != nil && hardStructErr == nil {
			hardStructErr = err
		}
	}
	structCounter.Done()
	if result, contextErr := checkContextsAfterPhase(ctx, lockCtx, "structure", unlock); contextErr != nil {
		return result, contextErr
	}
	if hardStructErr != nil {
		return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, hardStructErr
	}

	// Structure reads required tree blobs independently of this optional full
	// pack-content verification. Grouping only selects this latter phase.
	if filter := checkPackFilter(req.executionOptions); filter != nil {
		if testCheckPhase != nil {
			testCheckPhase("before-read-packs", lockCtx, unlock)
		}
		readCounter := progress.NewCounter(time.Hour, 0, nil)
		readErrChan := make(chan error)
		go chkr.ReadPacks(lockCtx, filter, readCounter, readErrChan)
		var hardReadErr error
		for err := range readErrChan {
			if err != nil && hardReadErr == nil {
				hardReadErr = err
			}
		}
		readCounter.Done()
		if result, contextErr := checkContextsAfterPhase(ctx, lockCtx, "read-packs", unlock); contextErr != nil {
			return result, contextErr
		}
		if hardReadErr != nil {
			return bridgeResult{status: StatusFailed, diagnostic: DiagRepoCorrupt}, hardReadErr
		}
	}
	if result, contextErr := checkContextsAfterPhase(ctx, lockCtx, "complete", unlock); contextErr != nil {
		return result, contextErr
	}

	return bridgeResult{status: StatusOk}, nil
}

// scrubError ensures that passwords or raw internal secrets never leak into diagnostics.
func scrubError(err error, req bridgeRequest) string {
	if err == nil {
		return DiagOk
	}
	s := err.Error()
	if strings.Contains(s, ".solstone-restore-staging-") || strings.Contains(s, "/dev/fd/") {
		return DiagIoFailed
	}
	errBytes := []byte(s)
	if (len(req.password) > 0 && bytes.Contains(errBytes, req.password)) ||
		(len(req.recoveryPassword) > 0 && bytes.Contains(errBytes, req.recoveryPassword)) {
		return DiagAuthFailed
	}
	if strings.Contains(s, "wrong password") || strings.Contains(s, "ciphertext verification failed") {
		return DiagAuthFailed
	}
	if strings.Contains(s, "does not exist") || strings.Contains(s, "not found") {
		return DiagRepoMissing
	}
	if strings.Contains(s, "already initialized") || strings.Contains(s, "already exists") {
		return DiagRepoExists
	}
	if strings.Contains(s, "is already locked") {
		return DiagLocked
	}
	return DiagIoFailed
}
