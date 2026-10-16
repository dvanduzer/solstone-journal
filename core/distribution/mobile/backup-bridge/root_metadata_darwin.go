// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build darwin || ios

package main

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
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
	var st unix.Stat_t
	if err := unix.Fstatat(parent, leaf, &st, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return nil, err
	}
	return openDarwinMetadataEntry(parent, leaf, rel, st)
}

func openDarwinMetadataEntry(parent int, leaf, name string, expected unix.Stat_t) (*os.File, error) {
	flags := unix.O_EVTONLY | unix.O_NOFOLLOW | unix.O_CLOEXEC | unix.O_NONBLOCK
	if expected.Mode&unix.S_IFMT == unix.S_IFLNK {
		flags = unix.O_EVTONLY | unix.O_SYMLINK | unix.O_CLOEXEC | unix.O_NONBLOCK
	}
	fd, err := unix.Openat(parent, leaf, flags, 0)
	if err != nil {
		return nil, err
	}
	var opened unix.Stat_t
	if err := unix.Fstat(fd, &opened); err != nil || opened.Dev != expected.Dev || opened.Ino != expected.Ino || opened.Mode&unix.S_IFMT != expected.Mode&unix.S_IFMT {
		_ = unix.Close(fd)
		if err != nil {
			return nil, err
		}
		return nil, errors.New("source metadata identity changed before open")
	}
	return os.NewFile(uintptr(fd), name), nil
}

func readableMetadataFile(file *os.File) (*os.File, error) {
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() && !info.IsDir() {
		return nil, errors.New("source object is not readable file or directory")
	}
	// O_EVTONLY descriptors permit reads on Darwin. Dup retains this object;
	// reopening its former source pathname could select a replacement inode.
	fd, err := unix.Dup(int(file.Fd()))
	if err != nil {
		return nil, err
	}
	unix.CloseOnExec(fd)
	return os.NewFile(uintptr(fd), file.Name()), nil
}

func readMetadataLink(workspace *os.Root, file *os.File) (target string, resultErr error) {
	if workspace == nil {
		return "", errors.New("source symlink metadata workspace is unavailable")
	}
	// Darwin cannot readlink an O_SYMLINK descriptor. Clone that held object
	// into an explicit private workspace, then inspect the clone. No source
	// directory entry is resolved, and no source inode is modified.
	var nonce [32]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		return "", err
	}
	name := ".backup-metadata-" + hex.EncodeToString(nonce[:16])
	cloneName := hex.EncodeToString(nonce[16:])
	if err := workspace.Mkdir(name, 0700); err != nil {
		return "", err
	}
	dir, err := workspace.OpenFile(name, os.O_RDONLY, 0)
	if err != nil {
		return "", errors.Join(err, workspace.Remove(name))
	}
	defer func() {
		resultErr = errors.Join(resultErr, clearDirectoryContents(int(dir.Fd())), dir.Close(), workspace.Remove(name))
	}()
	if err := unix.Fchmod(int(dir.Fd()), 0300); err != nil {
		return "", err
	}
	if err := unix.Fclonefileat(int(file.Fd()), int(dir.Fd()), cloneName, unix.CLONE_NOFOLLOW); err != nil {
		return "", err
	}
	for size := 256; size <= 65536; size *= 2 {
		buf := make([]byte, size)
		n, err := unix.Readlinkat(int(dir.Fd()), cloneName, buf)
		if err != nil {
			return "", err
		}
		if n < len(buf) {
			return string(buf[:n]), nil
		}
	}
	return "", errors.New("source symlink target exceeds bound")
}

func readMetadataLinkOwned(workspace *os.Root, file *os.File, entry *ownedCall) (target string, resultErr error) {
	if entry == nil {
		return readMetadataLink(workspace, file)
	}
	if entry.failed() {
		return "", errOwnedCleanup
	}
	if workspace == nil {
		return "", errors.New("source symlink metadata workspace is unavailable")
	}
	// Darwin cannot readlink an O_SYMLINK descriptor. Clone that held object
	// into an explicit private workspace, then inspect the clone. No source
	// directory entry is resolved, and no source inode is modified.
	var nonce [32]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		return "", err
	}
	name := ".backup-metadata-" + hex.EncodeToString(nonce[:16])
	cloneName := hex.EncodeToString(nonce[16:])
	if err := workspace.Mkdir(name, 0700); err != nil {
		return "", err
	}
	dir, err := workspace.OpenFile(name, os.O_RDONLY, 0)
	if err != nil {
		return "", errors.Join(err, workspace.Remove(name))
	}
	entry.hold(dir)
	defer func() {
		cleanupErr := entry.cleanup(func() error {
			err := clearOwnedMetadataDirectory(int(dir.Fd()), entry)
			if entry.failed() {
				return errOwnedCleanup
			}
			closeErr := entry.close(dir, "metadata-clone-directory")
			if entry.failed() {
				return errOwnedCleanup
			}
			return errors.Join(err, closeErr, workspace.Remove(name))
		})
		resultErr = errors.Join(resultErr, cleanupErr)
	}()
	if err := unix.Fchmod(int(dir.Fd()), 0300); err != nil {
		return "", err
	}
	if err := unix.Fclonefileat(int(file.Fd()), int(dir.Fd()), cloneName, unix.CLONE_NOFOLLOW); err != nil {
		return "", err
	}
	for size := 256; size <= 65536; size *= 2 {
		buf := make([]byte, size)
		n, err := unix.Readlinkat(int(dir.Fd()), cloneName, buf)
		if err != nil {
			return "", err
		}
		if n < len(buf) {
			return string(buf[:n]), nil
		}
	}
	return "", errors.New("source symlink target exceeds bound")
}
