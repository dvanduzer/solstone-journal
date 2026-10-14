// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build darwin || ios

package main

import "golang.org/x/sys/unix"

func chmodRetainedMetadataDirectory(fd int, mode uint32) error { return unix.Fchmod(fd, mode) }

func metadataRequiresSameVolume() bool { return true }
