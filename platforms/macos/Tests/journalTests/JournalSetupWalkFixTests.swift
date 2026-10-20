// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import AppKit
import JournalMarkKit
import Foundation
import Observation
@testable import JournalRuntime
import SetupChrome
import SwiftUI
import Testing
@testable import journal

@MainActor
private final class AcceptingLoginItems: LoginItemManaging {
    func register() throws {}
    func unregister() throws {}
}

private final class ChangeFlag: @unchecked Sendable {
    var fired = false
}

/// The by-eye walk of released Journal.app 2.0.52 (2026-10-20) found these on its setup steps.
/// Each test fails on the code the walk ran.
@Suite("Journal setup — walk fixes")
@MainActor
struct JournalSetupWalkFixTests {
    private static func source(_ file: String) throws -> String {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("Sources/journal/\(file)")
        return try String(contentsOf: url, encoding: .utf8)
    }

    // D2: the welcome switch redraws when a click flips the saved value. Observation has to see
    // the read through to the saved setting, or SwiftUI never re-renders the switch.
    @Test func theWelcomeLoginSwitchRedrawsWhenItsSavedValueFlips() {
        let suite = "app.solstone.journal.walk-fix.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set(true, forKey: "launchAtLoginEnabled")
        let config = JournalAppConfig(defaults: defaults, loginItemManager: AcceptingLoginItems())
        let fixture = makeModel(config: config, startResults: [], probeResults: [])
        let model = fixture.model
        model.route = .ritual(.welcome)
        let binding = model.welcomeLaunchAtLogin

        for expected in [false, true, false] {
            let flag = ChangeFlag()
            let shown = withObservationTracking {
                binding.wrappedValue
            } onChange: {
                flag.fired = true
            }
            #expect(shown == !expected)
            binding.wrappedValue = expected
            #expect(flag.fired, "the switch was not told its value changed to \(expected)")
            #expect(binding.wrappedValue == expected)
            #expect(defaults.bool(forKey: "launchAtLoginEnabled") == expected)
        }
    }

    // D4: after "decide later" the thinking card keeps its bottom padding and corners above the
    // footer at the 880 x 580 minimum. Four points above the footer is sheet, not card.
    @Test func theThinkingCardFitsAboveTheFooterAfterDecideLater() async throws {
        JournalMarkFont.register()
        let copy = try loadSetupFixtureCopy()
        let state = JournalSetupState(
            schema: "solstone.setup-state.v1",
            journalPath: "/tmp/journal",
            done: false,
            mark: .init(render: .uiTestSample, state: .locked, proposalID: "prop-1"),
            thinking: .init(choice: nil, saved: false, capability: .ok, downloadBytes: 1_500_000_000),
            downloads: .init(
                speech: .init(state: .idle, attemptID: nil, bytesDone: 0, bytesTotal: nil),
                thinking: .init(state: .idle, attemptID: nil, bytesDone: 0, bytesTotal: nil)
            )
        )
        let fixture = makeModel(startResults: [], probeResults: [])
        fixture.model.applySnapshotState(route: .ritual(.thinking), setupState: state, setupCopy: copy, draftThinkingChoice: .later)

        let size = CGSize(width: 880, height: 580)
        let view = JournalFirstRunView(
            model: fixture.model,
            minSize: size,
            background: SunArcBackgroundView(fixedDate: Date(timeIntervalSince1970: 1_790_190_000), timeZone: TimeZone(identifier: "America/Denver")!)
        )
        .frame(width: size.width, height: size.height)
        .environment(\.colorScheme, .dark)
        let host = NSHostingView(rootView: view)
        host.appearance = NSAppearance(named: .darkAqua)
        host.frame = NSRect(origin: .zero, size: size)
        host.layoutSubtreeIfNeeded()
        let rep = try #require(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: rep)
        let scale = CGFloat(rep.pixelsHigh) / size.height
        // The card's column, 4 pt above the footer's top hairline (18 pt inset + 66 pt bar + 1 pt).
        let y = rep.pixelsHigh - Int((18 + 66 + 1 + 4) * scale)
        let x = Int(size.width * 0.75 * scale)
        let color = try #require(rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB))
        let red = Int((color.redComponent * 255).rounded())
        // Dark sheet #221C19 (red 34); dark card #2B231C (red 43).
        #expect(red <= 38, "card fill runs into the footer: red \(red)")
    }

    // D8: the setting-up card never shows the runtime's internal step name, and no second
    // spinner sits in the band over the sun. The step name stays in its AX companion.
    @Test func settingUpShowsNoInternalStepNameAndNoBandSpinner() throws {
        let view = try Self.source("JournalFirstRunView.swift")
        #expect(!view.contains("Text(model.currentStep"), "the internal step name is shown as text")
        #expect(view.contains("AXStateCompanion(id: AXID.Journal.Ritual.setupStepState, value: model.currentStep ?? \"\")"))
        #expect(!view.contains(".overlay(alignment: .top) {\n            if model.userOperationInFlight"), "the band spinner is back")
    }
}
