// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//go:build darwin || ios || linux

package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/restic/restic/internal/data"
	"github.com/restic/restic/internal/fs"
	"golang.org/x/sys/unix"
)

var (
	errPathEscapes   = errors.New("path escapes from root")
	readXattrFault   error
	blockContentRead func()
)

type rootFS struct {
	root         *os.Root
	metadataRoot *os.Root
	sourcePath   string
}

var _ fs.FS = &rootFS{}

func newRootFS(root *os.Root, sourcePath string, metadataRoot *os.Root) *rootFS {
	return &rootFS{
		root:         root,
		metadataRoot: metadataRoot,
		sourcePath:   filepath.Clean(sourcePath),
	}
}

func (fsys *rootFS) resolve(name string) (string, error) {
	clean := filepath.Clean(name)
	if clean == fsys.sourcePath {
		return ".", nil
	}
	prefix := fsys.sourcePath + "/"
	if strings.HasPrefix(clean, prefix) {
		rel := strings.TrimPrefix(clean, prefix)
		rel = filepath.Clean(rel)
		if rel == ".." || strings.HasPrefix(rel, "../") || strings.HasPrefix(rel, "/") {
			return "", errPathEscapes
		}
		return rel, nil
	}
	return "", errPathEscapes
}

func (fsys *rootFS) Join(elem ...string) string {
	return filepath.Join(elem...)
}

func (fsys *rootFS) Separator() string {
	return string(filepath.Separator)
}

func (fsys *rootFS) Abs(p string) (string, error) {
	return filepath.Clean(p), nil
}

func (fsys *rootFS) Clean(p string) string {
	return filepath.Clean(p)
}

func (fsys *rootFS) VolumeName(p string) string {
	return ""
}

func (fsys *rootFS) IsAbs(p string) bool {
	return filepath.IsAbs(p)
}

func (fsys *rootFS) Dir(p string) string {
	return filepath.Dir(p)
}

func (fsys *rootFS) Base(p string) string {
	return filepath.Base(p)
}

func (fsys *rootFS) Lstat(name string) (*fs.ExtendedFileInfo, error) {
	rel, err := fsys.resolve(name)
	if err != nil {
		return nil, err
	}
	fi, err := fsys.root.Lstat(rel)
	if err != nil {
		return nil, err
	}
	ext := fs.ExtendedStat(fi)
	ext.Name = filepath.Base(name)
	return ext, nil
}

func (fsys *rootFS) OpenFile(name string, flag int, metadataOnly bool) (fs.File, error) {
	rel, err := fsys.resolve(name)
	if err != nil {
		return nil, err
	}

	rf := &rootFile{
		fsys:         fsys,
		virtualName:  filepath.Base(name),
		relPath:      rel,
		metadataOnly: metadataOnly,
	}

	if metadataOnly {
		file, err := openMetadataFile(fsys.root, rel)
		if err != nil {
			return nil, err
		}
		rf.file = file
		return rf, nil
	}

	if err := rf.openContent(); err != nil {
		return nil, err
	}
	return rf, nil
}

type rootFile struct {
	fsys         *rootFS
	virtualName  string
	relPath      string
	metadataOnly bool
	file         *os.File
	info         os.FileInfo
}

var _ fs.File = &rootFile{}

func (rf *rootFile) openContent() error {
	metadata, err := openMetadataFile(rf.fsys.root, rf.relPath)
	if err != nil {
		return err
	}
	defer metadata.Close()
	rf.file, err = readableMetadataFile(metadata)
	return err
}

func (rf *rootFile) MakeReadable() error {
	if !rf.metadataOnly {
		return errors.New("file is already opened for reading")
	}
	oldFile := rf.file
	readable, err := readableMetadataFile(oldFile)
	if err != nil {
		return err
	}
	oldInfo, oldErr := oldFile.Stat()
	newInfo, newErr := readable.Stat()
	if oldErr != nil || newErr != nil || !os.SameFile(oldInfo, newInfo) {
		_ = readable.Close()
		return errors.Join(oldErr, newErr, errors.New("source file identity changed before readable open"))
	}
	rf.file = readable
	_ = oldFile.Close()
	rf.metadataOnly = false
	rf.info = nil
	return nil
}

func (rf *rootFile) Read(p []byte) (n int, err error) {
	if blockContentRead != nil {
		blockContentRead()
	}
	if rf.file == nil || rf.metadataOnly {
		return 0, errors.New("file not open for reading")
	}
	return rf.file.Read(p)
}

func (rf *rootFile) Close() error {
	if rf.file != nil {
		err := rf.file.Close()
		rf.file = nil
		return err
	}
	return nil
}

func (rf *rootFile) Readdirnames(n int) ([]string, error) {
	if rf.file != nil && !rf.metadataOnly {
		return rf.file.Readdirnames(n)
	}
	if rf.file == nil {
		return nil, errors.New("source file is closed")
	}
	fd, err := unix.Openat(int(rf.file.Fd()), ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	f := os.NewFile(uintptr(fd), rf.relPath)
	defer f.Close()
	return f.Readdirnames(n)
}

func (rf *rootFile) fileInfo() (os.FileInfo, error) {
	if rf.info != nil {
		return rf.info, nil
	}
	if rf.file == nil {
		return nil, errors.New("source file is closed")
	}
	info, err := rf.file.Stat()
	if err != nil {
		return nil, err
	}
	rf.info = info
	return info, nil
}

func (rf *rootFile) Stat() (*fs.ExtendedFileInfo, error) {
	fi, err := rf.fileInfo()
	if err != nil {
		return nil, err
	}
	ext := fs.ExtendedStat(fi)
	ext.Name = rf.virtualName
	return ext, nil
}

func (rf *rootFile) ToNode(ignoreXattrListError bool, warnf func(format string, args ...any)) (*data.Node, error) {
	if readXattrFault != nil {
		return nil, readXattrFault
	}

	fi, err := rf.fileInfo()
	if err != nil {
		return nil, err
	}

	ext := fs.ExtendedStat(fi)
	ext.Name = rf.virtualName

	node := &data.Node{
		Path:       rf.virtualName,
		Name:       rf.virtualName,
		Mode:       fi.Mode() & (os.ModePerm | os.ModeType | os.ModeSetuid | os.ModeSetgid | os.ModeSticky),
		ModTime:    fi.ModTime(),
		Inode:      ext.Inode,
		DeviceID:   ext.DeviceID,
		ChangeTime: ext.ChangeTime,
		AccessTime: ext.AccessTime,
		UID:        ext.UID,
		GID:        ext.GID,
		User:       fs.LookupUsername(ext.UID),
		Group:      fs.LookupGroup(ext.GID),
		Links:      ext.Links,
	}

	switch fi.Mode() & os.ModeType {
	case 0:
		node.Type = data.NodeTypeFile
		node.Size = uint64(fi.Size())
	case os.ModeDir:
		node.Type = data.NodeTypeDir
	case os.ModeSymlink:
		node.Type = data.NodeTypeSymlink
		target, err := readMetadataLink(rf.fsys.metadataRoot, rf.file)
		if err != nil {
			return nil, err
		}
		node.LinkTarget = target
	case os.ModeDevice | os.ModeCharDevice:
		node.Type = data.NodeTypeCharDev
		node.Device = ext.Device
	case os.ModeDevice:
		node.Type = data.NodeTypeDev
		node.Device = ext.Device
	case os.ModeNamedPipe:
		node.Type = data.NodeTypeFifo
	case os.ModeSocket:
		node.Type = data.NodeTypeSocket
	default:
		return nil, fmt.Errorf("unsupported file type %v", fi.Mode().Type())
	}

	xattrs, err := readNodeFileXattrs(rf.fsys.root, rf.relPath, rf.file)
	if err != nil {
		return nil, err
	}
	node.ExtendedAttributes = xattrs

	return node, nil
}

func openatParent(rootFD int, p string) (int, string, func(), error) {
	p = strings.TrimPrefix(filepath.Clean(p), "/")
	if p == "." || p == "" {
		return rootFD, ".", func() {}, nil
	}
	dir := filepath.Dir(p)
	leaf := filepath.Base(p)
	if dir == "." || dir == "" {
		return rootFD, leaf, func() {}, nil
	}

	comps := strings.Split(dir, "/")
	curFD := rootFD
	for _, comp := range comps {
		if comp == "" || comp == "." {
			continue
		}
		if comp == ".." {
			if curFD != rootFD {
				_ = unix.Close(curFD)
			}
			return -1, "", nil, errPathEscapes
		}
		nextFD, err := unix.Openat(curFD, comp, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if curFD != rootFD {
			_ = unix.Close(curFD)
		}
		if err != nil {
			return -1, "", nil, err
		}
		curFD = nextFD
	}

	return curFD, leaf, func() {
		if curFD != rootFD {
			_ = unix.Close(curFD)
		}
	}, nil
}
