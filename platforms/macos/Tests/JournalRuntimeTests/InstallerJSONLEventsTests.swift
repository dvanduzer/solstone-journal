// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import Testing
@testable import JournalRuntime

@Suite("InstallerJSONLEvents")
struct InstallerJSONLEventsTests {
    @Test func parsesAndRendersEachEventType() {
        assertEvent(
            #"{"event":"setup.started","version":"0.2.1","mode":"non_interactive"}"#,
            .setupStarted(version: "0.2.1", mode: "non_interactive"),
            "setup started (version 0.2.1, non_interactive)"
        )
        assertEvent(
            #"{"event":"step.started","step":"doctor","index":1,"total":6}"#,
            .stepStarted(step: "doctor", index: 1, total: 6),
            "step 1/6: doctor"
        )
        assertEvent(
            #"{"event":"step.started","step":"skills_user","index":4,"total":7}"#,
            .stepStarted(step: "skills_user", index: 4, total: 7),
            "step 4/7: skills_user"
        )
        assertEvent(
            #"{"event":"doctor.started","version":"0.2.1"}"#,
            .doctorStarted(version: "0.2.1"),
            "doctor started"
        )
        assertEvent(
            #"{"event":"check.completed","name":"python_version","severity":"blocker","status":"ok","detail":"python ok","fix":""}"#,
            .checkCompleted(name: "python_version", severity: "blocker", status: "ok", detail: "python ok", fix: ""),
            "  [ok] python_version - python ok"
        )
        assertEvent(
            #"{"event":"doctor.completed","status":"warning","duration_ms":422,"summary":{"total":17,"failed":0,"warnings":3,"skipped":5}}"#,
            .doctorCompleted(status: "warning", durationMS: 422, summary: DoctorSummary(total: 17, failed: 0, warnings: 3, skipped: 5)),
            "doctor: 17 checks (0 failed, 3 warnings, 5 skipped)"
        )
        assertEvent(
            #"{"event":"step.completed","step":"doctor","outcome":"ok","duration_ms":121}"#,
            .stepCompleted(step: "doctor", outcome: "ok", reason: nil, durationMS: 121),
            "step doctor done (ok, 121ms)"
        )
        assertEvent(
            #"{"event":"step.completed","step":"skills_journal","outcome":"ok","duration_ms":42}"#,
            .stepCompleted(step: "skills_journal", outcome: "ok", reason: nil, durationMS: 42),
            "step skills_journal done (ok, 42ms)"
        )
        assertEvent(
            #"{"event":"step.completed","step":"skills_user","outcome":"skipped","reason":"--skip-skills"}"#,
            .stepCompleted(step: "skills_user", outcome: "skipped", reason: "--skip-skills", durationMS: nil),
            "step skills_user skipped (--skip-skills)"
        )
        assertEvent(
            #"{"event":"step.warning","step":"doctor","text":"port in use","fix_hint":"kill 123"}"#,
            .stepWarning(step: "doctor", text: "port in use", fixHint: "kill 123"),
            "  warn: port in use (fix: kill 123)"
        )
        assertEvent(
            #"{"event":"step.failed","step":"doctor","error":{"code":"port_in_use_non_interactive","message":"port failed","details":"","exit_code":2}}"#,
            .stepFailed(step: "doctor", errorCode: "port_in_use_non_interactive", message: "port failed", details: "", exitCode: 2),
            "step doctor failed: port failed"
        )
        assertEvent(
            #"{"event":"setup.completed","status":"ok","duration_ms":4000}"#,
            .setupCompleted(status: "ok", durationMS: 4000, failedStep: nil),
            "setup ok (4000ms)"
        )
    }

    @Test func rendersAlternateStatusesAndSkippedStep() {
        assertEvent(
            #"{"event":"check.completed","name":"port_5015_free","status":"warning","detail":"port busy"}"#,
            .checkCompleted(name: "port_5015_free", severity: nil, status: "warning", detail: "port busy", fix: nil),
            "  [warn] port_5015_free - port busy"
        )
        assertEvent(
            #"{"event":"check.completed","name":"launchd_stale_plist","status":"skipped","detail":"launchd plist absent"}"#,
            .checkCompleted(name: "launchd_stale_plist", severity: nil, status: "skipped", detail: "launchd plist absent", fix: nil),
            "  [skip] launchd_stale_plist - launchd plist absent"
        )
        assertEvent(
            #"{"event":"step.completed","step":"install_models","outcome":"skipped","reason":"--skip-models"}"#,
            .stepCompleted(step: "install_models", outcome: "skipped", reason: "--skip-models", durationMS: nil),
            "step install_models skipped (--skip-models)"
        )
        assertEvent(
            #"{"event":"setup.completed","status":"failed","failed_step":"doctor"}"#,
            .setupCompleted(status: "failed", durationMS: nil, failedStep: "doctor"),
            "setup failed at doctor"
        )
    }

    @Test func parsesAndRendersBrainSkipImpliedBySkipModels() {
        assertEvent(
            #"{"event":"step.completed","step":"brain","outcome":"skipped","reason":"--skip-models implies --skip-brain"}"#,
            .stepCompleted(step: "brain", outcome: "skipped", reason: "--skip-models implies --skip-brain", durationMS: nil),
            "step brain skipped (--skip-models implies --skip-brain)"
        )
    }

    @Test func parsesAndRendersBrainProviderSkipReason() {
        assertEvent(
            #"{"event":"step.completed","step":"brain","outcome":"skipped","reason":"a provider is already configured"}"#,
            .stepCompleted(step: "brain", outcome: "skipped", reason: "a provider is already configured", durationMS: nil),
            "step brain skipped (a provider is already configured)"
        )
    }

    @Test func negativeCasesClassifyRawLines() {
        #expect(SetupEventParser.parse(line: "{") == .malformed("{"))
        #expect(SetupEventParser.parse(line: #"{"event":"future.event"}"#) == .unrecognized(#"{"event":"future.event"}"#))
        #expect(SetupEventParser.parse(line: "   ") == .unparseableLine("   "))
        #expect(SetupEventParser.parse(line: #"{"event":"step.started""#) == .malformed(#"{"event":"step.started""#))
    }

    @Test func extraFieldsAreTolerated() {
        assertEvent(
            #"{"event":"step.started","step":"doctor","index":1,"total":6,"hint":"future"}"#,
            .stepStarted(step: "doctor", index: 1, total: 6),
            "step 1/6: doctor"
        )
    }

    @Test func setupStartedMissingVersionRendersPlainly() {
        assertEvent(
            #"{"event":"setup.started","mode":"non_interactive"}"#,
            .setupStarted(version: nil, mode: "non_interactive"),
            "setup started"
        )
    }

    @Test func rendererSkipsEmptyUnparseableLines() {
        var renderer = EventRenderer()
        renderer.append(.unparseableLine(""))
        renderer.append(.unparseableLine("  "))
        #expect(renderer.renderedLog == "")
    }

    /// The native setup's own vocabulary and emitted lines, written by the
    /// `mac_app_setup_contract_fixture_matches_the_native_vocabulary` test in
    /// core/crates/solstone-core-setup, which keeps the fixture equal to the native
    /// authority. Every event type must be recognized; the free-text fields
    /// (steps, skip reasons, error codes, statuses) pass through, so each native
    /// value must still parse into an event and render.
    @Test func understandsEverySetupEventTheNativeRuntimeEmits() throws {
        let url = try #require(Bundle.module.url(
            forResource: "native-setup-contract",
            withExtension: "json",
            subdirectory: "Fixtures"
        ))
        let contract = try JSONDecoder().decode(NativeSetupContract.self, from: Data(contentsOf: url))
        #expect(contract.schemaVersion == 1)
        #expect(Set(InstallerKnownValues.eventTypes) == Set(contract.eventTypes))
        #expect(contract.lines.count == contract.eventTypes.count)

        var lines = contract.lines
        lines += contract.stepNames.map { line(["event": "step.started", "step": $0, "index": 1, "total": 1]) }
        lines += contract.skipReasons.map {
            line(["event": "step.completed", "step": "brain", "outcome": "skipped", "reason": $0])
        }
        lines += contract.errorCodes.map {
            line(["event": "step.failed", "step": "journal", "error": ["code": $0, "message": "failed"]])
        }
        lines += contract.statuses.map { line(["event": "setup.completed", "status": $0]) }
        for raw in lines {
            guard case .event(let event) = SetupEventParser.parse(line: raw) else {
                Issue.record("the app does not understand a native setup line: \(raw)")
                continue
            }
            var renderer = EventRenderer()
            renderer.append(event)
            #expect(!renderer.renderedLog.isEmpty, "\(raw)")
        }
    }

    private func assertEvent(_ line: String, _ expected: SetupEvent, _ rendered: String) {
        let parsed = SetupEventParser.parse(line: line)
        #expect(parsed == .event(expected))
        guard case .event(let event) = parsed else { return }
        var renderer = EventRenderer()
        renderer.append(event)
        #expect(renderer.renderedLog == rendered + "\n")
    }

    private func line(_ object: [String: Any]) -> String {
        let data = try! JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        return String(decoding: data, as: UTF8.self)
    }
}

private struct NativeSetupContract: Decodable {
    let schemaVersion: Int
    let eventTypes: [String]
    let stepNames: [String]
    let errorCodes: [String]
    let skipReasons: [String]
    let statuses: [String]
    let lines: [String]

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case eventTypes = "event_types"
        case stepNames = "step_names"
        case errorCodes = "error_codes"
        case skipReasons = "skip_reasons"
        case statuses
        case lines
    }
}
