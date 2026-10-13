// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Darwin
import Foundation
@testable import JournalRuntime
import JournalRuntimeTestSupport

// A real native journal under the real supervisor, for the capture app's opt-in door test.
// Debug builds only: it reaches the runtime's internal containment reader through @testable.
// The capture package cannot import JournalRuntime, so it starts this test-only executable,
// reads one `ready` record, runs its assertions against the returned journal, then writes
// `stop` (or closes stdin) and requires one `stopped` record and exit 0.
//
// Protocol, one JSON object per stdout line (diagnostics go to stderr):
//   {"schema_version":1,"event":"ready","journal_root":…,"convey_base_url":…,
//    "direct_door_port":…,"door_state":"withheld","runtime_root":…}
//   {"schema_version":1,"event":"stopped","all_admitted_processes_gone":…,
//    "forbidden_home_artifacts":[…],"fixture_tree_removed":…}
//   {"schema_version":1,"event":"error","stage":…,"detail":…}
// A `stopped` record with any false or non-empty measurement exits 1. Never shipped in an app.

private func emit(_ record: [String: Any]) {
    var record = record
    record["schema_version"] = 1
    let data = try! JSONSerialization.data(withJSONObject: record, options: [.sortedKeys])
    FileHandle.standardOutput.write(data + Data("\n".utf8))
}

private struct StageError: Error {
    let stage: String
    let detail: String
}

@MainActor
private func stage<T>(_ name: String, _ body: () async throws -> T) async throws -> T {
    do {
        return try await body()
    } catch let error as StageError {
        throw error
    } catch {
        throw StageError(stage: name, detail: "\(error)")
    }
}

/// Blocks until the caller writes `stop` or closes stdin. A caller that dies closes stdin too,
/// so the journal it started never outlives it.
private func waitForStop() async {
    await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
        DispatchQueue.global().async {
            while let line = readLine() {
                if line.trimmingCharacters(in: .whitespaces) == "stop" { break }
            }
            continuation.resume()
        }
    }
}

@MainActor
private func runFixture() async -> Int32 {
    let fixture: NativeRuntimeFixture
    do {
        fixture = try NativeRuntimeFixture(label: "door")
    } catch {
        emit(["event": "error", "stage": "fixture", "detail": "\(error)"])
        return 1
    }
    let runner = SupervisedJournalRunner(
        statusSink: { _ in },
        gate: MockSingleSupervisorGate(result: .success),
        requiredModels: JournalRequiredModelsReconciler()
    )
    let admitted = AdmittedProcesses()
    var started = false
    var exitCode: Int32 = 0

    do {
        let setup = try await stage("setup") { try await NativeIntegrationHelpers.runAppSetup(fixture) }
        guard setup.succeeded else { throw StageError(stage: "setup", detail: setup.stderr) }
        let directPort = try await stage("ports") { try NativeRuntimeFixture.reserveLoopbackPort() }
        let conveyPort = try await stage("ports") { try NativeRuntimeFixture.reserveLoopbackPort() }
        try await stage("configure") { try NativeIntegrationHelpers.configureDirectDoorPort(fixture, port: directPort) }
        try await stage("required-models") {
            try await JournalRequiredModelsReconciler().reconcile(runtime: fixture.runtime, journalRoot: fixture.journalRoot)
        }
        started = true
        try await stage("start") {
            try await runner.start(
                runtime: fixture.runtime,
                journalRoot: fixture.journalRoot,
                port: conveyPort,
                receiptContext: NativeIntegrationHelpers.silentReceiptContext()
            )
        }
        guard let identity = await runner.currentIdentity() else {
            throw StageError(stage: "start", detail: "supervisor reported no child identity")
        }
        admitted.record([identity.pid])
        let readiness = await JournalReadinessGate().waitUntilReady(
            journalRoot: fixture.journalRoot,
            runtime: fixture.runtime,
            timeout: .seconds(120),
            terminalCheck: { await runner.terminalReason() },
            identityProvider: { await runner.currentIdentity() },
            readinessAcceptance: { await runner.markReady(identity: $0) }
        )
        guard readiness == .ready else { throw StageError(stage: "readiness", detail: "\(readiness)") }
        let reader = LiveJournalProcessContainmentEvidenceReader()
        if let evidence = reader.containmentEvidence(for: identity.pid),
           let group = reader.processIDs(inProcessGroup: evidence.processGroupID) {
            admitted.record(group)
        }
        guard let door = NativeIntegrationHelpers.readDirectDoorRecord(fixture) else {
            throw StageError(stage: "door", detail: "no health/direct-door.json under the journal root")
        }
        emit([
            "event": "ready",
            "journal_root": fixture.journalRoot.path,
            "convey_base_url": "http://127.0.0.1:\(conveyPort)",
            "direct_door_port": directPort,
            "door_state": door.state,
            "runtime_root": fixture.runtimeRoot.path,
        ])
        await waitForStop()
    } catch let error as StageError {
        emit(["event": "error", "stage": error.stage, "detail": error.detail])
        exitCode = 1
    } catch {
        emit(["event": "error", "stage": "unknown", "detail": "\(error)"])
        exitCode = 1
    }

    if started {
        await runner.stopForTermination()
    }
    let gone = await NativeIntegrationHelpers.waitUntil(timeout: .seconds(20)) {
        admitted.all().allSatisfy { !NativeIntegrationHelpers.processExists($0) }
    }
    if !gone {
        NativeIntegrationHelpers.killSurvivors(admitted.all())
    }
    let forbidden = fixture.forbiddenHomeArtifacts()
    let base = fixture.home.deletingLastPathComponent()
    fixture.clear()
    let removed = !FileManager.default.fileExists(atPath: base.path)
    if exitCode == 0 {
        emit([
            "event": "stopped",
            "all_admitted_processes_gone": gone,
            "forbidden_home_artifacts": forbidden,
            "fixture_tree_removed": removed,
        ])
        if !gone || !forbidden.isEmpty || !removed { exitCode = 1 }
    }
    return exitCode
}

let code = await runFixture()
exit(code)
