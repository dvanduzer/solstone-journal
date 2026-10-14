// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build darwin || ios || linux

package main

import (
	"bytes"
	"errors"
	"fmt"
	"github.com/restic/restic/internal/data"
	"golang.org/x/sys/unix"
)

// An unsupported initial list means this object has no accessible xattr
// facility. Once names are listed, every named value is required.
func collectNodeXattrs(list func([]byte) (int, error), get func(string, []byte) (int, error)) ([]data.ExtendedAttribute, error) {
	const maxMetadataBytes = 64 << 20
	size, err := list(nil)
	if errors.Is(err, unix.ENOTSUP) || errors.Is(err, unix.EOPNOTSUPP) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if size == 0 {
		return nil, nil
	}
	if size < 0 || size > maxMetadataBytes {
		return nil, errors.New("xattr list exceeds bound")
	}
	names := make([]byte, size)
	n, err := list(names)
	if err != nil {
		return nil, err
	}
	if n < 0 || n > len(names) {
		return nil, errors.New("invalid xattr list length")
	}
	names = names[:n]
	if n == 0 {
		return nil, nil
	}
	if names[n-1] != 0 {
		return nil, errors.New("unterminated xattr list")
	}
	attrs := make([]data.ExtendedAttribute, 0)
	remaining := maxMetadataBytes - len(names)
	for len(names) > 0 {
		end := bytes.IndexByte(names, 0)
		rawName := names[:end]
		names = names[end+1:]
		if len(rawName) == 0 {
			continue
		}
		name := string(rawName)
		size, err := get(name, nil)
		if err != nil {
			return nil, err
		}
		if size < 0 || size > remaining-len(name)-96 {
			return nil, errors.New("aggregate xattr metadata exceeds bound")
		}
		remaining -= size + len(name) + 96
		value := make([]byte, size)
		n, err := get(name, value)
		if err != nil {
			return nil, err
		}
		if n != len(value) {
			return nil, fmt.Errorf("xattr value length changed")
		}
		attrs = append(attrs, data.ExtendedAttribute{Name: name, Value: value})
	}
	return attrs, nil
}
