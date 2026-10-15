// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"unicode/utf8"

	"github.com/restic/restic/internal/filter"
)

// The v1 policy is a JSON array of journal-relative restic exclusion patterns.
// There is no environment expansion, file loading, negation or source authority
// in this input. The caller resolves its retained exclude file before dispatch.
const (
	maxSnapshotSelectionBytes = 8 * 1024 * 1024
	maxSnapshotExclusions     = 65536
	maxSnapshotPatternDepth   = 64
)

type snapshotSelection struct {
	patterns []filter.Pattern
	mu       sync.Mutex
	err      error
}

func parseSnapshotSelection(input []byte) (*snapshotSelection, int32) {
	if len(input) > maxSnapshotSelectionBytes {
		return nil, StatusCapExceeded
	}
	if len(input) == 0 || !utf8.Valid(input) || bytes.IndexByte(input, 0) >= 0 {
		return nil, StatusInvalidArgument
	}
	if !snapshotSelectionUnicode(input) {
		return nil, StatusInvalidArgument
	}
	decoder := json.NewDecoder(bytes.NewReader(input))
	opening, err := decoder.Token()
	if err != nil || opening != json.Delim('[') {
		return nil, StatusInvalidArgument
	}
	var rooted []string
	for decoder.More() {
		if len(rooted) == maxSnapshotExclusions {
			return nil, StatusCapExceeded
		}
		var pattern string
		if decoder.Decode(&pattern) != nil {
			return nil, StatusInvalidArgument
		}
		if len(pattern) > maxPathLen {
			return nil, StatusCapExceeded
		}
		if pattern == "" || strings.HasPrefix(pattern, "/") || strings.HasPrefix(pattern, "!") ||
			strings.ContainsAny(pattern, "\x00\r\n") {
			return nil, StatusInvalidArgument
		}
		parts := strings.Split(pattern, "/")
		if len(parts) > maxSnapshotPatternDepth {
			return nil, StatusCapExceeded
		}
		recursive := 0
		for _, part := range parts {
			if part == "" || part == "." || part == ".." {
				return nil, StatusInvalidArgument
			}
			if part == "**" {
				recursive++
				if recursive > 1 {
					return nil, StatusInvalidArgument
				}
			}
		}
		rooted = append(rooted, "/"+pattern)
	}
	closing, err := decoder.Token()
	if err != nil || closing != json.Delim(']') {
		return nil, StatusInvalidArgument
	}
	if _, err := decoder.Token(); err != io.EOF {
		return nil, StatusInvalidArgument
	}
	if filter.ValidatePatterns(rooted) != nil {
		return nil, StatusInvalidArgument
	}
	return &snapshotSelection{patterns: filter.ParsePatterns(rooted)}, StatusOk
}

// encoding/json replaces unmatched UTF-16 escapes with U+FFFD. Refuse that lossy
// input while retaining actual U+FFFD filenames and correctly paired escapes.
func snapshotSelectionUnicode(input []byte) bool {
	for index := 0; index < len(input); index++ {
		if input[index] != '\\' {
			continue
		}
		index++
		if index >= len(input) {
			return false
		}
		if input[index] != 'u' {
			continue
		}
		if index+4 >= len(input) {
			return false
		}
		unit, err := strconv.ParseUint(string(input[index+1:index+5]), 16, 16)
		if err != nil || unit >= 0xdc00 && unit <= 0xdfff {
			return false
		}
		index += 4
		if unit < 0xd800 || unit > 0xdbff {
			continue
		}
		if index+6 >= len(input) || input[index+1] != '\\' || input[index+2] != 'u' {
			return false
		}
		low, err := strconv.ParseUint(string(input[index+3:index+7]), 16, 16)
		if err != nil || low < 0xdc00 || low > 0xdfff {
			return false
		}
		index += 6
	}
	return true
}

// Match in the held source's relative coordinates, so a root basename that
// contains metacharacters cannot broaden an exclusion into owner subdirectories.
func (selection *snapshotSelection) permits(ctx context.Context, source *rootFS, item string) bool {
	if err := ctx.Err(); err != nil {
		return selection.refuse(err)
	}
	relative, err := source.resolve(item)
	if err == nil && relative == "." {
		return true
	}
	if err != nil {
		return selection.refuse(err)
	}
	// Patterns are rooted positive exclusions. Matching them individually has
	// the same OR semantics while allowing the real operation cancellation to
	// interrupt a large policy. The parser bounds each pattern's recursion/depth.
	path := "/" + filepath.ToSlash(relative)
	for _, pattern := range selection.patterns {
		if err := ctx.Err(); err != nil {
			return selection.refuse(err)
		}
		rejected, err := filter.List([]filter.Pattern{pattern}, path)
		if err != nil {
			return selection.refuse(err)
		}
		if rejected {
			return false
		}
	}
	return true
}

func (selection *snapshotSelection) refuse(err error) bool {
	selection.mu.Lock()
	if selection.err == nil {
		selection.err = err
	}
	selection.mu.Unlock()
	return false
}

func (selection *snapshotSelection) failure() error {
	selection.mu.Lock()
	defer selection.mu.Unlock()
	return selection.err
}
