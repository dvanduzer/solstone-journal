// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import Testing
import SolstoneCore
@testable import journal

@Suite("JournalSetupContinuationStore")
struct JournalSetupContinuationStoreTests {
    @Test(arguments: ["setup-refusal.json", "remove"])
    func refusalPersistenceFailureKeepsTheExactObligation(failure: String) throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = JournalSetupContinuationStore(directory: directory, replace: { data, url in
            if url.lastPathComponent == failure { throw CocoaError(.fileWriteUnknown) }
            try data.write(to: url, options: .atomic)
        }, remove: { url in
            if failure == "remove" { throw CocoaError(.fileWriteUnknown) }
            try FileManager.default.removeItem(at: url)
        })
        let root = directory.appendingPathComponent("journal")
        let input = UUID()
        try store.storeProgress(step1: input, journalRootPath: root.path)
        let pending = try store.mintPending(step1: input)
        let refusal = SetupHandoffRefusal(handoffID: pending, reason: .heldPairing)
        #expect(throws: (any Error).self) { try store.resolveRefusal(refusal) }
        #expect(try store.loadActive()?.pending == pending)
        let rebuilt = JournalSetupContinuationStore(directory: directory)
        if failure == "remove" { try rebuilt.repairActiveRefusal() }
        else { try rebuilt.resolveRefusal(refusal) }
        #expect(try rebuilt.loadActive() == nil)
        #expect(try rebuilt.loadRefusal()?.handoffID == pending)
        #expect(rebuilt.acceptance(of: input) == .accepted)
        try rebuilt.storeProgress(step1: input, journalRootPath: root.path)
        #expect(try rebuilt.loadActive() == nil)
        let newer = UUID()
        try rebuilt.storeProgress(step1: newer, journalRootPath: root.path)
        let next = try rebuilt.mintPending(step1: newer)
        try rebuilt.resolveRefusal(refusal)
        #expect(try rebuilt.loadActive()?.pending == next)
    }

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

    @Test func ledgerFailureRetainsMembershipAndSupersedingInputRepairsEveryAlias() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let gate = WriteGate()
        let store = JournalSetupContinuationStore(directory: directory, replace: { data, url in
            if gate.fail && url.lastPathComponent == JournalSetupContinuationStore.acceptanceFileName {
                throw CocoaError(.fileWriteUnknown)
            }
            try data.write(to: url, options: .atomic)
        })
        let root = directory.appendingPathComponent("journal", isDirectory: true)
        let owner = try store.storeLocal(root: root)
        let pending = try store.mintPending(step1: owner)
        let first = UUID(), second = UUID()
        gate.fail = true
        #expect(throws: CocoaError.self) {
            try store.storeProgress(step1: first, journalRootPath: root.path)
        }
        guard case .ready(let retained) = store.loadLocal() else {
            Issue.record("ledger failure must retain the original obligation")
            return
        }
        #expect(retained.localHandoffID == owner)
        #expect(retained.pendingStep5HandoffID == pending)
        #expect(retained.activeStep1Aliases == [first])
        #expect(store.acceptance(of: first) == .notAccepted)
        #expect(throws: JournalSetupContinuationError.failedRecord) {
            try store.retire(matchingPendingStep5: pending)
        }
        gate.fail = false
        try store.storeProgress(step1: second, journalRootPath: root.path)
        #expect(store.acceptance(of: first) == .accepted)
        #expect(store.acceptance(of: second) == .accepted)
        #expect(store.loadContinuation() == .missing)
        #expect(try store.mintPending(step1: owner) == pending)
        try store.retire(matchingPendingStep5: pending)
        #expect(try store.loadActive() == nil)
        store.prepareAcceptanceAttempt()
        try store.storeProgress(step1: first, journalRootPath: root.path)
        #expect(store.lastStoreWasDuplicate)
        #expect(try store.loadActive() == nil)
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
        guard case .ready(let joined) = reloaded.loadContinuation() else {
            Issue.record("new input must join the effective obligation")
            return
        }
        #expect(joined.step1HandoffID == step1)
        #expect(joined.activeStep1Aliases == [other])
        #expect(joined.pendingStep5HandoffID == pending)
        #expect(try reloaded.storeLocal(root: root) == step1)
        #expect(reloaded.loadLocal() == .missing)
        try reloaded.retire(matchingPendingStep5: pending)
        #expect(reloaded.loadContinuation() == .missing)
        try reloaded.storeProgress(step1: UUID(), journalRootPath: root.path)
        let nextOwner = try #require(try reloaded.loadActive()).owner
        let replacement = try reloaded.mintPending(step1: nextOwner)
        #expect(replacement != pending)
        try reloaded.retire(matchingPendingStep5: pending)
        #expect(try reloaded.loadActive()?.pending == replacement)
        #expect(reloaded.acceptance(of: step1) == .accepted)
    }

    @Test func retirementRequiresOriginalOwnerAndEveryAliasButNotSyntheticLocalOwner() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = JournalSetupContinuationStore(directory: directory)
        let root = directory.appendingPathComponent("journal")
        let original = UUID(), alias = UUID()
        try store.storeProgress(step1: original, journalRootPath: root.path)
        try store.storeProgress(step1: alias, journalRootPath: root.path)
        let pending = try store.mintPending(step1: original)
        let ledger = directory.appendingPathComponent(JournalSetupContinuationStore.acceptanceFileName)
        let before = try Data(contentsOf: directory.appendingPathComponent(JournalSetupContinuationStore.fileName))
        for omitted in [original, alias] {
            let ids = [original, alias].filter { $0 != omitted }.map { $0.uuidString.lowercased() }
            try JSONSerialization.data(withJSONObject: ["schema": JournalSetupContinuationStore.acceptanceSchema,
                "acceptedStep1HandoffIDs": ids]).write(to: ledger, options: .atomic)
            #expect(throws: JournalSetupContinuationError.failedRecord) {
                try store.retire(matchingPendingStep5: pending)
            }
            #expect(try Data(contentsOf: directory.appendingPathComponent(JournalSetupContinuationStore.fileName)) == before)
        }
        try store.repairActiveAcceptance()
        try store.retire(matchingPendingStep5: pending)
        let local = try store.storeLocal(root: root)
        let localPending = try store.mintPending(step1: local)
        #expect(store.acceptance(of: local) == .notAccepted)
        try store.retire(matchingPendingStep5: localPending)
        #expect(try store.loadActive() == nil)
    }

    @Test func conflictingFilesAndDeletionFailurePreserveTheObligation() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = JournalSetupContinuationStore(directory: directory)
        let root = directory.appendingPathComponent("journal")
        let local = try store.storeLocal(root: root)
        let pending = try store.mintPending(step1: local)
        let failedDelete = JournalSetupContinuationStore(directory: directory, remove: { _ in
            throw CocoaError(.fileWriteNoPermission)
        })
        #expect(throws: CocoaError.self) { try failedDelete.retire(matchingPendingStep5: pending) }
        #expect(try store.loadActive()?.pending == pending)
        let incomingURL = directory.appendingPathComponent(JournalSetupContinuationStore.fileName)
        let bytes = try JSONSerialization.data(withJSONObject: continuationObject(JournalSetupContinuation(
            step1HandoffID: UUID(), journalRootPath: root.path, pendingStep5HandoffID: UUID()
        )))
        try bytes.write(to: incomingURL, options: .atomic)
        #expect(throws: JournalSetupContinuationError.conflictingRecords) { try store.loadActive() }
        #expect(throws: JournalSetupContinuationError.conflictingRecords) { try store.storeLocal(root: root) }
        #expect(throws: JournalSetupContinuationError.conflictingRecords) {
            try store.storeProgress(step1: UUID(), journalRootPath: root.path)
        }
        #expect(try Data(contentsOf: incomingURL) == bytes)
        #expect(store.loadLocal() != .missing)
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
