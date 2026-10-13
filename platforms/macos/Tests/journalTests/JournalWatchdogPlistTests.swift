// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import SolstoneCore
import Testing

/// The Journal app's watchdog LaunchAgent keeps the shared watchdog's launch contract and its
/// inert environment rows; the watchdog reads its product from the enclosing bundle.
@Suite("Journal watchdog LaunchAgent")
struct JournalWatchdogPlistTests {
    @Test func journalPlistKeepsInertWatchdogEnvironmentContract() throws {
        let plist = try loadWatchdogPlist("Sources/journal/app.solstone.journal.watchdog.plist")

        #expect(plist["Label"] as? String == "app.solstone.journal.watchdog")
        #expect(plist["BundleProgram"] as? String == "Contents/MacOS/solstone-watchdog")
        #expect(plist["RunAtLoad"] as? Bool == true)
        #expect(plist["ThrottleInterval"] as? Int == 30)
        #expect(plist["AssociatedBundleIdentifiers"] as? [String] == ["app.solstone.journal"])

        let environment = try #require(plist["EnvironmentVariables"] as? [String: String])
        #expect(environment["SOLSTONE_WATCHDOG_TARGET_BUNDLE_ID"] == "app.solstone.journal")
        #expect(environment["SOLSTONE_WATCHDOG_LOGGER_SUBSYSTEM"] == "app.solstone.journal.watchdog")
        #expect(environment["SOLSTONE_WATCHDOG_MARKER_DISCRIMINATOR"] == ExpectedExitMarker.journalMarkerDiscriminator)
        #expect(!environment.values.contains { $0.hasPrefix("/") })
    }

    private func loadWatchdogPlist(_ relativePath: String) throws -> [String: Any] {
        let plistURL = journalPackageRoot().appendingPathComponent(relativePath)
        let data = try Data(contentsOf: plistURL)
        let value = try PropertyListSerialization.propertyList(from: data, options: [], format: nil)
        return try #require(value as? [String: Any])
    }
}
