//go:build !ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"os"
	"strings"

	"golang.org/x/sys/unix"
)

// openPinnedDirectory resolves every component relative to the already-opened
// parent with O_NOFOLLOW. The returned descriptor names the selected directory
// even if a caller later replaces any component of the original path.
func openPinnedDirectory(path string) (*os.File, error) {
	if err := sanitizePath(path); err != nil {
		return nil, err
	}
	flags := unix.O_RDONLY | unix.O_DIRECTORY | unix.O_NOFOLLOW | unix.O_CLOEXEC
	fd, err := unix.Open("/", flags, 0)
	if err != nil {
		return nil, err
	}
	for _, component := range strings.Split(strings.TrimPrefix(path, "/"), "/") {
		if component == "" {
			continue
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

func directoryContains(ancestor, descendant *os.File) (bool, error) {
	current, err := unix.Dup(int(descendant.Fd()))
	if err != nil {
		return false, err
	}

	var ancestorStat unix.Stat_t
	if err := unix.Fstat(int(ancestor.Fd()), &ancestorStat); err != nil {
		_ = unix.Close(current)
		return false, err
	}
	for {
		var currentStat unix.Stat_t
		if err := unix.Fstat(current, &currentStat); err != nil {
			_ = unix.Close(current)
			return false, err
		}
		if ancestorStat.Dev == currentStat.Dev && ancestorStat.Ino == currentStat.Ino {
			_ = unix.Close(current)
			return true, nil
		}
		parent, openErr := unix.Openat(current, "..", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if openErr != nil {
			_ = unix.Close(current)
			return false, openErr
		}
		var parentStat unix.Stat_t
		if err := unix.Fstat(parent, &parentStat); err != nil {
			_ = unix.Close(parent)
			_ = unix.Close(current)
			return false, err
		}
		if currentStat.Dev == parentStat.Dev && currentStat.Ino == parentStat.Ino {
			_ = unix.Close(parent)
			_ = unix.Close(current)
			return false, nil
		}
		_ = unix.Close(current)
		current = parent
	}
}

func pinnedDirectoriesOverlap(a, b *os.File) (bool, error) {
	if same, err := sameDirectory(a, b); err != nil || same {
		return same, err
	}
	aContainsB, err := directoryContains(a, b)
	if err != nil || aContainsB {
		return aContainsB, err
	}
	return directoryContains(b, a)
}
