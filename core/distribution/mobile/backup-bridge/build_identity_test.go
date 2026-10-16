// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"encoding/hex"
	"strings"
	"testing"
)

func TestBuildIdentityClosedEncoding(t *testing.T) {
	value := strings.Repeat("ab", 32)
	got, valid := decodeBuildIdentity(value)
	if !valid || hex.EncodeToString(got[:]) != value {
		t.Fatal("valid linked identity refused")
	}
	for _, malformed := range []string{"", value[:62], value + "00", strings.ToUpper(value), strings.Repeat("z", 64), value[:63] + "\x00"} {
		if _, valid := decodeBuildIdentity(malformed); valid {
			t.Fatal("noncanonical linked identity accepted")
		}
	}
}
