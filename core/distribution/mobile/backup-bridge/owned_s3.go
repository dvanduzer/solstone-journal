// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

// owner.mu is released before any cohort sealing/blocking. Keep actual fields
// and both exclusion registrations until every required close is confirmed.
func (owner *s3Owner) closeOwned() error {
	owner.mu.Lock()
	if owner.closed {
		owner.mu.Unlock()
		return nil
	}
	defer func() { owner.mu.Unlock() }()
	if owner.owned.failed() {
		return errOwnedCleanup
	}
	if owner.begun && !owner.primaryDrained || owner.cleanupActive {
		return owner.owned.markFault()
	}
	if owner.stateFile != nil {
		if err := owner.owned.close(owner.stateFile, "s3-owner-state-file"); err != nil {
			return err
		}
		owner.stateFile = nil
	}
	if owner.root != nil {
		if err := owner.owned.close(owner.root, "s3-owner-root"); err != nil {
			return err
		}
		owner.root = nil
	}
	if owner.directory != nil {
		if err := closeOwnedPrivateDirectory(owner.directory, owner.owned, "s3-owner-directory"); err != nil {
			return err
		}
		owner.directory = nil
	}
	owner.closed = true
	for _, password := range owner.record.Passwords {
		zeroBytes(password)
	}
	for _, password := range owner.attemptPasses {
		zeroBytes(password)
	}
	if owner.record.Active != nil {
		if len(owner.record.Active.RecoveryPassword) > 0 {
			zeroBytes(owner.record.Active.RecoveryPassword)
		}
		if len(owner.record.Active.Ciphertext) > 0 {
			zeroBytes(owner.record.Active.Ciphertext)
		}
	}
	if owner.preservedAddIntent != nil {
		if len(owner.preservedAddIntent.RecoveryPassword) > 0 {
			zeroBytes(owner.preservedAddIntent.RecoveryPassword)
		}
		if len(owner.preservedAddIntent.Ciphertext) > 0 {
			zeroBytes(owner.preservedAddIntent.Ciphertext)
		}
	}
	if owner.preservedRemoveIntent != nil {
		if len(owner.preservedRemoveIntent.RecoveryPassword) > 0 {
			zeroBytes(owner.preservedRemoveIntent.RecoveryPassword)
		}
		if len(owner.preservedRemoveIntent.Ciphertext) > 0 {
			zeroBytes(owner.preservedRemoveIntent.Ciphertext)
		}
	}
	if owner.record.RecoveryHold != nil {
		if len(owner.record.RecoveryHold.RecoveryPassword) > 0 {
			zeroBytes(owner.record.RecoveryHold.RecoveryPassword)
		}
		if len(owner.record.RecoveryHold.Ciphertext) > 0 {
			zeroBytes(owner.record.RecoveryHold.Ciphertext)
		}
	}
	owner.attemptPasses = nil
	owner.preservedAddIntent = nil
	owner.preservedRemoveIntent = nil
	// No reverse path acquires owner.mu while holding s3OwnersMu; keep namespace
	// registration until all teardown has actually completed.
	s3OwnersMu.Lock()
	if s3Owners[owner.destination] == owner {
		delete(s3Owners, owner.destination)
	}
	s3OwnersMu.Unlock()
	return nil
}
