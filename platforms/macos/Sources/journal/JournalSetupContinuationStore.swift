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
    var activeStep1Aliases: [UUID] = []
}

struct JournalSetupLocalContinuation: Equatable, Sendable {
    var localHandoffID: UUID
    var journalRootPath: String
    var pendingStep5HandoffID: UUID?
    var activeStep1Aliases: [UUID] = []
}

enum JournalSetupActiveContinuation: Equatable, Sendable {
    case incoming(JournalSetupContinuation)
    case local(JournalSetupLocalContinuation)

    var owner: UUID {
        switch self {
        case .incoming(let record): record.step1HandoffID
        case .local(let record): record.localHandoffID
        }
    }

    var rootPath: String? {
        switch self {
        case .incoming(let record): record.journalRootPath
        case .local(let record): record.journalRootPath
        }
    }

    var pending: UUID? {
        switch self {
        case .incoming(let record): record.pendingStep5HandoffID
        case .local(let record): record.pendingStep5HandoffID
        }
    }

    var acceptedInputs: [UUID] {
        switch self {
        case .incoming(let record): [record.step1HandoffID] + record.activeStep1Aliases
        case .local(let record): record.activeStep1Aliases
        }
    }
}

struct JournalSetupRefusalResolution: Codable, Equatable, Sendable {
    var schema = "solstone.journal-setup-refusal.v1"
    let handoffID: UUID
    let journalRootPath: String
    let reason: SetupHandoffRefusalReason
    var acknowledged = false
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
    case conflictingRoot
    case conflictingRecords
}

struct JournalSetupContinuationStore: Sendable {
    static let schema = "solstone.journal-setup-continuation.v1"
    static let acceptanceSchema = "solstone.journal-setup-acceptance.v1"
    static let localSchema = "solstone.journal-setup-local.v1"
    static let fileName = "setup-continuation.json"
    static let acceptanceFileName = "setup-acceptance.json"
    static let localFileName = "setup-local.json"
    static let lockFileName = "setup-continuation.lock"

    let directory: URL
    private let replace: @Sendable (Data, URL) throws -> Void
    private let read: @Sendable (URL) throws -> Data
    private let lock: @Sendable (URL, @Sendable () throws -> Void) throws -> Bool
    private let remove: @Sendable (URL) throws -> Void
    private let shared = Shared()

    init(
        directory: URL? = nil,
        replace: @escaping @Sendable (Data, URL) throws -> Void = journalSetupContinuationReplace,
        read: @escaping @Sendable (URL) throws -> Data = { try Data(contentsOf: $0) },
        lock: @escaping @Sendable (URL, @Sendable () throws -> Void) throws -> Bool = journalSetupContinuationLock,
        remove: @escaping @Sendable (URL) throws -> Void = { try FileManager.default.removeItem(at: $0) }
    ) {
        self.directory = directory ?? ExpectedExitMarker
            .markerURL(for: ExpectedExitMarker.journalMarkerDiscriminator)
            .deletingLastPathComponent()
        self.replace = replace
        self.read = read
        self.lock = lock
        self.remove = remove
    }

    func loadContinuation() -> JournalSetupRecordLoad<JournalSetupContinuation> {
        loadRecord(at: continuationURL, decode: Self.decodeContinuation)
    }

    func loadLocal() -> JournalSetupRecordLoad<JournalSetupLocalContinuation> {
        loadRecord(at: localURL, decode: Self.decodeLocal)
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

    /// Both formats share one effective obligation under the stable continuation lock.
    func loadActive() throws -> JournalSetupActiveContinuation? {
        try withContinuationLock { try activeUnlocked() }
    }

    private func activeUnlocked() throws -> JournalSetupActiveContinuation? {
        let incoming = loadContinuation()
        let local = loadLocal()
        if case .failed = incoming { throw JournalSetupContinuationError.failedRecord }
        if case .failed = local { throw JournalSetupContinuationError.failedRecord }
        switch (incoming, local) {
        case (.ready, .ready): throw JournalSetupContinuationError.conflictingRecords
        case (.ready(let record), .missing): return .incoming(record)
        case (.missing, .ready(let record)): return .local(record)
        default: return nil
        }
    }

    /// Persist membership first. A failed ledger write retains every accepted input for recovery.
    func storeProgress(step1: UUID, journalRootPath: String?) throws {
        if let journalRootPath, !Self.isAbsoluteRoot(journalRootPath) {
            throw JournalSetupContinuationError.invalidRoot
        }
        try withContinuationLock {
            let active = try activeUnlocked()
            let ledger = try ledgerUnlocked()
            if ledger.contains(step1), !(active?.acceptedInputs.contains(step1) ?? false) {
                shared.mutex.lock()
                shared.duplicate = true
                shared.mutex.unlock()
                return
            }
            let joined: JournalSetupActiveContinuation
            switch active {
            case .incoming(var record):
                try validateRoot(existing: record.journalRootPath, proposed: journalRootPath)
                if record.journalRootPath == nil { record.journalRootPath = journalRootPath }
                if !([record.step1HandoffID] + record.activeStep1Aliases).contains(step1) {
                    record.activeStep1Aliases.append(step1)
                }
                joined = .incoming(record)
            case .local(var record):
                try validateRoot(existing: record.journalRootPath, proposed: journalRootPath)
                if !record.activeStep1Aliases.contains(step1) { record.activeStep1Aliases.append(step1) }
                joined = .local(record)
            case nil:
                try supersedeRefusal()
                joined = .incoming(JournalSetupContinuation(
                    step1HandoffID: step1, journalRootPath: journalRootPath, pendingStep5HandoffID: nil
                ))
            }
            if joined != active { try writeActive(joined) }
            try coverInputs(joined.acceptedInputs, existing: ledger)
        }
    }

    func bindRoot(step1: UUID, root: URL) throws {
        let path = root.standardizedFileURL.path
        guard Self.isAbsoluteRoot(path) else { throw JournalSetupContinuationError.invalidRoot }
        try withContinuationLock {
            guard let active = try activeUnlocked(), active.owner == step1 else {
                throw JournalSetupContinuationError.notReady
            }
            try validateRoot(existing: active.rootPath, proposed: path)
            if case .incoming(var record) = active, record.journalRootPath == nil {
                record.journalRootPath = path
                try writeContinuation(record)
            }
        }
    }

    func mintPending(step1: UUID) throws -> UUID {
        try withContinuationLock {
            guard let active = try activeUnlocked(), active.owner == step1, active.rootPath != nil else {
                throw JournalSetupContinuationError.notReady
            }
            if let pending = active.pending { return pending }
            let pending = UUID()
            switch active {
            case .incoming(var record):
                record.pendingStep5HandoffID = pending
                try writeContinuation(record)
            case .local(var record):
                record.pendingStep5HandoffID = pending
                try writeLocal(record)
            }
            return pending
        }
    }

    func pendingStep5(ownedBy handoff: UUID) throws -> UUID? {
        guard let active = try loadActive(), active.owner == handoff else { return nil }
        return active.pending
    }

    /// Reuses either active format; no second obligation can be created alongside it.
    func storeLocal(root: URL) throws -> UUID {
        let path = root.standardizedFileURL.path
        guard Self.isAbsoluteRoot(path) else { throw JournalSetupContinuationError.invalidRoot }
        return try withContinuationLock {
            if let active = try activeUnlocked() {
                try validateRoot(existing: active.rootPath, proposed: path)
                if case .incoming(var record) = active, record.journalRootPath == nil {
                    record.journalRootPath = path
                    try writeContinuation(record)
                }
                return active.owner
            }
            try supersedeRefusal()
            let record = JournalSetupLocalContinuation(
                localHandoffID: UUID(), journalRootPath: path, pendingStep5HandoffID: nil
            )
            try writeLocal(record)
            return record.localHandoffID
        }
    }

    func repairActiveAcceptance() throws {
        try withContinuationLock {
            guard let active = try activeUnlocked() else { return }
            try coverInputs(active.acceptedInputs, existing: ledgerUnlocked())
        }
    }

    private var resolutionURL: URL { directory.appendingPathComponent("setup-refusal.json") }

    func loadRefusal() throws -> JournalSetupRefusalResolution? {
        let data: Data
        do { data = try read(resolutionURL) }
        catch { if Self.isMissingFile(error) { return nil }; throw error }
        let value = try JSONDecoder().decode(JournalSetupRefusalResolution.self, from: data)
        guard value.schema == "solstone.journal-setup-refusal.v1",
              Self.isAbsoluteRoot(value.journalRootPath),
              Self.standardizedPath(value.journalRootPath) == value.journalRootPath else {
            throw JournalSetupContinuationError.failedRecord
        }
        return value
    }

    /// Persist the terminal outcome before retiring its exact obligation. A failed removal is retryable.
    func resolveRefusal(_ refusal: SetupHandoffRefusal) throws {
        try withContinuationLock { try resolveRefusalUnlocked(refusal) }
    }

    /// Cleanup can be interrupted after our local terminal decision became durable.
    func repairActiveRefusal() throws {
        try withContinuationLock {
            guard let value = try loadRefusal(), let active = try activeUnlocked(),
                  active.pending == value.handoffID else { return }
            guard active.rootPath.map(Self.standardizedPath) == value.journalRootPath else {
                throw JournalSetupContinuationError.conflictingRoot
            }
            try resolveRefusalUnlocked(SetupHandoffRefusal(handoffID: value.handoffID, reason: value.reason))
        }
    }

    private func resolveRefusalUnlocked(_ refusal: SetupHandoffRefusal) throws {
        guard let active = try activeUnlocked(), active.pending == refusal.handoffID,
              let root = active.rootPath else { return }
        let ledger = try ledgerUnlocked()
        guard active.acceptedInputs.allSatisfy({ ledger.contains($0) }) else {
            throw JournalSetupContinuationError.failedRecord
        }
        let prior = try loadRefusal()
        if prior?.handoffID != refusal.handoffID {
            let resolution = JournalSetupRefusalResolution(handoffID: refusal.handoffID,
                journalRootPath: Self.standardizedPath(root), reason: refusal.reason)
            try replace(JSONEncoder().encode(resolution), resolutionURL)
        } else if prior?.journalRootPath != Self.standardizedPath(root) {
            throw JournalSetupContinuationError.conflictingRoot
        }
        guard try activeUnlocked() == active else { return }
        switch active {
        case .incoming: try remove(continuationURL)
        case .local: try remove(localURL)
        }
    }

    func acknowledgeRefusal(_ id: UUID) throws {
        try withContinuationLock {
            guard var value = try loadRefusal(), value.handoffID == id else { return }
            value.acknowledged = true
            try replace(JSONEncoder().encode(value), resolutionURL)
        }
    }

    private func supersedeRefusal() throws {
        if var value = try loadRefusal(), !value.acknowledged {
            value.acknowledged = true
            try replace(JSONEncoder().encode(value), resolutionURL)
        }
    }

    func retire(matchingPendingStep5 pending: UUID) throws {
        try withContinuationLock {
            guard let first = try activeUnlocked(), first.pending == pending else { return }
            let ledger = try ledgerUnlocked()
            guard first.acceptedInputs.allSatisfy({ ledger.contains($0) }) else {
                throw JournalSetupContinuationError.failedRecord
            }
            guard try activeUnlocked() == first else { return }
            if var resolution = try loadRefusal(), resolution.handoffID == pending, !resolution.acknowledged {
                resolution.acknowledged = true
                try replace(JSONEncoder().encode(resolution), resolutionURL)
            }
            let url: URL
            switch first {
            case .incoming: url = continuationURL
            case .local: url = localURL
            }
            try remove(url)
        }
    }

    private func validateRoot(existing: String?, proposed: String?) throws {
        if let existing, let proposed,
           Self.standardizedPath(existing) != Self.standardizedPath(proposed) {
            throw JournalSetupContinuationError.conflictingRoot
        }
    }

    private func ledgerUnlocked() throws -> [UUID] {
        switch loadLedger() {
        case .failed: throw JournalSetupContinuationError.failedRecord
        case .missing: return []
        case .ready(let record): return record.acceptedStep1HandoffIDs
        }
    }

    private func coverInputs(_ inputs: [UUID], existing: [UUID]) throws {
        var ids = existing
        for input in inputs where !ids.contains(input) { ids.append(input) }
        if ids != existing { try writeLedger(JournalSetupAcceptanceLedger(acceptedStep1HandoffIDs: ids)) }
    }

    private func writeActive(_ active: JournalSetupActiveContinuation) throws {
        switch active {
        case .incoming(let record): try writeContinuation(record)
        case .local(let record): try writeLocal(record)
        }
    }

    private var continuationURL: URL {
        directory.appendingPathComponent(Self.fileName)
    }

    private var localURL: URL {
        directory.appendingPathComponent(Self.localFileName)
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

    private func writeContinuation(_ record: JournalSetupContinuation) throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try replace(Self.encodeContinuation(record), continuationURL)
    }

    private func writeLocal(_ record: JournalSetupLocalContinuation) throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try replace(Self.encodeLocal(record), localURL)
    }

    private func writeLedger(_ ledger: JournalSetupAcceptanceLedger) throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try replace(Self.encodeLedger(ledger), acceptanceURL)
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
            "schema", "step1HandoffID", "journalRootPath", "pendingStep5HandoffID", "activeStep1Aliases",
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
        guard let aliases = decodeAliases(object), !aliases.contains(step1) else { return .failed }
        return .ready(JournalSetupContinuation(
            step1HandoffID: step1,
            journalRootPath: root,
            pendingStep5HandoffID: pending,
            activeStep1Aliases: aliases
        ))
    }

    private static func decodeLocal(_ data: Data) -> JournalSetupRecordLoad<JournalSetupLocalContinuation> {
        guard let object = jsonObject(data, allowed: [
            "schema", "localHandoffID", "journalRootPath", "pendingStep5HandoffID", "activeStep1Aliases",
        ]) else {
            return .failed
        }
        guard object["schema"] as? String == localSchema else { return .failed }
        guard object.keys.contains("localHandoffID"), let localID = uuid(object["localHandoffID"]) else {
            return .failed
        }
        guard object.keys.contains("journalRootPath"),
              let path = object["journalRootPath"] as? String,
              isAbsoluteRoot(path) else {
            return .failed
        }
        guard object.keys.contains("pendingStep5HandoffID") else { return .failed }
        let pending: UUID?
        if object["pendingStep5HandoffID"] is NSNull {
            pending = nil
        } else if let value = uuid(object["pendingStep5HandoffID"]) {
            pending = value
        } else {
            return .failed
        }
        guard let aliases = decodeAliases(object) else { return .failed }
        return .ready(JournalSetupLocalContinuation(
            localHandoffID: localID,
            journalRootPath: path,
            pendingStep5HandoffID: pending,
            activeStep1Aliases: aliases
        ))
    }

    private static func decodeAliases(_ object: [String: Any]) -> [UUID]? {
        guard let raw = object["activeStep1Aliases"] else { return [] }
        guard let items = raw as? [Any] else { return nil }
        let ids = items.compactMap { uuid($0) }
        guard ids.count == items.count, Set(ids).count == ids.count else { return nil }
        return ids
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
            "activeStep1Aliases": record.activeStep1Aliases.map { $0.uuidString.lowercased() },
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

    private static func encodeLocal(_ record: JournalSetupLocalContinuation) throws -> Data {
        var object: [String: Any] = [
            "schema": localSchema,
            "activeStep1Aliases": record.activeStep1Aliases.map { $0.uuidString.lowercased() },
            "localHandoffID": record.localHandoffID.uuidString.lowercased(),
            "journalRootPath": record.journalRootPath,
        ]
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
