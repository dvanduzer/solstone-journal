// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import Testing
@testable import journal

@Suite("JournalSetupContinuationStore")
struct JournalSetupContinuationStoreTests {
    @Test func missingIOAndMalformedLoadsStayDistinct() throws {
        let missing = try makeStore { _ in
            throw NSError(domain: NSCocoaErrorDomain, code: NSFileReadNoSuchFileError)
        }
        #expect(missing.loadContinuation() == .missing)
        #expect(missing.loadLedger() == .missing)

        let posix = try makeStore { _ in
            throw NSError(domain: NSPOSIXErrorDomain, code: Int(ENOENT))
        }
        #expect(posix.loadContinuation() == .missing)

        let wrapped = try makeStore { _ in
            throw NSError(
                domain: NSCocoaErrorDomain,
                code: NSFileReadUnknownError,
                userInfo: [NSUnderlyingErrorKey: NSError(domain: NSPOSIXErrorDomain, code: Int(ENOENT))]
            )
        }
        #expect(wrapped.loadContinuation() == .missing)

        let io = try makeStore { _ in throw CocoaError(.fileReadUnknown) }
        #expect(io.loadContinuation() == .failed)
        #expect(io.loadLedger() == .failed)

        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = JournalSetupContinuationStore(directory: directory)
        let step1 = UUID()
        let root = directory.appendingPathComponent("journal", isDirectory: true).path
        try store.storeProgress(step1: step1, journalRootPath: root)

        for body in [
            Data("not json".utf8),
            Data(#"{"schema":"other","step1HandoffID":"\#(step1.uuidString)"}"#.utf8),
            Data(#"{"schema":"\#(JournalSetupContinuationStore.schema)","step1HandoffID":"\#(step1.uuidString)","extra":true}"#.utf8),
            Data(#"{"schema":"\#(JournalSetupContinuationStore.schema)","step1HandoffID":"not-a-uuid"}"#.utf8),
            Data(#"{"schema":"\#(JournalSetupContinuationStore.schema)","step1HandoffID":"\#(step1.uuidString)","journalRootPath":""}"#.utf8),
            Data(#"{"schema":"\#(JournalSetupContinuationStore.schema)","step1HandoffID":"\#(step1.uuidString)","journalRootPath":"relative"}"#.utf8),
        ] {
            let url = directory.appendingPathComponent(JournalSetupContinuationStore.fileName)
            try body.write(to: url, options: .atomic)
            #expect(store.loadContinuation() == .failed)
            #expect(throws: JournalSetupContinuationError.failedRecord) {
                try store.storeProgress(step1: UUID(), journalRootPath: root)
            }
            #expect(try Data(contentsOf: url) == body)
        }

        let ledgerURL = directory.appendingPathComponent(JournalSetupContinuationStore.acceptanceFileName)
        let duplicate = Data("""
        {"acceptedStep1HandoffIDs":["\(step1.uuidString.lowercased())","\(step1.uuidString.lowercased())"],"schema":"\(JournalSetupContinuationStore.acceptanceSchema)"}
        """.utf8)
        try duplicate.write(to: ledgerURL, options: .atomic)
        #expect(store.loadLedger() == .failed)
        #expect(throws: JournalSetupContinuationError.failedRecord) {
            try store.storeProgress(step1: UUID(), journalRootPath: root)
        }
        #expect(try Data(contentsOf: ledgerURL) == duplicate)
    }

    @Test func ledgerWriteFailureRestoresContinuationAndDuplicateDoesNotRewrite() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let gate = WriteGate()
        let store = JournalSetupContinuationStore(directory: directory) { data, url in
            if gate.fail && url.lastPathComponent == JournalSetupContinuationStore.acceptanceFileName {
                throw CocoaError(.fileWriteUnknown)
            }
            try data.write(to: url, options: .atomic)
        }
        let absolute = directory.appendingPathComponent("journal", isDirectory: true).standardizedFileURL.path
        let first = UUID()
        gate.fail = true
        #expect(throws: CocoaError.self) {
            try store.storeProgress(step1: first, journalRootPath: absolute)
        }
        #expect(store.loadContinuation() == .missing)
        #expect(store.loadLedger() == .missing)
        #expect(!FileManager.default.fileExists(
            atPath: directory.appendingPathComponent(JournalSetupContinuationStore.fileName).path
        ))

        gate.fail = false
        try store.storeProgress(step1: first, journalRootPath: absolute)
        let before = try Data(contentsOf: directory.appendingPathComponent(JournalSetupContinuationStore.fileName))
        let second = UUID()
        gate.fail = true
        #expect(throws: CocoaError.self) {
            try store.storeProgress(step1: second, journalRootPath: absolute)
        }
        #expect(try Data(contentsOf: directory.appendingPathComponent(JournalSetupContinuationStore.fileName)) == before)
        #expect(store.acceptance(of: second) == .notAccepted)
        #expect(store.acceptance(of: first) == .accepted)

        gate.fail = false
        store.prepareAcceptanceAttempt()
        try store.storeProgress(step1: first, journalRootPath: absolute)
        #expect(store.lastStoreWasDuplicate)
        #expect(try Data(contentsOf: directory.appendingPathComponent(JournalSetupContinuationStore.fileName)) == before)
    }

    @Test func rootlessIntentReloadsBindMintsOnceAndRetireIsConditional() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let step1 = UUID()
        let root = directory.appendingPathComponent("journal", isDirectory: true)
        try JournalSetupContinuationStore(directory: directory)
            .storeProgress(step1: step1, journalRootPath: nil)
        let reloaded = JournalSetupContinuationStore(directory: directory)
        guard case .ready(let rootless) = reloaded.loadContinuation() else {
            Issue.record("expected a rootless continuation")
            return
        }
        #expect(rootless.journalRootPath == nil)
        #expect(rootless.step1HandoffID == step1)
        #expect(reloaded.acceptance(of: step1) == .accepted)

        try reloaded.bindRoot(step1: step1, root: root)
        #expect(reloaded.ready(matching: root)?.journalRootPath == root.standardizedFileURL.path)

        let pending = try reloaded.mintPending(step1: step1)
        #expect(try reloaded.mintPending(step1: step1) == pending)

        let other = UUID()
        try reloaded.storeProgress(step1: other, journalRootPath: root.standardizedFileURL.path)
        let replacement = try reloaded.mintPending(step1: other)
        try reloaded.retire(matchingPendingStep5: pending)
        guard case .ready(let kept) = reloaded.loadContinuation() else {
            Issue.record("an older pending id must not retire the active continuation")
            return
        }
        #expect(kept.step1HandoffID == other)
        #expect(kept.pendingStep5HandoffID == replacement)
        #expect(reloaded.acceptance(of: step1) == .accepted)

        try reloaded.retire(matchingPendingStep5: replacement)
        #expect(reloaded.loadContinuation() == .missing)
        #expect(reloaded.acceptance(of: other) == .accepted)
    }

    @Test func mailboxCallUnderTheContinuationLockIsRefusedAndRereadMismatchDoesNotDelete() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = JournalSetupContinuationStore(directory: directory)
        #expect(throws: JournalSetupContinuationError.mailboxWhileLocked) {
            try store.withLockHeld {
                try store.assertMailboxAllowed()
            }
        }

        let script = ReadScript()
        let scripted = JournalSetupContinuationStore(directory: directory, read: { url in
            try script.read(url)
        })
        let step1 = UUID()
        let pending = UUID()
        let root = directory.appendingPathComponent("journal", isDirectory: true).standardizedFileURL.path
        try scripted.storeProgress(step1: step1, journalRootPath: root)
        let record = JournalSetupContinuation(
            step1HandoffID: step1,
            journalRootPath: root,
            pendingStep5HandoffID: pending
        )
        let url = directory.appendingPathComponent(JournalSetupContinuationStore.fileName)
        try JSONSerialization.data(
            withJSONObject: continuationObject(record),
            options: [.sortedKeys]
        ).write(to: url, options: .atomic)
        script.mismatch = true
        try scripted.retire(matchingPendingStep5: pending)
        #expect(script.continuationReads >= 2)
        #expect(FileManager.default.fileExists(atPath: url.path))
        guard case .ready(let loaded) = JournalSetupContinuationStore(directory: directory).loadContinuation() else {
            Issue.record("mismatch must leave the continuation file")
            return
        }
        #expect(loaded.pendingStep5HandoffID == pending)
    }

    private func makeDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("journal-continuation-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    private func makeStore(
        read: @escaping @Sendable (URL) throws -> Data
    ) throws -> JournalSetupContinuationStore {
        JournalSetupContinuationStore(directory: try makeDirectory(), read: read)
    }

    private func continuationObject(_ record: JournalSetupContinuation) -> [String: Any] {
        [
            "journalRootPath": record.journalRootPath ?? NSNull(),
            "pendingStep5HandoffID": record.pendingStep5HandoffID?.uuidString.lowercased() ?? NSNull(),
            "schema": JournalSetupContinuationStore.schema,
            "step1HandoffID": record.step1HandoffID.uuidString.lowercased(),
        ]
    }
}

private final class WriteGate: @unchecked Sendable {
    var fail = false
}

private final class ReadScript: @unchecked Sendable {
    private let lock = NSLock()
    var mismatch = false
    private(set) var continuationReads = 0

    func read(_ url: URL) throws -> Data {
        let data = try Data(contentsOf: url)
        guard url.lastPathComponent == JournalSetupContinuationStore.fileName else { return data }
        lock.lock()
        let armed = mismatch
        if armed { continuationReads += 1 }
        let count = continuationReads
        lock.unlock()
        guard armed, count >= 2 else { return data }
        var object = try JSONSerialization.jsonObject(with: data) as? [String: Any] ?? [:]
        object["pendingStep5HandoffID"] = UUID().uuidString.lowercased()
        return try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }
}
