//go:build darwin || ios || linux

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/unix"
)

const restoreStagePrefix = ".solstone-restore-staging-"

var syncDirectory = unix.Fsync

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

func adoptRoot(path string, pinned *os.File) (*os.Root, error) {
	root, err := os.OpenRoot(path)
	if err != nil {
		return nil, errPinnedIdentity
	}
	rootInfo, err := root.Stat(".")
	if err != nil {
		_ = root.Close()
		return nil, errPinnedIdentity
	}
	pinnedInfo, err := pinned.Stat()
	if err != nil {
		_ = root.Close()
		return nil, errPinnedIdentity
	}
	if !os.SameFile(rootInfo, pinnedInfo) {
		_ = root.Close()
		return nil, errPinnedIdentity
	}
	return root, nil
}

func sameDirectory(a, b *os.File) (bool, error) {
	aInfo, err := a.Stat()
	if err != nil {
		return false, err
	}
	bInfo, err := b.Stat()
	if err != nil {
		return false, err
	}
	return os.SameFile(aInfo, bInfo), nil
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

func ensureDirectoryEmpty(file *os.File) error {
	copyFD, err := unix.Dup(int(file.Fd()))
	if err != nil {
		return err
	}
	copyFile := os.NewFile(uintptr(copyFD), "restore destination")
	defer copyFile.Close()
	entries, err := copyFile.Readdirnames(1)
	if err == nil || len(entries) != 0 {
		return errRestoreNonEmpty
	}
	if !errors.Is(err, io.EOF) {
		return err
	}
	return nil
}

func pinRequestPaths(req *bridgeRequest) (err error) {
	defer func() {
		if err != nil {
			_ = closeRequestPaths(req)
		}
	}()
	repo, err := openPinnedDirectory(req.repoPath)
	if err != nil {
		if req.operation == OpInit && (errors.Is(err, unix.ENOENT) || os.IsNotExist(err)) {
			if mkErr := os.MkdirAll(req.repoPath, 0700); mkErr != nil {
				return mkErr
			}
			repo, err = openPinnedDirectory(req.repoPath)
		}
		if err != nil {
			return err
		}
	}
	req.pathFiles = append(req.pathFiles, repo)
	repoRoot, err := adoptRoot(req.repoPath, repo)
	if err != nil {
		return err
	}
	req.repoRoot = repoRoot

	var source *os.File
	if req.operation == OpSnapshot || req.operation == OpRestore {
		originalSource := req.sourcePath
		source, err = openPinnedDirectory(originalSource)
		if err != nil {
			return err
		}
		req.pathFiles = append(req.pathFiles, source)
		if overlap, overlapErr := pinnedDirectoriesOverlap(repo, source); overlapErr != nil || overlap {
			if overlapErr != nil {
				return overlapErr
			}
			return errPathOverlap
		}
		sourceRoot, err := adoptRoot(originalSource, source)
		if err != nil {
			return err
		}
		req.sourceRoot = sourceRoot
		sourceName := filepath.Base(originalSource)
		if sourceName == string(filepath.Separator) || sourceName == "." {
			return errPathOverlap
		}
		req.sourceName = sourceName
	}

	if req.operation == OpRestore {
		originalRestore := req.restorePath
		parentPath := filepath.Dir(originalRestore)
		parent, openErr := openPinnedDirectory(parentPath)
		if openErr != nil {
			return openErr
		}
		req.pathFiles = append(req.pathFiles, parent)
		req.restoreParentPath = parentPath
		name := filepath.Base(originalRestore)
		var destinationStat unix.Stat_t
		if statErr := unix.Fstatat(int(parent.Fd()), name, &destinationStat, unix.AT_SYMLINK_NOFOLLOW); statErr == nil {
			return errRestoreExists
		} else if !errors.Is(statErr, unix.ENOENT) {
			return statErr
		}
		for _, other := range []*os.File{repo, source} {
			if other == nil {
				continue
			}
			insideOther, overlapErr := directoryContains(other, parent)
			if overlapErr != nil {
				return overlapErr
			}
			if insideOther {
				return errPathOverlap
			}
		}
		stageRootName, nameErr := newRestoreStageName()
		if nameErr != nil {
			return nameErr
		}
		if err := unix.Mkdirat(int(parent.Fd()), stageRootName, 0700); err != nil {
			return err
		}
		stageRootFD, openErr := unix.Openat(int(parent.Fd()), stageRootName, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if openErr != nil {
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return openErr
		}
		stageRoot := os.NewFile(uintptr(stageRootFD), "restore staging root")
		if stageRoot == nil {
			_ = unix.Close(stageRootFD)
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return errors.New("could not retain restore staging root")
		}
		stageName, nameErr := newRestoreStageName()
		if nameErr != nil {
			_ = stageRoot.Close()
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return nameErr
		}
		if err := unix.Mkdirat(stageRootFD, stageName, 0700); err != nil {
			_ = stageRoot.Close()
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return err
		}
		stageFD, openErr := unix.Openat(stageRootFD, stageName, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if openErr != nil {
			_ = unix.Unlinkat(stageRootFD, stageName, unix.AT_REMOVEDIR)
			_ = stageRoot.Close()
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return openErr
		}
		stage := os.NewFile(uintptr(stageFD), "restore staging payload")
		if stage == nil {
			_ = unix.Close(stageFD)
			_ = unix.Unlinkat(stageRootFD, stageName, unix.AT_REMOVEDIR)
			_ = stageRoot.Close()
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return errors.New("could not retain restore staging payload")
		}
		stagePath := filepath.Join(parentPath, stageRootName, stageName)
		restoreRoot, adoptErr := adoptRoot(stagePath, stage)
		if adoptErr != nil {
			_ = stage.Close()
			_ = unix.Unlinkat(stageRootFD, stageName, unix.AT_REMOVEDIR)
			_ = stageRoot.Close()
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return adoptErr
		}
		if err := unix.Fchmod(stageRootFD, 0300); err != nil {
			_ = restoreRoot.Close()
			_ = stage.Close()
			_ = unix.Unlinkat(stageRootFD, stageName, unix.AT_REMOVEDIR)
			_ = stageRoot.Close()
			_ = unix.Unlinkat(int(parent.Fd()), stageRootName, unix.AT_REMOVEDIR)
			return err
		}
		req.pathFiles = append(req.pathFiles, stageRoot, stage)
		req.restoreRoot = restoreRoot
		req.restoreParent = parent
		req.restoreStageRoot = stageRoot
		req.restoreStage = stage
		req.restoreName = name
		req.restoreStageRootName = stageRootName
		req.restoreStageName = stageName
	}

	req.pathsPinned = true
	return nil
}

func newRestoreStageName() (string, error) {
	var random [16]byte
	if _, err := rand.Read(random[:]); err != nil {
		return "", err
	}
	return restoreStagePrefix + hex.EncodeToString(random[:]), nil
}

func pinnedEntryMatches(parentFD int, name string, pinned *os.File) (bool, error) {
	var entryStat, pinnedStat unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &entryStat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		if errors.Is(err, unix.ENOENT) {
			return false, nil
		}
		return false, err
	}
	if err := unix.Fstat(int(pinned.Fd()), &pinnedStat); err != nil {
		return false, err
	}
	return entryStat.Dev == pinnedStat.Dev && entryStat.Ino == pinnedStat.Ino, nil
}

// clearDirectoryContents cleans the retained private directory itself. Each
// child is opened and checked against its descriptor immediately before unlink.
// The containing staging root is non-enumerable and its random payload name is
// never returned across the callable boundary.
func clearDirectoryContents(fd int) error {
	if err := clearFileFlags(fd); err != nil {
		return err
	}
	if err := unix.Fchmod(fd, 0700); err != nil {
		return err
	}
	copyFD, err := unix.Dup(fd)
	if err != nil {
		return err
	}
	listing := os.NewFile(uintptr(copyFD), "restore cleanup directory")
	names, readErr := listing.Readdirnames(-1)
	_ = listing.Close()
	if readErr != nil {
		return readErr
	}
	for _, child := range names {
		var stat unix.Stat_t
		if err := unix.Fstatat(fd, child, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return err
		}
		if stat.Mode&unix.S_IFMT == unix.S_IFDIR {
			identityFD, err := openCleanupEntryAt(fd, child, uint32(stat.Mode))
			if err != nil {
				return err
			}
			identityFile := os.NewFile(uintptr(identityFD), "restore cleanup directory identity")
			if identityFile == nil {
				_ = unix.Close(identityFD)
				return errors.New("could not retain restore cleanup directory identity")
			}
			var openedStat unix.Stat_t
			if err := unix.Fstat(identityFD, &openedStat); err != nil || openedStat.Dev != stat.Dev || openedStat.Ino != stat.Ino {
				_ = identityFile.Close()
				if err != nil {
					return err
				}
				return errRestoreStageChanged
			}
			if err := clearFileFlags(identityFD); err != nil {
				_ = identityFile.Close()
				return err
			}
			matches, err := pinnedEntryMatches(fd, child, identityFile)
			_ = identityFile.Close()
			if err != nil {
				return err
			}
			if !matches {
				return errRestoreStageChanged
			}
			if err := unix.Fchmodat(fd, child, 0700, 0); err != nil {
				return err
			}
			childFD, err := unix.Openat(fd, child, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
			if err != nil {
				return err
			}
			childFile := os.NewFile(uintptr(childFD), "restore cleanup child")
			if childFile == nil {
				_ = unix.Close(childFD)
				return errors.New("could not retain restore cleanup child")
			}
			if err := clearDirectoryContents(childFD); err != nil {
				_ = childFile.Close()
				return err
			}
			matches, err = pinnedEntryMatches(fd, child, childFile)
			_ = childFile.Close()
			if err != nil {
				return err
			}
			if !matches {
				return errRestoreStageChanged
			}
			if err := unix.Unlinkat(fd, child, unix.AT_REMOVEDIR); err != nil {
				return err
			}
			continue
		}
		childFD, err := openCleanupEntryAt(fd, child, uint32(stat.Mode))
		if err != nil {
			return err
		}
		childFile := os.NewFile(uintptr(childFD), "restore cleanup entry")
		if childFile == nil {
			_ = unix.Close(childFD)
			return errors.New("could not retain restore cleanup entry")
		}
		if err := clearFileFlags(childFD); err != nil {
			_ = childFile.Close()
			return err
		}
		matches, err := pinnedEntryMatches(fd, child, childFile)
		_ = childFile.Close()
		if err != nil {
			return err
		}
		if !matches {
			return errRestoreStageChanged
		}
		if err := unix.Unlinkat(fd, child, 0); err != nil {
			return err
		}
	}
	return nil
}

func closeRequestPaths(req *bridgeRequest) error {
	var cleanupErr error
	if req.metadataRoot != nil {
		cleanupErr = cleanupMetadataWorkspace(req)
	}
	if req.restoreStage != nil && !req.restorePublished {
		cleanupErr = clearDirectoryContents(int(req.restoreStage.Fd()))
	}
	if cleanupErr == nil && req.restoreStageRoot != nil && req.restoreStageName != "" {
		rootFD := int(req.restoreStageRoot.Fd())
		matches, err := pinnedEntryMatches(rootFD, req.restoreStageName, req.restoreStage)
		if err != nil {
			cleanupErr = err
		} else if matches {
			cleanupErr = unix.Unlinkat(rootFD, req.restoreStageName, unix.AT_REMOVEDIR)
		} else {
			var stat unix.Stat_t
			if err := unix.Fstatat(rootFD, req.restoreStageName, &stat, unix.AT_SYMLINK_NOFOLLOW); err == nil {
				cleanupErr = errRestoreStageChanged
			} else if !errors.Is(err, unix.ENOENT) {
				cleanupErr = err
			}
		}
	}
	if cleanupErr == nil && req.restoreStageRoot != nil {
		cleanupErr = clearDirectoryContents(int(req.restoreStageRoot.Fd()))
	}
	if cleanupErr == nil && req.restoreParent != nil && req.restoreStageRoot != nil && req.restoreStageRootName != "" {
		parentFD := int(req.restoreParent.Fd())
		matches, err := pinnedEntryMatches(parentFD, req.restoreStageRootName, req.restoreStageRoot)
		if err != nil {
			cleanupErr = err
		} else if !matches {
			cleanupErr = errRestoreStageChanged
		} else if err := unix.Unlinkat(parentFD, req.restoreStageRootName, unix.AT_REMOVEDIR); err != nil {
			cleanupErr = err
		} else {
			cleanupErr = syncDirectory(parentFD)
		}
	}
	if req.metadataRoot != nil {
		cleanupErr = errors.Join(cleanupErr, req.metadataRoot.Close())
		req.metadataRoot = nil
	}
	if req.metadataFile != nil {
		// Closing this retained flock descriptor releases custody only after the
		// worker has stopped using the workspace and cleanup has been attempted.
		cleanupErr = errors.Join(cleanupErr, req.metadataFile.Close())
		req.metadataFile = nil
	}
	for _, file := range req.pathFiles {
		_ = file.Close()
	}
	req.pathFiles = nil
	if req.repoRoot != nil {
		_ = req.repoRoot.Close()
		req.repoRoot = nil
	}
	if req.sourceRoot != nil {
		_ = req.sourceRoot.Close()
		req.sourceRoot = nil
	}
	if req.restoreRoot != nil {
		_ = req.restoreRoot.Close()
		req.restoreRoot = nil
	}
	req.pathsPinned = false
	req.sourceName = ""
	return cleanupErr
}

func recheckRestoreParent(req *bridgeRequest) error {
	if req.restoreParent == nil || req.restoreParentPath == "" {
		return errors.New("restore parent not pinned")
	}
	root, err := os.OpenRoot(req.restoreParentPath)
	if err != nil {
		return errPinnedIdentity
	}
	defer root.Close()
	rootInfo, err := root.Stat(".")
	if err != nil {
		return errPinnedIdentity
	}
	parentInfo, err := req.restoreParent.Stat()
	if err != nil {
		return errPinnedIdentity
	}
	if !os.SameFile(rootInfo, parentInfo) {
		return errPinnedIdentity
	}
	return nil
}

func promoteRestoreStage(ctx context.Context, req *bridgeRequest) error {
	if req.restoreStageName == "" {
		return nil
	}
	if req.restoreParent == nil || req.restoreStageRoot == nil || req.restoreStage == nil || req.restoreName == "" {
		return errors.New("restore publication paths missing")
	}
	parentFD := int(req.restoreParent.Fd())
	if err := syncDirectory(int(req.restoreStage.Fd())); err != nil {
		return err
	}
	if err := syncDirectory(int(req.restoreStageRoot.Fd())); err != nil {
		return err
	}
	if err := syncDirectory(parentFD); err != nil {
		return err
	}
	var destinationStat unix.Stat_t
	if statErr := unix.Fstatat(parentFD, req.restoreName, &destinationStat, unix.AT_SYMLINK_NOFOLLOW); statErr == nil {
		return errRestoreExists
	} else if !errors.Is(statErr, unix.ENOENT) {
		return statErr
	}
	matches, err := pinnedEntryMatches(int(req.restoreStageRoot.Fd()), req.restoreStageName, req.restoreStage)
	if err != nil {
		return err
	}
	if !matches {
		return errRestoreStageChanged
	}
	if testGate != nil {
		if err := testGate("before-publish", ctx); err != nil {
			return err
		}
	}
	if err := recheckRestoreParent(req); err != nil {
		return err
	}
	if err := publishDirectory(int(req.restoreStageRoot.Fd()), req.restoreStageName, parentFD, req.restoreName); err != nil {
		return err
	}
	req.restorePublished = true
	matches, err = pinnedEntryMatches(parentFD, req.restoreName, req.restoreStage)
	if err != nil || !matches {
		if err == nil {
			err = errRestoreStageChanged
		}
		return errors.Join(errRestoreCommittedUncertain, err)
	}
	if err := syncDirectory(parentFD); err != nil {
		return errors.Join(errRestoreCommittedUncertain, err)
	}
	return nil
}
