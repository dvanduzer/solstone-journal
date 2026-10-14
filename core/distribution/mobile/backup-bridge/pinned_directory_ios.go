//go:build ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import "os"

func openPinnedDirectory(path string) (*os.File, error) {
	return openSearchPinnedDirectory(path)
}

func directoryContains(ancestor, descendant *os.File) (bool, error) {
	return directoryContainsSearch(ancestor, descendant)
}

func pinnedDirectoriesOverlap(a, b *os.File) (bool, error) {
	return searchDirectoriesOverlap(a, b)
}
