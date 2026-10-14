// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//go:build linux

package main

import (
	"bytes"
	"errors"
	"os"
	"unsafe"

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

	if fi.Mode()&os.ModeSymlink != 0 {
		fd, err := unix.Openat(parentFD, leaf, unix.O_PATH|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if err != nil {
			return nil, err
		}
		defer unix.Close(fd)

		empty := []byte{0}
		sz, _, errno := unix.Syscall6(
			unix.SYS_LISTXATTRAT,
			uintptr(fd),
			uintptr(unsafe.Pointer(&empty[0])),
			0,
			0,
			uintptr(unix.AT_EMPTY_PATH|unix.AT_SYMLINK_NOFOLLOW),
			0,
		)
		if errno != 0 {
			if errno == unix.ENOTSUP || errno == unix.EOPNOTSUPP || errno == unix.ENODATA {
				return nil, nil
			}
			return nil, errno
		}
		if int(sz) <= 0 {
			return nil, nil
		}

		buf := make([]byte, sz)
		sz2, _, errno := unix.Syscall6(
			unix.SYS_LISTXATTRAT,
			uintptr(fd),
			uintptr(unsafe.Pointer(&empty[0])),
			uintptr(unsafe.Pointer(&buf[0])),
			uintptr(sz),
			uintptr(unix.AT_EMPTY_PATH|unix.AT_SYMLINK_NOFOLLOW),
			0,
		)
		if errno != 0 {
			if errno == unix.ENOTSUP || errno == unix.EOPNOTSUPP || errno == unix.ENODATA {
				return nil, nil
			}
			return nil, errno
		}

		var attrs []data.ExtendedAttribute
		rawNames := bytes.Split(buf[:sz2], []byte{0})
		for _, rawName := range rawNames {
			if len(rawName) == 0 {
				continue
			}
			name := string(rawName)
			cname, err := unix.BytePtrFromString(name)
			if err != nil {
				return nil, err
			}
			vsz, _, errno := unix.Syscall6(
				unix.SYS_GETXATTRAT,
				uintptr(fd),
				uintptr(unsafe.Pointer(&empty[0])),
				uintptr(unsafe.Pointer(cname)),
				0,
				0,
				uintptr(unix.AT_EMPTY_PATH|unix.AT_SYMLINK_NOFOLLOW),
			)
			if errno != 0 {
				if errno == unix.ENODATA {
					continue
				}
				if errno == unix.ENOTSUP || errno == unix.EOPNOTSUPP {
					return nil, nil
				}
				return nil, errno
			}
			if int(vsz) == 0 {
				attrs = append(attrs, data.ExtendedAttribute{Name: name, Value: []byte{}})
				continue
			}
			vbuf := make([]byte, vsz)
			_, _, errno = unix.Syscall6(
				unix.SYS_GETXATTRAT,
				uintptr(fd),
				uintptr(unsafe.Pointer(&empty[0])),
				uintptr(unsafe.Pointer(cname)),
				uintptr(unsafe.Pointer(&vbuf[0])),
				uintptr(vsz),
				uintptr(unix.AT_EMPTY_PATH|unix.AT_SYMLINK_NOFOLLOW),
			)
			if errno != 0 {
				if errno == unix.ENODATA {
					continue
				}
				if errno == unix.ENOTSUP || errno == unix.EOPNOTSUPP {
					return nil, nil
				}
				return nil, errno
			}
			attrs = append(attrs, data.ExtendedAttribute{Name: name, Value: vbuf})
		}
		return attrs, nil
	}

	fd, err := unix.Openat(parentFD, leaf, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
	if err != nil {
		fd, err = unix.Openat(parentFD, leaf, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if err != nil {
			return nil, err
		}
	}
	defer unix.Close(fd)

	sz, err := unix.Flistxattr(fd, nil)
	if err != nil {
		if errors.Is(err, unix.ENOTSUP) || errors.Is(err, unix.EOPNOTSUPP) || errors.Is(err, unix.ENODATA) {
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
		if errors.Is(err, unix.ENOTSUP) || errors.Is(err, unix.EOPNOTSUPP) || errors.Is(err, unix.ENODATA) {
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
			if errors.Is(err, unix.ENODATA) {
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
			if errors.Is(err, unix.ENODATA) {
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
