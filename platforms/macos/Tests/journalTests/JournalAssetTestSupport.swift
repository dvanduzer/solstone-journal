// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation

/// The Journal app's Swift package root (platforms/macos), from this test file's own path,
/// so source and asset reads never depend on the runner's working directory.
func journalPackageRoot(filePath: String = #filePath) -> URL {
    URL(fileURLWithPath: filePath)
        .deletingLastPathComponent()   // JournalAssetTestSupport.swift
        .deletingLastPathComponent()   // journalTests
        .deletingLastPathComponent()   // Tests
}
