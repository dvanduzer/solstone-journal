// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import "golang.org/x/sys/unix"

func s3StateSyncFile(fd int) error { return unix.Fsync(fd) }
func s3StateReplace(fd int, source, target string, replacing bool) error {
	flags := uint(unix.RENAME_NOREPLACE)
	if replacing {
		flags = unix.RENAME_EXCHANGE
	}
	return unix.Renameat2(fd, source, fd, target, flags)
}

// Linux production has no native remote transport. These checks accompany
// ordinary owner/mode enforcement in controlled tests, not iOS policy proof.
func s3StateRootPolicy(_ int) error         { return nil }
func s3StateFilePolicy(_ int, _ bool) error { return nil }
func s3PrepareStateRootPolicy(_ int) error  { return nil }
