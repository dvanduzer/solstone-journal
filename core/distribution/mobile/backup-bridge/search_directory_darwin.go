//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <fcntl.h>
*/
import "C"

import (
	"errors"
	"os"
	"strings"

	"golang.org/x/sys/unix"
)

// The SDK supplies search-only authority for protected iOS ancestors. The
// selected endpoint remains readable; no weaker open is attempted on failure.
func openSearchPinnedDirectory(path string) (*os.File, error) {
	if err := sanitizePath(path); err != nil {
		return nil, err
	}
	searchFlags := int(C.O_SEARCH) | unix.O_NOFOLLOW | unix.O_CLOEXEC
	rootFlags := searchFlags
	if path == "/" {
		rootFlags = unix.O_RDONLY | unix.O_DIRECTORY | unix.O_NOFOLLOW | unix.O_CLOEXEC
	}
	fd, err := unix.Open("/", rootFlags, 0)
	if err != nil {
		return nil, err
	}
	components := strings.Split(strings.TrimPrefix(path, "/"), "/")
	for i, component := range components {
		if component == "" {
			continue
		}
		flags := searchFlags
		if i == len(components)-1 {
			flags = unix.O_RDONLY | unix.O_DIRECTORY | unix.O_NOFOLLOW | unix.O_CLOEXEC
		}
		next, openErr := unix.Openat(fd, component, flags, 0)
		_ = unix.Close(fd)
		if openErr != nil {
			return nil, openErr
		}
		fd = next
	}
	file := os.NewFile(uintptr(fd), path)
	if file == nil {
		_ = unix.Close(fd)
		return nil, errors.New("could not retain directory descriptor")
	}
	return file, nil
}

type searchDirectory struct {
	fd   int
	stat unix.Stat_t
}

func searchDirectoryStat(fd int) (unix.Stat_t, error) {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return stat, err
	}
	if stat.Mode&unix.S_IFMT != unix.S_IFDIR {
		return stat, errPathNotDir
	}
	return stat, nil
}
func sameSearchDirectory(a, b unix.Stat_t) bool {
	return a.Dev == b.Dev && a.Ino == b.Ino
}
func closeSearchDirectoryChain(chain []searchDirectory) {
	for _, dir := range chain {
		_ = unix.Close(dir.fd)
	}
}

func buildSearchDirectoryChain(descendant *os.File, limit int, afterParent func(int, int) error) (chain []searchDirectory, resultErr error) {
	if limit < 1 || limit > 128 {
		return nil, errors.New("invalid directory ancestry bound")
	}
	first, err := unix.Dup(int(descendant.Fd()))
	if err != nil {
		return nil, err
	}
	unix.CloseOnExec(first)
	chain = []searchDirectory{{fd: first}}
	defer func() {
		if resultErr != nil {
			closeSearchDirectoryChain(chain)
		}
	}()
	chain[0].stat, err = searchDirectoryStat(first)
	if err != nil {
		return chain, err
	}
	if afterParent != nil {
		if err := afterParent(0, first); err != nil {
			return chain, err
		}
	}
	for len(chain) < limit {
		current := chain[len(chain)-1]
		parent, err := unix.Openat(current.fd, "..", int(C.O_SEARCH)|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if err != nil {
			return chain, err
		}
		parentStat, err := searchDirectoryStat(parent)
		if err != nil {
			_ = unix.Close(parent)
			return chain, err
		}
		if sameSearchDirectory(current.stat, parentStat) {
			_ = unix.Close(parent)
			return chain, nil
		}
		chain = append(chain, searchDirectory{fd: parent, stat: parentStat})
		if afterParent != nil {
			if err := afterParent(len(chain)-1, parent); err != nil {
				return chain, err
			}
		}
	}
	return chain, errors.New("directory ancestry exceeds bound")
}

func validateSearchDirectoryChain(chain []searchDirectory) error {
	for i, child := range chain {
		expected := child.stat
		if i+1 < len(chain) {
			expected = chain[i+1].stat
		}
		var actual unix.Stat_t
		if err := unix.Fstatat(child.fd, "..", &actual, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return err
		}
		if !sameSearchDirectory(actual, expected) {
			return errPinnedIdentity
		}
	}
	return nil
}
func searchChainContains(chain []searchDirectory, endpoint unix.Stat_t) bool {
	for _, dir := range chain {
		if sameSearchDirectory(dir.stat, endpoint) {
			return true
		}
	}
	return false
}

func directoryContainsSearch(ancestor, descendant *os.File) (bool, error) {
	return directoryContainsSearchBounded(ancestor, descendant, 128, nil)
}

// Finite link revalidation detects ordinary namespace instability. It does not
// promise an atomic namespace snapshot against arbitrary same-UID ABA mutation.
func directoryContainsSearchBounded(ancestor, descendant *os.File, limit int, afterParent func(int) error) (bool, error) {
	ancestorStat, err := searchDirectoryStat(int(ancestor.Fd()))
	if err != nil {
		return false, err
	}
	var hook func(int, int) error
	if afterParent != nil {
		hook = func(depth, fd int) error { return afterParent(depth) }
	}
	chain, err := buildSearchDirectoryChain(descendant, limit, hook)
	if err != nil {
		return false, err
	}
	defer closeSearchDirectoryChain(chain)
	if searchChainContains(chain, ancestorStat) {
		return true, nil
	}
	if err := validateSearchDirectoryChain(chain); err != nil {
		return false, err
	}
	return false, nil
}

func searchDirectoriesOverlap(a, b *os.File) (bool, error) {
	return searchDirectoriesOverlapBounded(a, b, 128, nil)
}

// Both chains remain owned through the single bidirectional decision. A false
// first direction cannot be released and forgotten before the reverse scan.
func searchDirectoriesOverlapBounded(a, b *os.File, limit int, afterFirst func() error) (bool, error) {
	aChain, err := buildSearchDirectoryChain(a, limit, nil)
	if err != nil {
		return false, err
	}
	defer closeSearchDirectoryChain(aChain)
	if afterFirst != nil {
		if err := afterFirst(); err != nil {
			return false, err
		}
	}
	bChain, err := buildSearchDirectoryChain(b, limit, nil)
	if err != nil {
		return false, err
	}
	defer closeSearchDirectoryChain(bChain)
	if searchChainContains(aChain, bChain[0].stat) || searchChainContains(bChain, aChain[0].stat) {
		return true, nil
	}
	if err := validateSearchDirectoryChain(aChain); err != nil {
		return false, err
	}
	if err := validateSearchDirectoryChain(bChain); err != nil {
		return false, err
	}
	return false, nil
}
