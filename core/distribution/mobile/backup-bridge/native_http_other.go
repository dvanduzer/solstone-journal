//go:build !darwin && !ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import "context"

func performNativeHTTP(_ context.Context, _, _ []byte, _ int) (nativeHTTPReply, error) {
	return nativeHTTPReply{}, errHTTPUnsupported
}
