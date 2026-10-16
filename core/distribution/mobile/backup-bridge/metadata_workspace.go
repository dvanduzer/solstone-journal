// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build linux || darwin || ios

package main

import (
	"errors"
	"fmt"
	"golang.org/x/sys/unix"
	"io"
	"os"
	"strings"
)

const metadataDirectoryPrefix = ".backup-metadata-"

var errMetadataCleanup = errors.New("metadata workspace requires cleanup")

func isMetadataNonce(name string) bool {
	if len(name) != 32 {
		return false
	}
	for _, ch := range name {
		if !(ch >= '0' && ch <= '9') && !(ch >= 'a' && ch <= 'f') {
			return false
		}
	}
	return true
}

func pinMetadataWorkspace(req *bridgeRequest, path string) (resultErr error) {
	return pinMetadataWorkspaceExpected(req, path, nil)
}

func pinMetadataWorkspaceExpected(req *bridgeRequest, path string, expected *borrowedDirectoryIdentity) (resultErr error) {
	if req.operation != OpSnapshot || req.metadataRoot != nil || req.sourceRoot == nil || req.repoRoot == nil && req.remote == nil {
		return errors.New("invalid metadata workspace operation")
	}
	if err := sanitizePath(path); err != nil {
		return err
	}
	file, err := openOwnedDirectory(path, req.owned)
	if err != nil {
		return err
	}
	retained := false
	defer func() {
		if !retained {
			req.owned.scrubRejection()
			_ = closeOwnedPrivateDirectory(file, req.owned, "metadata-unassigned-directory")
		}
	}()
	var stat unix.Stat_t
	if err := unix.Fstat(int(file.Fd()), &stat); err != nil {
		return err
	}
	if stat.Uid != uint32(os.Geteuid()) || stat.Mode&0777 != 0700 {
		return errors.New("metadata workspace must be privately owned")
	}
	if !matchesBorrowedDirectory("workspace", file, expected) {
		return errors.New("metadata workspace identity changed")
	}
	if err := reserveOwnedPrivateDirectory(file, req.owned); err != nil {
		return err
	}
	// flock is tied to this open directory description, not to a removable lock
	// file or process-associated fcntl lock. Rejected opens cannot unlock it.
	if err := unix.Flock(int(file.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		return err
	}
	protectedRoot := req.repoRoot
	if req.remote != nil {
		protectedRoot = req.remote.owner.root
	}
	for _, root := range []*os.Root{req.sourceRoot, protectedRoot} {
		other, err := root.OpenFile(".", os.O_RDONLY, 0)
		if err != nil {
			return err
		}
		overlap, overlapErr := ownedDirectoriesOverlap(file, other, req.owned)
		if root == req.sourceRoot && metadataRequiresSameVolume() {
			var sourceStat unix.Stat_t
			if err := unix.Fstat(int(other.Fd()), &sourceStat); err != nil {
				overlapErr = errors.Join(overlapErr, err)
			} else if sourceStat.Dev != stat.Dev {
				overlapErr = errors.Join(overlapErr, errors.New("metadata workspace is on a different source volume"))
			}
		}
		if overlapErr != nil || overlap {
			req.owned.scrubRejection()
		}
		if err := req.owned.close(other, "metadata-overlap-directory"); err != nil && req.owned != nil {
			return err
		}
		if overlapErr != nil {
			return overlapErr
		}
		if overlap {
			return errPathOverlap
		}
	}
	root, err := adoptOwnedRoot(path, file, req.owned)
	if err != nil {
		return err
	}
	req.metadataRoot = root
	req.metadataFile = file
	req.metadataPath = path
	retained = true
	return nil
}

// Census the entire reserved namespace before deleting anything. The caller
// explicitly dedicates this private directory to metadata scratch. Foreign or
// malformed entries are preserved; no recursive deletion follows links.
func cleanupMetadataWorkspace(req *bridgeRequest) (resultErr error) {
	return req.owned.cleanup(func() error { return cleanupMetadataWorkspaceImpl(req) })
}

func cleanupMetadataWorkspaceImpl(req *bridgeRequest) (resultErr error) {
	if req.metadataRoot == nil || req.metadataFile == nil {
		return nil
	}
	fd := int(req.metadataFile.Fd())
	listingFD, err := unix.Openat(fd, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return errors.Join(errMetadataCleanup, err)
	}
	listing := os.NewFile(uintptr(listingFD), "metadata workspace census")
	names, readErr := listing.Readdirnames(65)
	if errors.Is(readErr, io.EOF) {
		readErr = nil
	}
	if len(names) > 64 {
		readErr = errors.New("metadata workspace residue exceeds bound")
	}
	closeErr := req.owned.close(listing, "metadata-census")
	if readErr != nil || closeErr != nil {
		return errors.Join(errMetadataCleanup, readErr, closeErr)
	}
	var workspaceStat unix.Stat_t
	if err := unix.Fstat(fd, &workspaceStat); err != nil {
		return errors.Join(errMetadataCleanup, err)
	}
	type residue struct {
		name    string
		file    *os.File
		mode    uint32
		removed bool
	}
	entries := make([]residue, 0, len(names))
	defer func() {
		for _, entry := range entries {
			if req.owned.failed() {
				return
			}
			if !entry.removed {
				if err := unix.Fchmod(int(entry.file.Fd()), entry.mode); err != nil {
					resultErr = errors.Join(resultErr, errMetadataCleanup, err)
				}
			}
			if err := req.owned.close(entry.file, "metadata-residue-final"); err != nil {
				resultErr = errors.Join(resultErr, errMetadataCleanup, err)
			}
		}
	}()
	refuse := func(err error) error { return errors.Join(errMetadataCleanup, err) }
	for _, name := range names {
		if !strings.HasPrefix(name, metadataDirectoryPrefix) || !isMetadataNonce(strings.TrimPrefix(name, metadataDirectoryPrefix)) {
			return refuse(errors.New("unrecognized metadata workspace entry"))
		}
		var stat unix.Stat_t
		if err := unix.Fstatat(fd, name, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return refuse(err)
		}
		if stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Mode&0077 != 0 || stat.Uid != uint32(os.Geteuid()) || stat.Dev != workspaceStat.Dev {
			return refuse(errors.New("unrecognized metadata workspace directory"))
		}
		childFD, err := openCleanupEntryAt(fd, name, uint32(stat.Mode))
		if err != nil {
			return refuse(err)
		}
		child := os.NewFile(uintptr(childFD), "metadata residue")
		var held unix.Stat_t
		if err := unix.Fstat(childFD, &held); err != nil || held.Dev != stat.Dev || held.Ino != stat.Ino {
			if closeErr := req.owned.close(child, "metadata-residue-directory"); closeErr != nil && req.owned != nil {
				return refuse(closeErr)
			}
			return refuse(errors.Join(err, errors.New("metadata residue changed")))
		}
		if err := chmodRetainedMetadataDirectory(childFD, 0700); err != nil {
			if closeErr := req.owned.close(child, "metadata-residue-directory"); closeErr != nil && req.owned != nil {
				return refuse(closeErr)
			}
			return refuse(err)
		}
		readableFD, err := unix.Openat(childFD, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
		if err != nil {
			restoreErr := chmodRetainedMetadataDirectory(childFD, uint32(stat.Mode&0777))
			if closeErr := req.owned.close(child, "metadata-residue-directory"); closeErr != nil && req.owned != nil {
				return refuse(closeErr)
			}
			return refuse(errors.Join(err, restoreErr))
		}
		readable := os.NewFile(uintptr(readableFD), "metadata residue")
		req.owned.hold(readable)
		if closeErr := req.owned.close(child, "metadata-residue-directory"); closeErr != nil && req.owned != nil {
			return refuse(closeErr)
		}
		childFD = readableFD
		child = readable
		entries = append(entries, residue{name: name, file: child, mode: uint32(stat.Mode & 0777)})
		// Recognized parents may be mode 0300 after an interrupted clone. Restore
		// enumeration on this retained, identity-checked private object only.
		if err := unix.Fchmod(childFD, 0700); err != nil {
			return refuse(err)
		}
		readFD, err := unix.Openat(childFD, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
		if err != nil {
			return refuse(err)
		}
		reader := os.NewFile(uintptr(readFD), "metadata residue census")
		children, readErr := reader.Readdirnames(2)
		if errors.Is(readErr, io.EOF) {
			readErr = nil
		}
		closeErr := req.owned.close(reader, "metadata-residue-census")
		if readErr != nil || closeErr != nil {
			return refuse(errors.Join(readErr, closeErr))
		}
		if len(children) > 1 {
			return refuse(errors.New("unexpected metadata residue children"))
		}
		for _, name := range children {
			if !isMetadataNonce(name) {
				return refuse(errors.New("unexpected metadata residue child"))
			}
			var st unix.Stat_t
			if err := unix.Fstatat(childFD, name, &st, unix.AT_SYMLINK_NOFOLLOW); err != nil {
				return refuse(err)
			}
			if st.Mode&unix.S_IFMT != unix.S_IFLNK || st.Uid != uint32(os.Geteuid()) || st.Dev != workspaceStat.Dev {
				return refuse(errors.New("unexpected metadata residue object"))
			}
		}
	}
	for index, entry := range entries {
		matches, err := pinnedEntryMatches(fd, entry.name, entry.file)
		if err != nil || !matches {
			return refuse(errors.Join(err, errors.New("metadata residue changed before cleanup")))
		}
		if err := clearOwnedMetadataDirectory(int(entry.file.Fd()), req.owned); err != nil {
			return refuse(err)
		}
		if err := unix.Unlinkat(fd, entry.name, unix.AT_REMOVEDIR); err != nil {
			return refuse(err)
		}
		entries[index].removed = true
	}
	if len(entries) > 0 {
		if err := syncDirectory(fd); err != nil {
			return refuse(fmt.Errorf("metadata cleanup sync: %w", err))
		}
	}
	return nil
}
