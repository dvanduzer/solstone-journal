// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"io"
	"os"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

var errTargetAdmissionCleanup = errors.New("TARGET_ADMISSION_CLEANUP_FAILED")

// All owner/borrow/close state uses rootedTargets. The only nested order is
// handlesMu -> callHandle.mu -> rootedTargets. Target functions never take a
// handle lock. Descriptor use by a worker is protected by its retained borrow.
// Release/revalidate serialize their own descriptor use under rootedTargets.
type rootedTargetAdmission struct {
	id     uint64
	path   string
	rooted *rootedRestore
	root   *os.Root
	borrow *rootedRestore
}

// Private deterministic effect-boundary fault injection; absent in production.
var targetAdmissionCloseHook func(string, io.Closer) error

func closeTargetAdmissionResource(kind string, resource io.Closer) error {
	if targetAdmissionCloseHook != nil {
		return targetAdmissionCloseHook(kind, resource)
	}
	return resource.Close()
}

func targetAdmissionStatus(err error) int32 {
	if errors.Is(err, errS3OwnerBusy) || errors.Is(err, unix.EWOULDBLOCK) || errors.Is(err, unix.EAGAIN) {
		return StatusBusy
	}
	if errors.Is(err, errTargetAdmissionCleanup) {
		return StatusFailed
	}
	return StatusInvalidArgument
}

func (owner *rootedTargetAdmission) closeLocked() error {
	if owner.rooted.closeFailure != nil {
		return owner.rooted.closeFailure
	}
	if owner.root != nil {
		root := owner.root
		owner.root = nil
		if err := closeTargetAdmissionResource("root", root); err != nil {
			return owner.rooted.failCloseLocked(err)
		}
	}
	return owner.rooted.closeLocked()
}

// A nonzero ID with failed status is a refused cleanup owner, never usable
// admission. All IDs share the operation allocator, preventing kind confusion.
func admitRootedTarget(fd int, path string) (uint64, int32) {
	if fd < 0 || !utf8.ValidString(path) || len(path) > maxPathLen || validateExistingDir(path) != nil {
		return 0, StatusInvalidArgument
	}
	rootedTargets.Lock()
	refused := len(rootedTargets.failed) != 0
	full := len(rootedTargets.owners) >= 16
	rootedTargets.Unlock()
	if refused {
		return 0, StatusFailed
	}
	if full {
		return 0, StatusCapExceeded
	}
	id, status := allocateID(&nextID)
	if status != StatusOk {
		return 0, status
	}
	r, status := duplicateRootedTarget(fd)
	if r == nil {
		return 0, status
	}
	owner := &rootedTargetAdmission{id: id, path: path, rooted: r}
	if status == StatusOk {
		req := bridgeRequest{restorePath: path, rootedRestore: r}
		if err := pinRootedRestore(&req, nil); err != nil {
			status = targetAdmissionStatus(err)
		}
		owner.root = req.restoreRoot
	}
	rootedTargets.Lock()
	defer rootedTargets.Unlock()
	if status == StatusOk && (len(rootedTargets.owners) >= 16 || len(rootedTargets.failed) != 0) {
		status = StatusBusy
	}
	if status != StatusOk {
		if err := owner.closeLocked(); err == nil {
			return 0, status
		}
		rootedTargets.owners[id] = owner
		return id, StatusFailed
	}
	rootedTargets.owners[id] = owner
	return id, StatusOk
}

func revalidateRootedTarget(id uint64) int32 {
	rootedTargets.Lock()
	defer rootedTargets.Unlock()
	owner := rootedTargets.owners[id]
	if owner == nil {
		return StatusInvalidHandle
	}
	if owner.rooted.closeFailure != nil || len(rootedTargets.failed) != 0 {
		return StatusFailed
	}
	if err := recheckRootedRestore(&bridgeRequest{restorePath: owner.path, rootedRestore: owner.rooted}); err != nil {
		return StatusInvalidArgument
	}
	return StatusOk
}

func releaseRootedTarget(id uint64) int32 {
	rootedTargets.Lock()
	defer rootedTargets.Unlock()
	owner := rootedTargets.owners[id]
	if owner == nil {
		return StatusInvalidHandle
	}
	if owner.borrow != nil {
		return StatusBusy
	}
	if err := owner.closeLocked(); err != nil {
		return StatusFailed
	}
	delete(rootedTargets.owners, id)
	return StatusOk
}

func borrowRootedTarget(id uint64, selector string) (*rootedRestore, int32) {
	if !validateRootedSelector(selector) {
		return nil, StatusInvalidArgument
	}
	rootedTargets.Lock()
	defer rootedTargets.Unlock()
	owner := rootedTargets.owners[id]
	if owner == nil {
		return nil, StatusInvalidHandle
	}
	if owner.rooted.closeFailure != nil || len(rootedTargets.failed) != 0 {
		return nil, StatusFailed
	}
	if owner.borrow != nil {
		return nil, StatusBusy
	}
	if err := recheckRootedRestore(&bridgeRequest{restorePath: owner.path, rootedRestore: owner.rooted}); err != nil {
		return nil, StatusInvalidArgument
	}
	r := &rootedRestore{selector: selector, target: owner.rooted.target, lease: owner.rooted.lease,
		identity: owner.rooted.identity, borrowed: owner}
	owner.borrow = r
	return r, StatusOk
}

func releaseRootedRestoreBorrow(r *rootedRestore) error {
	if r == nil || r.borrowed == nil {
		return nil
	}
	rootedTargets.Lock()
	defer rootedTargets.Unlock()
	if r.borrowReleased {
		return nil
	}
	owner := r.borrowed
	if rootedTargets.owners[owner.id] != owner || owner.borrow != r {
		return errPinnedIdentity
	}
	owner.borrow, r.borrowReleased = nil, true
	return nil
}
