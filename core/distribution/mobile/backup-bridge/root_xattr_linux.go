// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build linux

package main

import (
	"fmt"
	"github.com/restic/restic/internal/data"
	"golang.org/x/sys/unix"
	"os"
	"runtime"
)

func readNodeFileXattrs(_ *os.Root, _ string, file *os.File) ([]data.ExtendedAttribute, error) {
	// A followed procfs magic link addresses the held inode itself, even when
	// that inode is a symlink. Llistxattr would instead inspect the procfs link.
	path := fmt.Sprintf("/proc/self/fd/%d", file.Fd())
	attrs, err := collectNodeXattrs(func(buf []byte) (int, error) { return unix.Listxattr(path, buf) },
		func(name string, buf []byte) (int, error) { return unix.Getxattr(path, name, buf) })
	runtime.KeepAlive(file)
	return attrs, err
}
