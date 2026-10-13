// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import Testing

/// The Journal app's half of the shared watchdog's source contracts: it quits through its
/// supervisor with a named reason, reads no legacy watchdog environment, and names no
/// temporary-directory path. The capture package checks the shared watchdog itself.
@Suite("Journal watchdog source contracts")
struct JournalWatchdogSourceContractTests {
    private static let legacyWatchdogEnvironmentKeys = [
        "SOLSTONE_WATCHDOG_TARGET_BUNDLE_ID",
        "SOLSTONE_WATCHDOG_LOGGER_SUBSYSTEM",
        "SOLSTONE_WATCHDOG_MARKER_DISCRIMINATOR"
    ]

    @Test func journalQuitsThroughItsSupervisorWithANamedReason() throws {
        let root = journalPackageRoot()
        let journalApp = try String(contentsOf: root.appendingPathComponent("Sources/journal/JournalApp.swift"), encoding: .utf8)
        let journalSupervisor = try String(contentsOf: root.appendingPathComponent("Sources/journal/JournalSupervisor.swift"), encoding: .utf8)

        #expect(journalApp.contains("await supervisor.terminate(reason: \"ordinary-quit\")"))
        #expect(journalApp.contains("await supervisor?.terminate(reason: \"updater-install\")"))
        #expect(journalSupervisor.contains("func terminate(reason: String = \"ordinary-quit\")"))
    }

    @Test func journalSourcesReadNoLegacyWatchdogEnvironmentAndNameNoTemporaryPaths() throws {
        let sources = try journalSwiftSources()
        #expect(!sources.isEmpty)
        for (url, content) in sources {
            #expect(!Self.legacyWatchdogEnvironmentKeys.contains { content.contains($0) }, "\(url.lastPathComponent)")
            #expect(!content.contains("/private/var/folders/"), "\(url.lastPathComponent)")
            #expect(!content.contains("/var/folders/"), "\(url.lastPathComponent)")
            #expect(!content.contains("$TMPDIR"), "\(url.lastPathComponent)")
        }
    }

    private func journalSwiftSources() throws -> [(URL, String)] {
        let sources = journalPackageRoot().appendingPathComponent("Sources", isDirectory: true)
        let enumerator = FileManager.default.enumerator(at: sources, includingPropertiesForKeys: nil)
        return try (enumerator?.allObjects as? [URL] ?? [])
            .filter { $0.pathExtension == "swift" }
            .map { ($0, try String(contentsOf: $0, encoding: .utf8)) }
    }
}
