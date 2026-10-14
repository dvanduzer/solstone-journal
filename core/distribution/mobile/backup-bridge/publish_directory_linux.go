//go:build linux

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import "golang.org/x/sys/unix"

func publishDirectory(sourceFD int, stage string, destinationFD int, destination string) error {
	return unix.Renameat2(sourceFD, stage, destinationFD, destination, unix.RENAME_NOREPLACE)
}

func clearFileFlags(_ int) error { return nil }

func openCleanupEntryAt(parentFD int, name string, mode uint32) (int, error) {
	flags := unix.O_PATH | unix.O_NOFOLLOW | unix.O_CLOEXEC
	if mode&unix.S_IFMT == unix.S_IFDIR {
		flags |= unix.O_DIRECTORY
	}
	return unix.Openat(parentFD, name, flags, 0)
}
