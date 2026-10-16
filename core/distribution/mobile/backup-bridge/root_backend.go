// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//go:build darwin || ios || linux

package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"hash"
	"io"
	"os"
	"path"
	"strings"
	"syscall"

	"github.com/restic/restic/internal/backend"
	"github.com/restic/restic/internal/backend/layout"
	"github.com/restic/restic/internal/backend/util"
	"golang.org/x/sys/unix"
)

var errTooShort = fmt.Errorf("file is too short")

type rootBackend struct {
	root   *os.Root
	layout layout.Layout
	modes  util.Modes
}

var _ backend.Backend = &rootBackend{}

func cleanRel(p string) string {
	p = strings.TrimPrefix(p, "/")
	p = path.Clean(p)
	p = strings.TrimPrefix(p, "/")
	if p == "" || p == "." {
		return "."
	}
	return p
}

func newRootBackend(root *os.Root) (*rootBackend, error) {
	if root == nil {
		return nil, errors.New("nil root")
	}
	l := layout.NewDefaultLayout("", path.Join)
	fi, err := root.Stat(cleanRel(l.Filename(backend.Handle{Type: backend.ConfigFile})))
	m := util.DeriveModesFromFileInfo(fi, err)
	return &rootBackend{
		root:   root,
		layout: l,
		modes:  m,
	}, nil
}

func createRootBackend(root *os.Root) (*rootBackend, error) {
	be, err := newRootBackend(root)
	if err != nil {
		return nil, err
	}
	configFile := cleanRel(be.layout.Filename(backend.Handle{Type: backend.ConfigFile}))
	if _, err := root.Lstat(configFile); err == nil {
		return nil, errors.New("config file already exists")
	}
	for _, d := range be.layout.Paths() {
		rel := cleanRel(d)
		if err := root.MkdirAll(rel, be.modes.Dir); err != nil {
			return nil, err
		}
	}
	return be, nil
}

func (b *rootBackend) Properties() backend.Properties {
	return backend.Properties{
		Connections:      1,
		HasAtomicReplace: true,
	}
}

func (b *rootBackend) Hasher() hash.Hash {
	return nil
}

func (b *rootBackend) IsNotExist(err error) bool {
	return errors.Is(err, os.ErrNotExist)
}

func (b *rootBackend) IsPermanentError(err error) bool {
	return b.IsNotExist(err) || errors.Is(err, errTooShort) || errors.Is(err, os.ErrPermission)
}

func (b *rootBackend) Close() error {
	return nil
}

func (b *rootBackend) Warmup(_ context.Context, _ []backend.Handle) ([]backend.Handle, error) {
	return []backend.Handle{}, nil
}

func (b *rootBackend) WarmupWait(_ context.Context, _ []backend.Handle) error {
	return nil
}

func isSyncIgnored(err error) bool {
	if err == nil {
		return true
	}
	if errors.Is(err, unix.ENOTSUP) || errors.Is(err, syscall.ENOTSUP) || errors.Is(err, unix.ENOTTY) || errors.Is(err, syscall.ENOTTY) {
		return true
	}
	return false
}

func (b *rootBackend) Save(_ context.Context, h backend.Handle, rd backend.RewindReader) (err error) {
	finalname := cleanRel(b.layout.Filename(h))
	dir := cleanRel(path.Dir(finalname))

	var randBuf [16]byte
	if _, err := rand.Read(randBuf[:]); err != nil {
		return err
	}
	tmpName := path.Join(dir, path.Base(finalname)+"-tmp-"+hex.EncodeToString(randBuf[:]))
	tmpName = cleanRel(tmpName)

	f, err := b.root.OpenFile(tmpName, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if b.IsNotExist(err) {
		if mkErr := b.root.MkdirAll(dir, b.modes.Dir); mkErr == nil {
			f, err = b.root.OpenFile(tmpName, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
		}
	}
	if err != nil {
		return err
	}

	defer func() {
		if err != nil {
			_ = f.Close()
			_ = b.root.Remove(tmpName)
		}
	}()

	wbytes, err := io.Copy(f, rd)
	if err != nil {
		return err
	}
	if wbytes != rd.Length() {
		return fmt.Errorf("wrote %d bytes instead of the expected %d bytes", wbytes, rd.Length())
	}

	syncErr := f.Sync()
	if syncErr != nil && !isSyncIgnored(syncErr) {
		return syncErr
	}

	if chmodErr := unix.Fchmod(int(f.Fd()), uint32(b.modes.File&^0222)); chmodErr != nil {
		if !errors.Is(chmodErr, unix.EPERM) && !errors.Is(chmodErr, unix.ENOTSUP) && !errors.Is(chmodErr, unix.EOPNOTSUPP) {
			return chmodErr
		}
	}

	if err = f.Close(); err != nil {
		return err
	}

	if err = b.root.Rename(tmpName, finalname); err != nil {
		return err
	}

	dirFile, dirErr := b.root.OpenFile(dir, os.O_RDONLY, 0)
	if dirErr != nil {
		return dirErr
	}
	syncDirErr := unix.Fsync(int(dirFile.Fd()))
	closeDirErr := dirFile.Close()
	if syncDirErr != nil && !isSyncIgnored(syncDirErr) {
		return syncDirErr
	}
	if closeDirErr != nil {
		return closeDirErr
	}

	return nil
}

func (b *rootBackend) Load(ctx context.Context, h backend.Handle, length int, offset int64, fn func(rd io.Reader) error) error {
	return util.DefaultLoad(ctx, h, length, offset, b.openReader, fn)
}

func (b *rootBackend) openReader(_ context.Context, h backend.Handle, length int, offset int64) (io.ReadCloser, error) {
	name := cleanRel(b.layout.Filename(h))
	f, err := b.root.OpenFile(name, os.O_RDONLY, 0)
	if err != nil {
		return nil, err
	}

	fi, err := f.Stat()
	if err != nil {
		_ = f.Close()
		return nil, err
	}

	size := fi.Size()
	if size < offset+int64(length) {
		_ = f.Close()
		return nil, errTooShort
	}

	if offset > 0 {
		_, err = f.Seek(offset, io.SeekStart)
		if err != nil {
			_ = f.Close()
			return nil, err
		}
	}

	if length > 0 {
		return util.LimitReadCloser(f, int64(length)), nil
	}

	return f, nil
}

func (b *rootBackend) Stat(_ context.Context, h backend.Handle) (backend.FileInfo, error) {
	name := cleanRel(b.layout.Filename(h))
	fi, err := b.root.Stat(name)
	if err != nil {
		return backend.FileInfo{}, err
	}
	return backend.FileInfo{Size: fi.Size(), Name: h.Name}, nil
}

func (b *rootBackend) Remove(_ context.Context, h backend.Handle) error {
	name := cleanRel(b.layout.Filename(h))
	return b.root.Remove(name)
}

// Private test observation of actual lock enumeration; no external setter.
var testRootLockList func(context.Context)

func (b *rootBackend) List(ctx context.Context, t backend.FileType, fn func(backend.FileInfo) error) error {
	if t == backend.LockFile && testRootLockList != nil {
		testRootLockList(ctx)
	}
	basedir, subdirs := b.layout.Basedir(t)
	basedir = cleanRel(basedir)
	var err error
	if subdirs {
		err = b.visitDirs(ctx, basedir, fn)
	} else {
		err = b.visitFiles(ctx, basedir, fn, false)
	}
	if b.IsNotExist(err) {
		return nil
	}
	return err
}

func (b *rootBackend) visitDirs(ctx context.Context, dir string, fn func(backend.FileInfo) error) error {
	d, err := b.root.OpenFile(dir, os.O_RDONLY, 0)
	if err != nil {
		return err
	}
	sub, err := d.Readdirnames(-1)
	_ = d.Close()
	if err != nil {
		return err
	}

	for _, f := range sub {
		if err := b.visitFiles(ctx, path.Join(dir, f), fn, true); err != nil {
			return err
		}
	}
	return ctx.Err()
}

func (b *rootBackend) visitFiles(ctx context.Context, dir string, fn func(backend.FileInfo) error, ignoreNotADirectory bool) error {
	d, err := b.root.OpenFile(dir, os.O_RDONLY, 0)
	if err != nil {
		return err
	}
	if ignoreNotADirectory {
		fi, err := d.Stat()
		if err != nil || !fi.IsDir() {
			_ = d.Close()
			return err
		}
	}
	sub, err := d.Readdir(-1)
	_ = d.Close()
	if err != nil {
		return err
	}

	for _, fi := range sub {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		err := fn(backend.FileInfo{
			Name: fi.Name(),
			Size: fi.Size(),
		})
		if err != nil {
			return err
		}
	}
	return nil
}

func (b *rootBackend) Delete(_ context.Context) error {
	d, err := b.root.OpenFile(".", os.O_RDONLY, 0)
	if err != nil {
		return err
	}
	sub, err := d.Readdirnames(-1)
	_ = d.Close()
	if err != nil {
		return err
	}

	for _, name := range sub {
		if err := b.root.RemoveAll(name); err != nil {
			return err
		}
	}
	return nil
}
