//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"errors"
	"io"
	"os"
	"path/filepath"
	"testing"

	"golang.org/x/sys/unix"
)

func searchFixture(t *testing.T) string {
	t.Helper()
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	return base
}
func searchPin(t *testing.T, path string) *os.File {
	t.Helper()
	f, err := openSearchPinnedDirectory(path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = f.Close() })
	return f
}
func TestSearchDirectoryPinsReadableEndpointThroughSearchOnlyParent(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Fatal("search-only permission proof requires non-root")
	}
	base := searchFixture(t)
	parent := filepath.Join(base, "parent")
	endpoint := filepath.Join(parent, "selected")
	if err := os.MkdirAll(endpoint, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(endpoint, "owner"), []byte("retained"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(parent, 0111); err != nil {
		t.Fatal(err)
	}
	defer os.Chmod(parent, 0700)
	if f, err := os.Open(parent); err == nil {
		f.Close()
		t.Fatal("read-denied positive control opened")
	}
	held := searchPin(t, endpoint)
	entries, err := held.Readdirnames(-1)
	if err != nil || len(entries) != 1 || entries[0] != "owner" {
		t.Fatalf("endpoint not readable: %v %v", entries, err)
	}
	moved := filepath.Join(parent, "moved")
	if err := os.Chmod(parent, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(endpoint, moved); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(endpoint, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(endpoint, "owner"), []byte("replacement"), 0600); err != nil {
		t.Fatal(err)
	}
	fd, err := unix.Openat(int(held.Fd()), "owner", unix.O_RDONLY|unix.O_NOFOLLOW, 0)
	if err != nil {
		t.Fatal(err)
	}
	f := os.NewFile(uintptr(fd), "retained owner")
	defer f.Close()
	data, err := io.ReadAll(f)
	if err != nil || string(data) != "retained" {
		t.Fatalf("redirected endpoint: %q %v", data, err)
	}
}
func TestSearchDirectoryRefusesLeafAndAncestorSymlinks(t *testing.T) {
	base := searchFixture(t)
	real := filepath.Join(base, "real")
	if err := os.MkdirAll(filepath.Join(real, "child"), 0700); err != nil {
		t.Fatal(err)
	}
	alias := filepath.Join(base, "alias")
	if err := os.Symlink("real", alias); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{alias, filepath.Join(alias, "child")} {
		f, err := openSearchPinnedDirectory(path)
		if f != nil {
			f.Close()
		}
		if err == nil {
			t.Fatalf("symlink accepted: %s", path)
		}
	}
}
func TestSearchDirectoryContainmentAndFailures(t *testing.T) {
	base := searchFixture(t)
	a := filepath.Join(base, "a")
	d := filepath.Join(a, "p", "d")
	sibling := filepath.Join(base, "sibling")
	if err := os.MkdirAll(d, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(sibling, 0700); err != nil {
		t.Fatal(err)
	}
	af, df, sf := searchPin(t, a), searchPin(t, d), searchPin(t, sibling)
	for _, test := range []struct {
		a, b *os.File
		want bool
	}{{af, af, true}, {af, df, true}, {df, af, false}, {sf, df, false}, {df, sf, false}} {
		got, err := directoryContainsSearch(test.a, test.b)
		if err != nil || got != test.want {
			t.Fatalf("containment=%v,%v want %v", got, err, test.want)
		}
	}
	if got, err := directoryContainsSearchBounded(sf, df, 1, nil); err == nil || got {
		t.Fatal("bound returned disjoint success")
	}
	closed, err := openSearchPinnedDirectory(a)
	if err != nil {
		t.Fatal(err)
	}
	closed.Close()
	if got, err := directoryContainsSearch(closed, df); err == nil || got {
		t.Fatal("invalid ancestor accepted")
	}
	if got, err := directoryContainsSearch(af, closed); err == nil || got {
		t.Fatal("invalid descendant accepted")
	}
	injected := errors.New("injected parent observation failure")
	if got, err := directoryContainsSearchBounded(sf, df, 128, func(int) error { return injected }); !errors.Is(err, injected) || got {
		t.Fatal("parent failure accepted")
	}
}
func TestSearchDirectoryRefusesConcurrentAncestryChanges(t *testing.T) {
	for _, kind := range []string{"deeper", "shallower", "detach-old-parent"} {
		t.Run(kind, func(t *testing.T) {
			base := searchFixture(t)
			a := filepath.Join(base, "a")
			p := filepath.Join(a, "p")
			d := filepath.Join(p, "d")
			if err := os.MkdirAll(filepath.Join(p, "q"), 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(d, 0700); err != nil {
				t.Fatal(err)
			}
			af, df := searchPin(t, a), searchPin(t, d)
			got, err := directoryContainsSearchBounded(af, df, 128, func(depth int) error {
				if depth != 1 {
					return nil
				}
				destination := filepath.Join(a, "d")
				if kind == "deeper" {
					destination = filepath.Join(p, "q", "d")
				}
				if err := os.Rename(d, destination); err != nil {
					return err
				}
				if kind == "detach-old-parent" {
					return os.Rename(p, filepath.Join(base, "detached"))
				}
				return nil
			})
			if err == nil && !got {
				t.Fatal("continuously contained descendant classified disjoint")
			}
			if kind == "detach-old-parent" && err == nil {
				t.Fatal("detached parent chain was not refused")
			}
		})
	}
}

func TestSearchDirectoryOverlapRetainsBothChains(t *testing.T) {
	base := searchFixture(t)
	a, b := filepath.Join(base, "a"), filepath.Join(base, "b")
	if err := os.Mkdir(a, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(b, 0700); err != nil {
		t.Fatal(err)
	}
	af, bf := searchPin(t, a), searchPin(t, b)
	if got, err := searchDirectoriesOverlap(af, bf); err != nil || got {
		t.Fatalf("siblings: %v %v", got, err)
	}
	got, err := searchDirectoriesOverlapBounded(af, bf, 128, func() error { return os.Rename(a, filepath.Join(b, "a")) })
	if err == nil && !got {
		t.Fatal("between-chain overlap classified disjoint")
	}
	if _, err := af.Stat(); err != nil {
		t.Fatal("caller endpoint closed", err)
	}
	if _, err := bf.Stat(); err != nil {
		t.Fatal("caller endpoint closed", err)
	}
}

func TestSearchDirectoryChainDescriptorsCloseOnEveryExit(t *testing.T) {
	base := searchFixture(t)
	deep := filepath.Join(base, "a", "b", "c")
	if err := os.MkdirAll(deep, 0700); err != nil {
		t.Fatal(err)
	}
	endpoint := searchPin(t, deep)
	for _, kind := range []string{"success", "cap", "observation-error"} {
		for repeat := 0; repeat < 4; repeat++ {
			var observed []int
			limit := 128
			if kind == "cap" {
				limit = 2
			}
			chain, err := buildSearchDirectoryChain(endpoint, limit, func(depth, fd int) error {
				observed = append(observed, fd)
				if kind == "observation-error" && depth == 1 {
					return errors.New("injected observation error")
				}
				return nil
			})
			if kind == "success" {
				if err != nil {
					t.Fatal(err)
				}
				closeSearchDirectoryChain(chain)
			} else if err == nil {
				closeSearchDirectoryChain(chain)
				t.Fatal("expected failure")
			}
			if len(observed) == 0 {
				t.Fatal("descriptor oracle never observed a handle")
			}
			for _, fd := range observed {
				if _, err := unix.FcntlInt(uintptr(fd), unix.F_GETFD, 0); !errors.Is(err, unix.EBADF) {
					t.Fatalf("%s fd %d not closed: %v", kind, fd, err)
				}
			}
			if _, err := endpoint.Stat(); err != nil {
				t.Fatal("caller endpoint closed", err)
			}
		}
	}
}
