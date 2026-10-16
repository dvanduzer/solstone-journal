//go:build ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <fcntl.h>
*/
import "C"
import "golang.org/x/sys/unix"

func ownedDirectoryFlags(endpoint bool) int {
	if endpoint {
		return unix.O_RDONLY | unix.O_DIRECTORY | unix.O_NOFOLLOW | unix.O_CLOEXEC
	}
	return int(C.O_SEARCH) | unix.O_NOFOLLOW | unix.O_CLOEXEC
}
