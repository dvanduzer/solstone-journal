// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build linux

package main

import (
	"errors"
	"fmt"
	"golang.org/x/sys/unix"
	"os"
)

func openMetadataFile(root *os.Root, rel string) (*os.File, error) {
	base, err := root.OpenFile(".", os.O_RDONLY, 0)
	if err != nil {
		return nil, err
	}
	defer base.Close()
	parent, leaf, closeParent, err := openatParent(int(base.Fd()), rel)
	if err != nil {
		return nil, err
	}
	defer closeParent()
	// O_PATH never opens a replacement FIFO or device for I/O.
	fd, err := unix.Openat(parent, leaf, unix.O_PATH|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	return os.NewFile(uintptr(fd), rel), nil
}

func readableMetadataFile(file *os.File) (*os.File, error) {
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() && !info.IsDir() {
		return nil, errors.New("source object is not readable file or directory")
	}
	// Following this procfs magic link reopens the held object, including after
	// its original directory entry is replaced. No source pathname is resolved.
	fd, err := unix.Open(fmt.Sprintf("/proc/self/fd/%d", file.Fd()), unix.O_RDONLY|unix.O_NONBLOCK|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	return os.NewFile(uintptr(fd), file.Name()), nil
}

func readMetadataLink(_ *os.Root, file *os.File) (string, error) {
	for size := 256; size <= 65536; size *= 2 {
		buf := make([]byte, size)
		n, err := unix.Readlinkat(int(file.Fd()), "", buf)
		if err != nil {
			return "", err
		}
		if n < len(buf) {
			return string(buf[:n]), nil
		}
	}
	return "", errors.New("source symlink target exceeds bound")
}

func readMetadataLinkOwned(workspace *os.Root, file *os.File, _ *ownedCall) (string, error) {
	return readMetadataLink(workspace, file)
}
