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
	flags := unix.O_EVTONLY | unix.O_NOFOLLOW | unix.O_CLOEXEC | unix.O_NONBLOCK
	switch mode & unix.S_IFMT {
	case unix.S_IFDIR:
		flags |= unix.O_DIRECTORY
	case unix.S_IFLNK:
		flags = unix.O_EVTONLY | unix.O_SYMLINK | unix.O_CLOEXEC | unix.O_NONBLOCK
	}
	fd, err := unix.Openat(parentFD, name, flags, 0)
	if err == unix.EACCES && mode&unix.S_IFMT != unix.S_IFLNK {
		// This is the retained private staging namespace. Re-enable traversal
		// without following a replacement symlink; the caller checks dev/inode
		// against its pre-open identity before touching or traversing the fd.
		permissions := uint32(0600)
		if mode&unix.S_IFMT == unix.S_IFDIR {
			permissions = 0700
		}
		if err = unix.Fchmodat(parentFD, name, permissions, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return -1, err
		}
		return unix.Openat(parentFD, name, flags, 0)
	}
	return fd, err
}
