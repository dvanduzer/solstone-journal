// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"io"
	"sync"
	"sync/atomic"
)

var errOwnedCleanup = errors.New("VERSIONED_NATIVE_CLEANUP_UNCONFIRMED")

// This is only the new normal-call cohort. Legacy and retained-target dispatch
// do not acquire these records. A failed record never retires or reopens.
var ownedCalls = struct {
	sync.Mutex
	sealed bool
	live   map[*ownedCall]struct{}
}{live: make(map[*ownedCall]struct{})}

const maxOwnedCalls = 16

type ownedClose struct {
	done    chan struct{}
	started bool
	err     error
}

type ownedCall struct {
	mu        sync.Mutex
	resources map[io.Closer]*ownedClose
	request   *bridgeRequest
	owner     *s3Owner
	fault     error
	published bool // ownedCalls mutex
	admitted  atomic.Bool
}

// Private deterministic controls, nil in production; there is no ABI setter.
var testOwnedStage func(string, *ownedCall)
var testOwnedClose func(string, io.Closer, func() error) error

func reserveOwnedCall() (*ownedCall, int32) {
	ownedCalls.Lock()
	if ownedCalls.sealed {
		ownedCalls.Unlock()
		return nil, StatusFailed
	}
	if len(ownedCalls.live) >= maxOwnedCalls {
		ownedCalls.Unlock()
		return nil, StatusBusy
	}
	entry := &ownedCall{resources: make(map[io.Closer]*ownedClose)}
	ownedCalls.live[entry] = struct{}{}
	ownedCalls.Unlock()
	if testOwnedStage != nil {
		testOwnedStage("reserved", entry)
	}
	return entry, StatusOk
}

func (entry *ownedCall) retain(req *bridgeRequest) {
	if entry == nil {
		return
	}
	entry.mu.Lock()
	entry.request = req
	entry.mu.Unlock()
}
func (entry *ownedCall) retainOwner(owner *s3Owner) {
	if entry == nil {
		return
	}
	entry.mu.Lock()
	entry.owner = owner
	entry.mu.Unlock()
}
func (entry *ownedCall) hold(resource io.Closer) {
	if entry == nil || resource == nil {
		return
	}
	entry.mu.Lock()
	if entry.resources[resource] == nil {
		entry.resources[resource] = &ownedClose{done: make(chan struct{})}
	}
	entry.mu.Unlock()
}
func (entry *ownedCall) markFault() error {
	if entry == nil {
		return errOwnedCleanup
	}
	entry.mu.Lock()
	entry.fault = errOwnedCleanup
	entry.mu.Unlock()
	return errOwnedCleanup
}
func (entry *ownedCall) failed() bool {
	if entry == nil {
		return false
	}
	entry.mu.Lock()
	defer entry.mu.Unlock()
	return entry.fault != nil
}

// A helper records the fault locally. Its caller releases owner/exclusion locks
// before sealing or blocking. Every actual closer is attempted at most once.
func (entry *ownedCall) close(resource io.Closer, stage string) error {
	return entry.closeAction(resource, stage, resource.Close)
}

func (entry *ownedCall) closeAction(resource io.Closer, stage string, action func() error) error {
	if entry == nil {
		return action()
	}
	entry.hold(resource)
	entry.mu.Lock()
	state := entry.resources[resource]
	if state.started {
		entry.mu.Unlock()
		<-state.done
		return state.err
	}
	if entry.fault != nil {
		entry.mu.Unlock()
		return errOwnedCleanup
	}
	state.started = true
	entry.mu.Unlock()
	var err error
	func() {
		defer func() {
			if recover() != nil {
				err = errOwnedCleanup
			}
		}()
		if testOwnedClose != nil {
			err = testOwnedClose(stage, resource, action)
		} else {
			err = action()
		}
	}()
	entry.mu.Lock()
	if err != nil {
		entry.fault = errOwnedCleanup
		state.err = errOwnedCleanup
	}
	close(state.done)
	result := state.err
	entry.mu.Unlock()
	return result
}

// Logical cleanup outcomes retain their existing error; a panic cannot certify
// actual cleanup. This boundary does not classify all ordinary I/O as a fault.
func (entry *ownedCall) cleanup(action func() error) (err error) {
	if entry == nil {
		return action()
	}
	if entry.failed() {
		return errOwnedCleanup
	}
	defer func() {
		if recover() != nil {
			err = entry.markFault()
		}
	}()
	return action()
}

func (entry *ownedCall) blockIfFault(req *bridgeRequest) {
	if entry == nil || !entry.failed() {
		return
	}
	entry.retain(req)
	ownedCalls.Lock()
	ownedCalls.sealed = true
	ownedCalls.Unlock()
	if testOwnedStage != nil {
		testOwnedStage("fault-retained", entry)
	}
	// The actual entry/worker stack stays live. There is deliberately no reset,
	// cancellation escape, terminal result or timeout pretending this drained.
	select {}
}
func (entry *ownedCall) retire() {
	if entry == nil {
		return
	}
	ownedCalls.Lock()
	if !entry.failed() {
		delete(ownedCalls.live, entry)
	}
	ownedCalls.Unlock()
}

func (entry *ownedCall) finishAdmission(status int32) int32 {
	ownedCalls.Lock()
	published := entry.published
	ownedCalls.Unlock()
	if published {
		return status
	}
	entry.mu.Lock()
	req, owner := entry.request, entry.owner
	entry.mu.Unlock()
	if req == nil {
		req = &bridgeRequest{owned: entry}
	}
	// Mutable secrets are scrubbed before a rejection cleanup can block.
	zeroBytes(req.password)
	zeroBytes(req.recoveryPassword)
	if !entry.failed() {
		err := entry.cleanup(func() error { return closeRequestPaths(req) })
		if entry.failed() {
			entry.blockIfFault(req)
		}
		if owner != nil {
			err = errors.Join(err, owner.Close())
		}
		if errors.Is(err, errMetadataCleanup) {
			status = StatusWorkspaceCleanupRequired
		} else if err != nil {
			status = StatusFailed
		}
	}
	entry.blockIfFault(req)
	entry.retire()
	return status
}

// A helper already knows admission is being rejected before entering a closer.
// No cohort lock is acquired under owner/private registry locks.
func (entry *ownedCall) scrubRejection() {
	if entry == nil || entry.admitted.Load() {
		return
	}
	entry.mu.Lock()
	req := entry.request
	entry.mu.Unlock()
	if req != nil {
		zeroBytes(req.password)
		zeroBytes(req.recoveryPassword)
	}
}
