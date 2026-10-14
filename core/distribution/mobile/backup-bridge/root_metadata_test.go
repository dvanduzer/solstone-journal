// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//go:build linux || darwin || ios

package main

import (
	"bytes"
	"errors"
	"golang.org/x/sys/unix"
	"io"
	"os"
	"path/filepath"
	"testing"
)

func metadataWorkspace(t *testing.T) *os.Root {
	t.Helper()
	root, err := os.OpenRoot(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { root.Close() })
	return root
}

func metadataFixture(t *testing.T) (string, *os.Root, *rootFS) {
	t.Helper()
	source := filepath.Join(t.TempDir(), "source")
	if err := os.Mkdir(source, 0700); err != nil {
		t.Fatal(err)
	}
	root, err := os.OpenRoot(source)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { root.Close() })
	return source, root, newRootFS(root, "source", metadataWorkspace(t))
}

func TestReadableSourceMetadataStaysWithOpenedInode(t *testing.T) {
	source, _, fsys := metadataFixture(t)
	path := filepath.Join(source, "file")
	if err := os.WriteFile(path, []byte("retained contents"), 0640); err != nil {
		t.Fatal(err)
	}
	file, err := fsys.OpenFile("source/file", os.O_RDONLY, true)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if err := file.MakeReadable(); err != nil {
		t.Fatal(err)
	}
	info, err := file.Stat()
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(path, path+".old"); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("replacement"), 0600); err != nil {
		t.Fatal(err)
	}
	node, err := file.ToNode(false, nil)
	if err != nil {
		t.Fatal(err)
	}
	contents, err := io.ReadAll(file)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(contents, []byte("retained contents")) || node.Size != uint64(info.Size) || node.Inode != info.Inode || node.Mode.Perm() != 0640 {
		t.Fatalf("mixed retained content and replacement metadata: %q %#v %#v", contents, node, info)
	}
}

func TestMetadataOnlyReplacementCannotSwitchReadableIdentity(t *testing.T) {
	source, _, fsys := metadataFixture(t)
	path := filepath.Join(source, "file")
	if err := os.WriteFile(path, []byte("retained"), 0640); err != nil {
		t.Fatal(err)
	}
	file, err := fsys.OpenFile("source/file", os.O_RDONLY, true)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	before, err := file.Stat()
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(path, path+".old"); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("replacement longer"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := file.MakeReadable(); err != nil {
		t.Fatal(err)
	}
	contents, err := io.ReadAll(file)
	if err != nil || string(contents) != "retained" {
		t.Fatalf("readable object changed: %q %v", contents, err)
	}
	node, err := file.ToNode(false, nil)
	if err != nil {
		t.Fatal(err)
	}
	if node.Size != uint64(before.Size) || node.Inode != before.Inode || node.Mode.Perm() != 0640 {
		t.Fatal("metadata-only identity changed")
	}
}

func TestEscapingSymlinkMetadataIsPreservedWithoutFollowing(t *testing.T) {
	source, _, fsys := metadataFixture(t)
	target := filepath.Join(t.TempDir(), "outside")
	if err := os.WriteFile(target, []byte("outside contents"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(target, filepath.Join(source, "link")); err != nil {
		t.Fatal(err)
	}
	file, err := fsys.OpenFile("source/link", os.O_RDONLY, true)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	node, err := file.ToNode(false, nil)
	if err != nil {
		t.Fatal(err)
	}
	if node.LinkTarget != target {
		t.Fatalf("symlink target not preserved: %#v", node)
	}
	if err := file.MakeReadable(); err == nil {
		t.Fatal("escaping symlink became readable")
	}
}

func TestRequiredXattrValueFailureIsNotOmitted(t *testing.T) {
	for _, failure := range []error{unix.EIO, unix.ENOTSUP, unix.ENODATA} {
		_, err := collectNodeXattrs(func(buf []byte) (int, error) {
			if buf != nil {
				copy(buf, []byte("user.required\x00"))
			}
			return len("user.required\x00"), nil
		}, func(string, []byte) (int, error) { return 0, failure })
		if !errors.Is(err, failure) {
			t.Fatalf("required metadata silently omitted: %v => %v", failure, err)
		}
	}
}

func TestOpenedSourceXattrsStayWithRetainedInode(t *testing.T) {
	source, _, fsys := metadataFixture(t)
	path := filepath.Join(source, "file")
	if err := os.WriteFile(path, []byte("retained"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := unix.Setxattr(path, "user.fixture", []byte("retained attribute"), 0); err != nil {
		t.Fatal(err)
	}
	file, err := fsys.OpenFile("source/file", os.O_RDONLY, true)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if _, err := file.Stat(); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(path, path+".old"); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("replacement"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := unix.Setxattr(path, "user.fixture", []byte("replacement attribute"), 0); err != nil {
		t.Fatal(err)
	}
	node, err := file.ToNode(false, nil)
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, attr := range node.ExtendedAttributes {
		if attr.Name == "user.fixture" {
			found = true
			if !bytes.Equal(attr.Value, []byte("retained attribute")) {
				t.Fatal("xattrs came from replacement inode")
			}
		}
	}
	if !found {
		t.Fatal("retained xattr disappeared")
	}
}

func TestRealEnumeratedXattrDisappearanceFails(t *testing.T) {
	source, _, _ := metadataFixture(t)
	path := filepath.Join(source, "file")
	if err := os.WriteFile(path, []byte("source"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := unix.Setxattr(path, "user.required", []byte("value"), 0); err != nil {
		t.Fatal(err)
	}
	file, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	removed := false
	_, err = collectNodeXattrs(func(buf []byte) (int, error) { return unix.Flistxattr(int(file.Fd()), buf) }, func(name string, buf []byte) (int, error) {
		if name == "user.required" && !removed {
			removed = true
			if err := unix.Removexattr(path, name); err != nil {
				return 0, err
			}
		}
		return unix.Fgetxattr(int(file.Fd()), name, buf)
	})
	if !removed || err == nil {
		t.Fatalf("enumerated xattr disappeared with success: removed=%v error=%v", removed, err)
	}
}

func TestRetainedSymlinkMetadataDoesNotAdoptReplacement(t *testing.T) {
	source, _, fsys := metadataFixture(t)
	path := filepath.Join(source, "link")
	if err := os.Symlink("retained-target", path); err != nil {
		t.Fatal(err)
	}
	file, err := fsys.OpenFile("source/link", os.O_RDONLY, true)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if _, err := file.Stat(); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(path, path+".old"); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("replacement-target", path); err != nil {
		t.Fatal(err)
	}
	node, err := file.ToNode(false, nil)
	if err != nil {
		t.Fatal(err)
	}
	if node.LinkTarget != "retained-target" {
		t.Fatalf("adopted replacement symlink: %q", node.LinkTarget)
	}
	entries, err := fsys.metadataRoot.Open(".")
	if err != nil {
		t.Fatal(err)
	}
	defer entries.Close()
	names, err := entries.Readdirnames(-1)
	if err != nil || len(names) != 0 {
		t.Fatalf("metadata workspace not cleared: %v %v", names, err)
	}
}

func TestSpecialMetadataDoesNotOpenFIFOForIO(t *testing.T) {
	source, _, fsys := metadataFixture(t)
	if err := unix.Mkfifo(filepath.Join(source, "pipe"), 0600); err != nil {
		t.Fatal(err)
	}
	file, err := fsys.OpenFile("source/pipe", os.O_RDONLY, true)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if err := file.MakeReadable(); err == nil {
		t.Fatal("FIFO became a content reader")
	}
	if _, err := fsys.OpenFile("source/pipe", os.O_RDONLY, false); err == nil {
		t.Fatal("FIFO opened for content")
	}
}

func TestXattrAggregateBudgetIncludesAllValues(t *testing.T) {
	calls := 0
	names := []byte("user.first\x00user.second\x00")
	_, err := collectNodeXattrs(func(buf []byte) (int, error) { copy(buf, names); return len(names), nil },
		func(_ string, buf []byte) (int, error) {
			if buf == nil {
				return 40 << 20, nil
			}
			calls++
			return len(buf), nil
		})
	if err == nil || calls != 1 {
		t.Fatalf("aggregate budget was not enforced before second allocation: calls=%d err=%v", calls, err)
	}
}
