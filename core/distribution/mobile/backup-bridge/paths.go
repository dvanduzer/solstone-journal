// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
)

var (
	errPathEmpty                 = errors.New("path is empty")
	errPathTooLong               = errors.New("path exceeds maximum length")
	errPathNulByte               = errors.New("path contains NUL byte")
	errPathNotAbsolute           = errors.New("path must be absolute")
	errPathNotClean              = errors.New("path is not canonical")
	errPathDoubleSlash           = errors.New("path begins with double slash")
	errPathSchemeDetected        = errors.New("path contains URI scheme or remote backend prefix")
	errPathSymlink               = errors.New("path component is a symlink")
	errPathNotDir                = errors.New("path is not a directory")
	errPathNotExist              = errors.New("path does not exist")
	errRestoreExists             = errors.New("restore destination already exists")
	errRestoreCommittedUncertain = errors.New("restore published but directory sync failed")
	errRestoreStageChanged       = errors.New("restore staging identity changed")
	errRestoreNonEmpty           = errors.New("restore destination is not empty")
	errPathOverlap               = errors.New("paths must not overlap or enclose each other")
	errPinnedIdentity            = errors.New("pinned path identity mismatch")
)

var knownSchemes = []string{
	"local:", "s3:", "sftp:", "rclone:", "rest:", "b2:", "azure:", "gs:", "swift:",
}

// sanitizePath validates basic syntactic safety of a path before inspection.
func sanitizePath(p string) error {
	if len(p) == 0 {
		return errPathEmpty
	}
	if len(p) > maxPathLen {
		return errPathTooLong
	}
	if strings.ContainsRune(p, '\x00') {
		return errPathNulByte
	}
	if strings.HasPrefix(p, "//") {
		return errPathDoubleSlash
	}
	if strings.Contains(p, "://") {
		return errPathSchemeDetected
	}
	for _, scheme := range knownSchemes {
		if strings.HasPrefix(p, scheme) {
			return errPathSchemeDetected
		}
	}
	if !filepath.IsAbs(p) {
		return errPathNotAbsolute
	}
	if filepath.Clean(p) != p {
		return errPathNotClean
	}
	return nil
}

// checkNoSymlinks walks all path components from root to target and verifies
// that none of the existing components are symlinks.
func checkNoSymlinks(p string) error {
	parts := strings.Split(p, string(filepath.Separator))
	current := string(filepath.Separator)

	for _, part := range parts {
		if part == "" {
			continue
		}
		current = filepath.Join(current, part)
		fi, err := os.Lstat(current)
		if err != nil {
			if os.IsNotExist(err) {
				// Remaining components do not exist yet; stop checking.
				break
			}
			return err
		}
		if fi.Mode()&os.ModeSymlink != 0 {
			return errPathSymlink
		}
	}
	return nil
}

// validateExistingDir verifies that p exists, is a directory, and contains no symlinks.
func validateExistingDir(p string) error {
	if err := sanitizePath(p); err != nil {
		return err
	}
	if err := checkNoSymlinks(p); err != nil {
		return err
	}
	fi, err := os.Stat(p)
	if err != nil {
		return err
	}
	if !fi.IsDir() {
		return errPathNotDir
	}
	return nil
}

// validateRestoreDest verifies that dst is absent and its parent is an existing,
// symlink-free directory. The bridge publishes a new private directory atomically;
// it never replaces caller-owned destination metadata or concurrent data.
func validateRestoreDest(dst string) error {
	if err := sanitizePath(dst); err != nil {
		return err
	}
	if err := checkNoSymlinks(dst); err != nil {
		return err
	}

	_, err := os.Lstat(dst)
	if err == nil {
		return errRestoreExists
	}
	if !os.IsNotExist(err) {
		return err
	}

	// Destination does not exist: ensure parent exists and is a directory.
	parent := filepath.Dir(dst)
	parentFi, err := os.Stat(parent)
	if err != nil {
		return err
	}
	if !parentFi.IsDir() {
		return errPathNotDir
	}
	return nil
}

// pathsOverlap checks whether a equals b or either path is enclosed within the other.
func pathsOverlap(a, b string) bool {
	if a == b {
		return true
	}
	relAtoB, err1 := filepath.Rel(a, b)
	if err1 == nil && relAtoB != "." && relAtoB != ".." && !strings.HasPrefix(relAtoB, ".."+string(filepath.Separator)) && !filepath.IsAbs(relAtoB) {
		// b is inside a
		return true
	}
	relBtoA, err2 := filepath.Rel(b, a)
	if err2 == nil && relBtoA != "." && relBtoA != ".." && !strings.HasPrefix(relBtoA, ".."+string(filepath.Separator)) && !filepath.IsAbs(relBtoA) {
		// a is inside b
		return true
	}
	return false
}
