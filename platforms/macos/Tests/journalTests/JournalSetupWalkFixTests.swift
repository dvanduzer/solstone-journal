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
}
