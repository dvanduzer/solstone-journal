//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import "golang.org/x/sys/unix"

func publishDirectory(sourceFD int, stage string, destinationFD int, destination string) error {
	return unix.RenameatxNp(sourceFD, stage, destinationFD, destination, unix.RENAME_EXCL)
}

func clearFileFlags(fd int) error { return unix.Fchflags(fd, 0) }

func openCleanupEntryAt(parentFD int, name string, mode uint32) (int, error) {
	flags := unix.O_EVTONLY | unix.O_NOFOLLOW | unix.O_CLOEXEC
	switch mode & unix.S_IFMT {
	case unix.S_IFDIR:
		flags |= unix.O_DIRECTORY
	case unix.S_IFLNK:
		flags |= unix.O_SYMLINK
	}
	return unix.Openat(parentFD, name, flags, 0)
}
