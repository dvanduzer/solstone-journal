// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import Testing
@testable import journal

@Suite("JournalSetupContinuationStore")
struct JournalSetupContinuationStoreTests {
    @Test func atomicSaveRoundTripsAndEncodesNullPending() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let root = directory.appendingPathComponent("journal", isDirectory: true)
        let store = JournalSetupContinuationStore(directory: directory)
        let pending = UUID()

        try store.save(root: root, pending: pending)
        try store.save(root: root, pending: nil)

        let record = try #require(store.load())
        #expect(record.journalRootPath == root.standardizedFileURL.path)
        #expect(record.pendingStep5HandoffID == nil)
        let file = directory.appendingPathComponent(JournalSetupContinuationStore.fileName)
        let object = try #require(JSONSerialization.jsonObject(with: Data(contentsOf: file)) as? [String: Any])
        #expect(object["schema"] as? String == JournalSetupContinuationStore.schema)
        #expect(object["pendingStep5HandoffID"] is NSNull)
        #expect(store.load(matching: root)?.journalRootPath == root.standardizedFileURL.path)
    }

    @Test func failedWriteThrowsAndLeavesPreviousRecord() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let first = directory.appendingPathComponent("one", isDirectory: true)
        let second = directory.appendingPathComponent("two", isDirectory: true)
        let gate = WriteGate()
        let store = JournalSetupContinuationStore(directory: directory) { data, url in
            if gate.fail {
                throw CocoaError(.fileWriteUnknown)
            }
            try data.write(to: url, options: .atomic)
        }
        let pending = UUID()
        try store.save(root: first, pending: pending)
        gate.fail = true

        #expect(throws: CocoaError.self) {
            try store.save(root: second, pending: nil)
        }

        let record = try #require(store.load())
        #expect(record.journalRootPath == first.standardizedFileURL.path)
        #expect(record.pendingStep5HandoffID == pending)
    }

    @Test func corruptLoadIsNil() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = JournalSetupContinuationStore(directory: directory)
        try Data("not json".utf8).write(to: directory.appendingPathComponent(JournalSetupContinuationStore.fileName))
        #expect(store.load() == nil)

        try Data("""
        {"schema":"other","journalRootPath":"/journal","pendingStep5HandoffID":null}
        """.utf8).write(to: directory.appendingPathComponent(JournalSetupContinuationStore.fileName))
        #expect(store.load() == nil)

        try Data("""
        {"schema":"\(JournalSetupContinuationStore.schema)","journalRootPath":"relative","pendingStep5HandoffID":null}
        """.utf8).write(to: directory.appendingPathComponent(JournalSetupContinuationStore.fileName))
        #expect(store.load() == nil)
    }

    @Test func rootMismatchDoesNotApply() throws {
        let directory = try makeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let root = directory.appendingPathComponent("journal", isDirectory: true)
        let other = directory.appendingPathComponent("other", isDirectory: true)
        let store = JournalSetupContinuationStore(directory: directory)
        try store.save(root: root, pending: nil)

        #expect(store.load(matching: other) == nil)
        #expect(store.load(matching: root) != nil)
    }

    private func makeDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("journal-continuation-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}

private final class WriteGate: @unchecked Sendable {
    var fail = false
}
