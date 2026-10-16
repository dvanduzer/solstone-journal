// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"golang.org/x/sys/unix"
	"os"
	"syscall"
)

// The caller keeps fd open and unchanged throughout synchronous entry. This
// observes identity only: no adoption, duplicate, finalizer or async fd storage.
type borrowedSourceIdentity struct {
	device uint64
	inode  uint64
}

func readBorrowedSourceIdentity(fd int) (*borrowedSourceIdentity, int32) {
	var stat unix.Stat_t
	if fd < 0 || unix.Fstat(fd, &stat) != nil || stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Uid != uint32(os.Geteuid()) {
		return nil, StatusInvalidArgument
	}
	return &borrowedSourceIdentity{device: uint64(stat.Dev), inode: uint64(stat.Ino)}, StatusOk
}

// Private test observation at the actual captured-root boundary; nil in the
// production archive and no exported setter or callback installation path.
var testSourceIdentity func(string, *bridgeRequest)

func verifyCapturedSource(req *bridgeRequest, expected *borrowedSourceIdentity) int32 {
	if req == nil || req.operation != OpSnapshot || req.sourceRoot == nil || expected == nil {
		return StatusInvalidArgument
	}
	if testSourceIdentity != nil {
		testSourceIdentity("captured", req)
	}
	info, err := req.sourceRoot.Stat(".")
	if err != nil || !info.IsDir() {
		return StatusInvalidArgument
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok || uint64(stat.Dev) != expected.device || uint64(stat.Ino) != expected.inode {
		return StatusInvalidArgument
	}
	return StatusOk
}
