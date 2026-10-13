// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import MacTestSupport
import Testing

/// Every logger the Journal app and its runtime adapter construct names a subsystem the
/// persisted log help lists. The capture package runs the same scan over its own sources.
@Suite("Journal log subsystems")
struct JournalLogSubsystemScannerTests {
    @Test func journalSourcesWalkPassesAgainstAllForPersistedHelp() throws {
        let files = try swiftSourceFiles(under: journalPackageRoot().appendingPathComponent("Sources", isDirectory: true))
        let result = scanLogSubsystemConstructions(files)
        #expect(result.ok, "\(result.failures.joined(separator: "\n"))")
    }
}
