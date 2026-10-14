// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//go:build darwin || ios

package main

import (
	"bytes"
	"errors"
	"os"
	"syscall"

	"github.com/restic/restic/internal/data"
	"golang.org/x/sys/unix"
)

func readNodeXattrs(root *os.Root, relPath string, fi os.FileInfo) ([]data.ExtendedAttribute, error) {
	rootFile, err := root.OpenFile(".", os.O_RDONLY, 0)
	if err != nil {
		return nil, err
	}
	defer rootFile.Close()

	parentFD, leaf, closeParent, err := openatParent(int(rootFile.Fd()), relPath)
	if err != nil {
		return nil, err
	}
	defer closeParent()

	var openFlags int = unix.O_RDONLY | unix.O_NOFOLLOW | unix.O_CLOEXEC
	if fi.Mode()&os.ModeSymlink != 0 {
		openFlags = unix.O_SYMLINK | unix.O_NOFOLLOW | unix.O_CLOEXEC
	}

	fd, err := unix.Openat(parentFD, leaf, openFlags, 0)
	if err != nil {
		fd, err = unix.Openat(parentFD, leaf, openFlags|unix.O_DIRECTORY, 0)
		if err != nil {
			return nil, err
		}
	}
	defer unix.Close(fd)

	sz, err := unix.Flistxattr(fd, nil)
	if err != nil {
		if errors.Is(err, unix.ENOTSUP) || errors.Is(err, unix.EOPNOTSUPP) || errors.Is(err, syscall.ENOATTR) {
			return nil, nil
		}
		return nil, err
	}
	if sz <= 0 {
		return nil, nil
	}

	buf := make([]byte, sz)
	sz2, err := unix.Flistxattr(fd, buf)
	if err != nil {
		if errors.Is(err, unix.ENOTSUP) || errors.Is(err, unix.EOPNOTSUPP) || errors.Is(err, syscall.ENOATTR) {
			return nil, nil
		}
		return nil, err
	}

	var attrs []data.ExtendedAttribute
	rawNames := bytes.Split(buf[:sz2], []byte{0})
	for _, rawName := range rawNames {
		if len(rawName) == 0 {
			continue
		}
		name := string(rawName)
		vsz, err := unix.Fgetxattr(fd, name, nil)
		if err != nil {
			if errors.Is(err, syscall.ENOATTR) {
				continue
			}
			if errors.Is(err, unix.ENOTSUP) || errors.Is(err, unix.EOPNOTSUPP) {
				return nil, nil
			}
			return nil, err
		}
		if vsz == 0 {
			attrs = append(attrs, data.ExtendedAttribute{Name: name, Value: []byte{}})
			continue
		}
		vbuf := make([]byte, vsz)
		_, err = unix.Fgetxattr(fd, name, vbuf)
		if err != nil {
			if errors.Is(err, syscall.ENOATTR) {
				continue
			}
			if errors.Is(err, unix.ENOTSUP) || errors.Is(err, unix.EOPNOTSUPP) {
				return nil, nil
			}
			return nil, err
		}
		attrs = append(attrs, data.ExtendedAttribute{Name: name, Value: vbuf})
	}
	return attrs, nil
}
