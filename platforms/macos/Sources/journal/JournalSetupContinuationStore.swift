// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Darwin
import Foundation
import SolstoneCore

enum JournalSetupRecordLoad<Value: Equatable & Sendable>: Equatable, Sendable {
    case missing
    case failed
    case ready(Value)
}

struct JournalSetupContinuation: Equatable, Sendable {
    var step1HandoffID: UUID
    var journalRootPath: String?
    var pendingStep5HandoffID: UUID?
}

struct JournalSetupAcceptanceLedger: Equatable, Sendable {
    var acceptedStep1HandoffIDs: [UUID]
}

enum JournalSetupAcceptanceLookup: Equatable, Sendable {
    case unreadable
    case notAccepted
    case accepted
}

enum JournalSetupContinuationError: Error, Equatable {
    case failedRecord
    case lockTimeout
    case mailboxWhileLocked
    case notReady
    case invalidRoot
}

struct JournalSetupContinuationStore: Sendable {
    static let schema = "solstone.journal-setup-continuation.v1"
    static let acceptanceSchema = "solstone.journal-setup-acceptance.v1"
    static let fileName = "setup-continuation.json"
    static let acceptanceFileName = "setup-acceptance.json"
    static let lockFileName = "setup-continuation.lock"

    let directory: URL
    private let replace: @Sendable (Data, URL) throws -> Void
    private let read: @Sendable (URL) throws -> Data
    private let lock: @Sendable (URL, @Sendable () throws -> Void) throws -> Bool
    private let shared = Shared()

    init(
        directory: URL? = nil,
        replace: @escaping @Sendable (Data, URL) throws -> Void = journalSetupContinuationReplace,
        read: @escaping @Sendable (URL) throws -> Data = { try Data(contentsOf: $0) },
        lock: @escaping @Sendable (URL, @Sendable () throws -> Void) throws -> Bool = journalSetupContinuationLock
    ) {
        self.directory = directory ?? ExpectedExitMarker
            .markerURL(for: ExpectedExitMarker.journalMarkerDiscriminator)
            .deletingLastPathComponent()
        self.replace = replace
        self.read = read
        self.lock = lock
    }

    func loadContinuation() -> JournalSetupRecordLoad<JournalSetupContinuation> {
        loadRecord(at: continuationURL, decode: Self.decodeContinuation)
    }

    func loadLedger() -> JournalSetupRecordLoad<JournalSetupAcceptanceLedger> {
        loadRecord(at: acceptanceURL, decode: Self.decodeLedger)
    }

    func acceptance(of step1: UUID) -> JournalSetupAcceptanceLookup {
        switch loadLedger() {
        case .failed:
            return .unreadable
        case .missing:
            return .notAccepted
        case .ready(let ledger):
            return ledger.acceptedStep1HandoffIDs.contains(step1) ? .accepted : .notAccepted
        }
    }

    func ready(matching root: URL) -> JournalSetupContinuation? {
        guard case .ready(let record) = loadContinuation(),
              let path = record.journalRootPath else {
            return nil
        }
        guard Self.standardizedPath(path) == root.standardizedFileURL.path else {
            return nil
        }
        return record
    }

    var isLockHeld: Bool {
        shared.mutex.lock()
        defer { shared.mutex.unlock() }
        return shared.held
    }

    var lastStoreWasDuplicate: Bool {
        shared.mutex.lock()
        defer { shared.mutex.unlock() }
        return shared.duplicate
    }

    func prepareAcceptanceAttempt() {
        shared.mutex.lock()
        shared.duplicate = false
        shared.mutex.unlock()
    }

    func assertMailboxAllowed() throws {
        if isLockHeld {
            throw JournalSetupContinuationError.mailboxWhileLocked
        }
    }

    func withLockHeld(_ body: @Sendable () throws -> Void) throws {
        try withContinuationLock(body)
    }

    /// Writes the continuation first, then the acceptance ledger. A ledger failure restores the
    /// previous continuation. An id already in a readable ledger is a duplicate and is not rewritten.
    func storeProgress(step1: UUID, journalRootPath: String?) throws {
        if let journalRootPath {
            guard Self.isAbsoluteRoot(journalRootPath) else {
                throw JournalSetupContinuationError.invalidRoot
            }
        }
        try withContinuationLock {
            let continuationLoad = loadContinuation()
            let ledgerLoad = loadLedger()
            if case .failed = continuationLoad {
                throw JournalSetupContinuationError.failedRecord
            }
            if case .failed = ledgerLoad {
                throw JournalSetupContinuationError.failedRecord
            }
            if case .ready(let ledger) = ledgerLoad, ledger.acceptedStep1HandoffIDs.contains(step1) {
                shared.mutex.lock()
                shared.duplicate = true
                shared.mutex.unlock()
                return
            }

            let previous = try previousContinuationBytes(continuationLoad)
            var pending: UUID?
            if case .ready(let existing) = continuationLoad, existing.step1HandoffID == step1 {
                pending = existing.pendingStep5HandoffID
            }
            let record = JournalSetupContinuation(
                step1HandoffID: step1,
                journalRootPath: journalRootPath,
                pendingStep5HandoffID: pending
            )
            try writeContinuation(record)
            do {
                var ids: [UUID] = []
                if case .ready(let ledger) = ledgerLoad {
                    ids = ledger.acceptedStep1HandoffIDs
                }
                ids.append(step1)
                try writeLedger(JournalSetupAcceptanceLedger(acceptedStep1HandoffIDs: ids))
            } catch {
                restoreContinuation(previous)
                throw error
            }
        }
    }

    func bindRoot(step1: UUID, root: URL) throws {
        let path = root.standardizedFileURL.path
        guard Self.isAbsoluteRoot(path) else {
            throw JournalSetupContinuationError.invalidRoot
        }
        try withContinuationLock {
            guard case .ready(let record) = loadContinuation(), record.step1HandoffID == step1 else {
                throw JournalSetupContinuationError.notReady
            }
            var updated = record
            updated.journalRootPath = path
            try writeContinuation(updated)
        }
    }

    func mintPending(step1: UUID) throws -> UUID {
        try withContinuationLock {
            guard case .ready(let record) = loadContinuation(),
                  record.step1HandoffID == step1,
                  record.journalRootPath != nil else {
                throw JournalSetupContinuationError.notReady
            }
            if let pending = record.pendingStep5HandoffID {
                return pending
            }
            let pending = UUID()
            var updated = record
            updated.pendingStep5HandoffID = pending
            try writeContinuation(updated)
            return pending
        }
    }

    /// Deletes the continuation only when a fresh read still shows the same step 1 and pending step 5.
    func retire(matchingPendingStep5 pending: UUID) throws {
        try withContinuationLock {
            guard case .ready(let first) = loadContinuation(),
                  first.pendingStep5HandoffID == pending else {
                return
            }
            guard case .ready(let second) = loadContinuation(),
                  second.step1HandoffID == first.step1HandoffID,
                  second.pendingStep5HandoffID == pending else {
                return
            }
            do {
                try FileManager.default.removeItem(at: continuationURL)
            } catch let error as CocoaError where error.code == .fileNoSuchFile {
                return
            }
        }
    }

    private var continuationURL: URL {
        directory.appendingPathComponent(Self.fileName)
    }

    private var acceptanceURL: URL {
        directory.appendingPathComponent(Self.acceptanceFileName)
    }

    private var lockURL: URL {
        directory.appendingPathComponent(Self.lockFileName)
    }

    private func withContinuationLock<T: Sendable>(_ body: @Sendable () throws -> T) throws -> T {
        let box = LockResultBox<T>()
        let acquired: Bool
        do {
            acquired = try lock(lockURL) {
                self.shared.mutex.lock()
                self.shared.held = true
                self.shared.mutex.unlock()
                defer {
                    self.shared.mutex.lock()
                    self.shared.held = false
                    self.shared.mutex.unlock()
                }
                box.store(try body())
            }
        } catch {
            throw error
        }
        guard acquired else {
            throw JournalSetupContinuationError.lockTimeout
        }
        return try box.take()
    }

    private func loadRecord<Value>(
        at url: URL,
        decode: (Data) -> JournalSetupRecordLoad<Value>
    ) -> JournalSetupRecordLoad<Value> {
        let data: Data
        do {
            data = try read(url)
        } catch {
            return Self.isMissingFile(error) ? .missing : .failed
        }
        return decode(data)
    }

    private func previousContinuationBytes(
        _ load: JournalSetupRecordLoad<JournalSetupContinuation>
    ) throws -> Data? {
        switch load {
        case .missing:
            return nil
        case .failed:
            throw JournalSetupContinuationError.failedRecord
        case .ready:
            return try read(continuationURL)
        }
    }

    private func writeContinuation(_ record: JournalSetupContinuation) throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try replace(Self.encodeContinuation(record), continuationURL)
    }

    private func writeLedger(_ ledger: JournalSetupAcceptanceLedger) throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try replace(Self.encodeLedger(ledger), acceptanceURL)
    }

    private func restoreContinuation(_ previous: Data?) {
        if let previous {
            try? replace(previous, continuationURL)
            return
        }
        if FileManager.default.fileExists(atPath: continuationURL.path) {
            try? FileManager.default.removeItem(at: continuationURL)
        }
    }

    private static func isAbsoluteRoot(_ path: String) -> Bool {
        !path.isEmpty && path.hasPrefix("/")
    }

    static func standardizedPath(_ path: String) -> String {
        URL(fileURLWithPath: path).standardizedFileURL.path
    }

    private static func isMissingFile(_ error: Error) -> Bool {
        let ns = error as NSError
        if ns.domain == NSCocoaErrorDomain && ns.code == NSFileReadNoSuchFileError {
            return true
        }
        if ns.domain == NSPOSIXErrorDomain && ns.code == Int(ENOENT) {
            return true
        }
        if let underlying = ns.userInfo[NSUnderlyingErrorKey] as? Error {
            return isMissingFile(underlying)
        }
        return false
    }

    private static func decodeContinuation(_ data: Data) -> JournalSetupRecordLoad<JournalSetupContinuation> {
        guard let object = jsonObject(data, allowed: [
            "schema", "step1HandoffID", "journalRootPath", "pendingStep5HandoffID",
        ]) else {
            return .failed
        }
        guard object["schema"] as? String == schema else { return .failed }
        guard let step1 = uuid(object["step1HandoffID"]) else { return .failed }
        let root: String?
        if object.keys.contains("journalRootPath") {
            if object["journalRootPath"] is NSNull {
                root = nil
            } else if let path = object["journalRootPath"] as? String, isAbsoluteRoot(path) {
                root = path
            } else {
                return .failed
            }
        } else {
            root = nil
        }
        let pending: UUID?
        if object.keys.contains("pendingStep5HandoffID") {
            if object["pendingStep5HandoffID"] is NSNull {
                pending = nil
            } else if let value = uuid(object["pendingStep5HandoffID"]) {
                pending = value
            } else {
                return .failed
            }
        } else {
            pending = nil
        }
        return .ready(JournalSetupContinuation(
            step1HandoffID: step1,
            journalRootPath: root,
            pendingStep5HandoffID: pending
        ))
    }

    private static func decodeLedger(_ data: Data) -> JournalSetupRecordLoad<JournalSetupAcceptanceLedger> {
        guard let object = jsonObject(data, allowed: ["schema", "acceptedStep1HandoffIDs"]) else {
            return .failed
        }
        guard object["schema"] as? String == acceptanceSchema else { return .failed }
        guard let rawIDs = object["acceptedStep1HandoffIDs"] as? [Any] else { return .failed }
        var ids: [UUID] = []
        var seen: Set<String> = []
        for raw in rawIDs {
            guard let id = uuid(raw) else { return .failed }
            let key = id.uuidString.lowercased()
            guard seen.insert(key).inserted else { return .failed }
            ids.append(id)
        }
        return .ready(JournalSetupAcceptanceLedger(acceptedStep1HandoffIDs: ids))
    }

    private static func jsonObject(_ data: Data, allowed: Set<String>) -> [String: Any]? {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            return nil
        }
        guard Set(object.keys).isSubset(of: allowed) else { return nil }
        return object
    }

    private static func uuid(_ raw: Any?) -> UUID? {
        guard let text = raw as? String, let id = UUID(uuidString: text) else { return nil }
        return id
    }

    private static func encodeContinuation(_ record: JournalSetupContinuation) throws -> Data {
        var object: [String: Any] = [
            "schema": schema,
            "step1HandoffID": record.step1HandoffID.uuidString.lowercased(),
        ]
        if let root = record.journalRootPath {
            object["journalRootPath"] = root
        } else {
            object["journalRootPath"] = NSNull()
        }
        if let pending = record.pendingStep5HandoffID {
            object["pendingStep5HandoffID"] = pending.uuidString.lowercased()
        } else {
            object["pendingStep5HandoffID"] = NSNull()
        }
        return try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    private static func encodeLedger(_ ledger: JournalSetupAcceptanceLedger) throws -> Data {
        let object: [String: Any] = [
            "acceptedStep1HandoffIDs": ledger.acceptedStep1HandoffIDs.map { $0.uuidString.lowercased() },
            "schema": acceptanceSchema,
        ]
        return try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
    }

    private final class Shared: @unchecked Sendable {
        let mutex = NSLock()
        var held = false
        var duplicate = false
    }

    private final class LockResultBox<T: Sendable>: @unchecked Sendable {
        private let mutex = NSLock()
        private var value: Result<T, Error>?

        func store(_ value: T) {
            mutex.lock()
            self.value = .success(value)
            mutex.unlock()
        }

        func take() throws -> T {
            mutex.lock()
            defer { mutex.unlock() }
            guard let value else {
                throw JournalSetupContinuationError.lockTimeout
            }
            return try value.get()
        }
    }
}

func journalSetupContinuationReplace(data: Data, to destination: URL) throws {
    try data.write(to: destination, options: .atomic)
}

func journalSetupContinuationLock(
    on url: URL,
    body: @Sendable () throws -> Void
) throws -> Bool {
    try FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(),
        withIntermediateDirectories: true
    )
    let fd = open(url.path, O_RDWR | O_CREAT, 0o600)
    guard fd >= 0 else { return false }
    defer { close(fd) }

    let deadline = Date().addingTimeInterval(2.0)
    var acquired = false
    while Date() < deadline {
        if flock(fd, LOCK_EX | LOCK_NB) == 0 {
            acquired = true
            break
        }
        let err = errno
        if err != EWOULDBLOCK && err != EAGAIN {
            return false
        }
        usleep(50_000)
    }
    guard acquired else { return false }
    defer { flock(fd, LOCK_UN) }
    try body()
    return true
}
