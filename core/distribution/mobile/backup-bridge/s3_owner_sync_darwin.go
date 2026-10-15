// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import "golang.org/x/sys/unix"

func s3StateSyncFile(fd int) error {
	if err := unix.Fsync(fd); err != nil {
		return err
	}
	_, err := unix.FcntlInt(uintptr(fd), unix.F_FULLFSYNC, 0)
	return err
}

func s3StateReplace(fd int, source, target string, replacing bool) error {
	flags := uint32(unix.RENAME_EXCL)
	if replacing {
		flags = unix.RENAME_SWAP
	}
	return unix.RenameatxNp(fd, source, fd, target, flags)
}
