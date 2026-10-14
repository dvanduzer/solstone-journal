// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build darwin || ios

package main

import (
	"github.com/restic/restic/internal/data"
	"golang.org/x/sys/unix"
	"os"
)

func readNodeFileXattrs(_ *os.Root, _ string, file *os.File) ([]data.ExtendedAttribute, error) {
	fd := int(file.Fd())
	return collectNodeXattrs(func(buf []byte) (int, error) { return unix.Flistxattr(fd, buf) },
		func(name string, buf []byte) (int, error) { return unix.Fgetxattr(fd, name, buf) })
}
