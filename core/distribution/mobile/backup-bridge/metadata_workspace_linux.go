// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build linux

package main

import (
	"fmt"
	"golang.org/x/sys/unix"
)

func chmodRetainedMetadataDirectory(fd int, mode uint32) error {
	return unix.Chmod(fmt.Sprintf("/proc/self/fd/%d", fd), mode)
}

func metadataRequiresSameVolume() bool { return false }
