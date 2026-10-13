// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import AppKit
import CoreGraphics
import MacTestSupport
import Testing
@testable import JournalMarkKit

/// The committed app icon, rendered from this package's own asset, keeps transparent corners
/// at every size the mark compositor produces.
@MainActor
@Suite("Journal committed app icon")
struct JournalCommittedIconTests {
    @Test func committedICNSCornersStayTransparentAtEveryLadderSide() throws {
        let url = journalPackageRoot().appendingPathComponent("Sources/journal/Resources/AppIcon.icns")
        let image = try #require(NSImage(contentsOf: url))

        for side in JournalIconCompositor.ladderSides {
            var rect = NSRect(x: 0, y: 0, width: side, height: side)
            let cgImage = try #require(image.cgImage(forProposedRect: &rect, context: nil, hints: nil))
            let snapshot = try #require(RGBASnapshot(image: cgImage))
            #expect(snapshot.alphaAt(x: 0, y: 0) == 0)
            #expect(snapshot.alphaAt(x: snapshot.width - 1, y: 0) == 0)
            #expect(snapshot.alphaAt(x: 0, y: snapshot.height - 1) == 0)
            #expect(snapshot.alphaAt(x: snapshot.width - 1, y: snapshot.height - 1) == 0)
        }
    }
}
