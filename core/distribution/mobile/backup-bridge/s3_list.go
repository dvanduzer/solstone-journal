// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"context"
	"encoding/xml"
	"net/http"
	"net/url"
	"strconv"
	"strings"

	"github.com/restic/restic/internal/backend"
)

const (
	s3PageKeys = 256
	s3MaxPages = 1024
	s3MaxToken = 4096
)

// Slices retain missing/duplicate singleton evidence instead of defaulting a
// missing IsTruncated to false. Extra descriptive S3 fields are not authority.
type s3ListReply struct {
	XMLName   xml.Name   `xml:"ListBucketResult"`
	Name      []string   `xml:"Name"`
	Prefix    []string   `xml:"Prefix"`
	KeyCount  []string   `xml:"KeyCount"`
	MaxKeys   []string   `xml:"MaxKeys"`
	Truncated []string   `xml:"IsTruncated"`
	Next      []string   `xml:"NextContinuationToken"`
	Common    []struct{} `xml:"CommonPrefixes"`
	Contents  []struct {
		Keys  []string `xml:"Key"`
		Sizes []string `xml:"Size"`
	} `xml:"Contents"`
}

func s3Directory(kind backend.FileType) (string, bool) {
	switch kind {
	case backend.ConfigFile:
		return "config", true
	case backend.KeyFile:
		return "keys/", true
	case backend.LockFile:
		return "locks/", true
	case backend.IndexFile:
		return "index/", true
	case backend.SnapshotFile:
		return "snapshots/", true
	case backend.PackFile:
		return "data/", true
	default:
		return "", false
	}
}

func (be *s3Backend) listedName(kind backend.FileType, key string) (string, error) {
	directory, ok := s3Directory(kind)
	if !ok || len(key) > 1024 {
		return "", errS3Protocol
	}
	if kind == backend.ConfigFile {
		if key != be.cfg.prefix+"/config" {
			return "", errS3Protocol
		}
		return strings.Repeat("0", 64), nil
	}
	prefix := be.cfg.prefix + "/" + directory
	if !strings.HasPrefix(key, prefix) {
		return "", errS3Protocol
	}
	name := strings.TrimPrefix(key, prefix)
	if kind == backend.PackFile {
		shard, id, found := strings.Cut(name, "/")
		if !found || !validS3ID(id) || shard != id[:2] {
			return "", errS3Protocol
		}
		return id, nil
	}
	if !validS3ID(name) {
		return "", errS3Protocol
	}
	return name, nil
}

func (be *s3Backend) decodeList(body []byte, kind backend.FileType, prefix, previous string) ([]backend.FileInfo, string, string, error) {
	var page s3ListReply
	if decodeS3XML(body, &page) != nil || page.XMLName.Space != "" && page.XMLName.Space != "http://s3.amazonaws.com/doc/2006-03-01/" {
		return nil, "", "", errS3Protocol
	}
	if len(page.Name) != 1 || page.Name[0] != be.cfg.bucket || len(page.Prefix) != 1 || page.Prefix[0] != prefix || len(page.KeyCount) != 1 || len(page.MaxKeys) != 1 || len(page.Truncated) != 1 || len(page.Next) > 1 || len(page.Common) != 0 {
		return nil, "", "", errS3Protocol
	}
	count, countErr := strconv.Atoi(page.KeyCount[0])
	maximum, maxErr := strconv.Atoi(page.MaxKeys[0])
	if countErr != nil || maxErr != nil || maximum <= 0 || maximum > s3PageKeys || count != len(page.Contents) || count > maximum {
		return nil, "", "", errS3Protocol
	}
	next := ""
	switch page.Truncated[0] {
	case "true":
		if len(page.Next) != 1 || page.Next[0] == "" || len(page.Next[0]) > s3MaxToken {
			return nil, "", "", errS3Protocol
		}
		next = page.Next[0]
	case "false":
		if len(page.Next) != 0 {
			return nil, "", "", errS3Protocol
		}
	default:
		return nil, "", "", errS3Protocol
	}
	files := make([]backend.FileInfo, 0, len(page.Contents))
	for _, item := range page.Contents {
		if len(item.Keys) != 1 || len(item.Sizes) != 1 || item.Keys[0] <= previous {
			return nil, "", "", errS3Protocol
		}
		name, err := be.listedName(kind, item.Keys[0])
		if err != nil {
			return nil, "", "", err
		}
		size, err := strconv.ParseInt(item.Sizes[0], 10, 64)
		if err != nil || size < 0 || size > maxHTTPObjectBytes {
			return nil, "", "", errS3Protocol
		}
		files = append(files, backend.FileInfo{Name: name, Size: size})
		previous = item.Keys[0]
	}
	return files, next, previous, nil
}

func (be *s3Backend) List(ctx context.Context, kind backend.FileType, consume func(backend.FileInfo) error) error {
	owned, done, err := be.enter(ctx)
	if err != nil {
		return err
	}
	defer done()
	directory, ok := s3Directory(kind)
	if !ok || consume == nil {
		return errS3Handle
	}
	prefix := be.cfg.prefix + "/" + directory
	token, previous, count := "", "", 0
	seen := make(map[string]bool)
	for page := 0; page < s3MaxPages; page++ {
		if owned.Err() != nil {
			return owned.Err()
		}
		query := url.Values{"list-type": {"2"}, "max-keys": {strconv.Itoa(s3PageKeys)}, "prefix": {prefix}}
		if token != "" {
			query.Set("continuation-token", token)
		}
		reply, body, err := be.request(owned, http.MethodGet, "", query, nil, "")
		if err != nil {
			return err
		}
		if reply.StatusCode != http.StatusOK {
			zeroBytes(body)
			return errS3Protocol
		}
		files, next, last, err := be.decodeList(body, kind, prefix, previous)
		zeroBytes(body)
		if err != nil {
			return err
		}
		if next != "" && seen[next] {
			return errS3Protocol
		}
		if len(files) > maxPackListCount-count {
			return errResourceLimit
		}
		count += len(files)
		for _, file := range files {
			if owned.Err() != nil {
				return owned.Err()
			}
			if err := consume(file); err != nil {
				return s3ConsumerError(owned, err)
			}
		}
		if owned.Err() != nil {
			return owned.Err()
		}
		if next == "" {
			return nil
		}
		seen[next] = true
		token, previous = next, last
	}
	return errResourceLimit
}
