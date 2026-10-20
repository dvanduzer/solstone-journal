// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import AppKit
import Foundation
import MacTestSupport
import Testing
@testable import journal

@Suite("JournalAppearance")
@MainActor
struct JournalAppearanceTests {
    @Test func nothingInTheJournalAppSetsTheAppearance() throws {
        let root = journalPackageRoot()
        let files = try #require(AppearanceSourceAssertions.swiftFiles(under: root.appendingPathComponent("Sources/journal")))
        #expect(!files.isEmpty, "no Swift sources under Sources/journal")
        for file in files {
            let source = try String(contentsOf: file, encoding: .utf8)
            let violations = AppearanceSourceAssertions.violations(in: source, file: file.lastPathComponent)
            #expect(violations.isEmpty, "\(violations.joined(separator: "\n"))")
        }
        // …and no plist pins the app to the light appearance either
        for plist in ["Sources/journal/Info.plist"] {
            let text = try String(contentsOf: root.appendingPathComponent(plist), encoding: .utf8)
            #expect(!text.contains("NSRequiresAquaSystemAppearance"), "\(plist) forces the light appearance")
        }
    }
}
