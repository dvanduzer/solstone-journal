// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import SolstoneCore

struct JournalSetupContinuation: Equatable, Sendable {
    var journalRootPath: String
    var pendingStep5HandoffID: UUID?
}

struct JournalSetupContinuationStore: Sendable {
    static let schema = "solstone.journal-setup-continuation.v1"
    static let fileName = "setup-continuation.json"

    let directory: URL
    private let replace: @Sendable (Data, URL) throws -> Void

    init(
        directory: URL? = nil,
        replace: @escaping @Sendable (Data, URL) throws -> Void = journalSetupContinuationReplace
    ) {
        self.directory = directory ?? ExpectedExitMarker
            .markerURL(for: ExpectedExitMarker.journalMarkerDiscriminator)
            .deletingLastPathComponent()
        self.replace = replace
    }

    func load() -> JournalSetupContinuation? {
        guard let data = try? Data(contentsOf: fileURL) else { return nil }
        return Self.decode(data)
    }

    func load(matching root: URL) -> JournalSetupContinuation? {
        guard let record = load() else { return nil }
        guard Self.standardizedPath(record.journalRootPath) == root.standardizedFileURL.path else {
            return nil
        }
        return record
    }

    func save(root: URL, pending: UUID?) throws {
        let standardized = root.standardizedFileURL
        guard standardized.path.hasPrefix("/") else {
            throw CocoaError(.fileWriteInvalidFileName)
        }
        let payload = Payload(
            schema: Self.schema,
            journalRootPath: standardized.path,
            pendingStep5HandoffID: pending?.uuidString.lowercased()
        )
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(payload)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try replace(data, fileURL)
    }

    func remove() throws {
        guard FileManager.default.fileExists(atPath: fileURL.path) else { return }
        try FileManager.default.removeItem(at: fileURL)
    }

    private var fileURL: URL {
        directory.appendingPathComponent(Self.fileName)
    }

    private static func decode(_ data: Data) -> JournalSetupContinuation? {
        guard let payload = try? JSONDecoder().decode(Payload.self, from: data) else { return nil }
        guard payload.schema == schema else { return nil }
        guard payload.journalRootPath.hasPrefix("/") else { return nil }
        let pending: UUID?
        if let raw = payload.pendingStep5HandoffID {
            guard let parsed = UUID(uuidString: raw) else { return nil }
            pending = parsed
        } else {
            pending = nil
        }
        return JournalSetupContinuation(
            journalRootPath: payload.journalRootPath,
            pendingStep5HandoffID: pending
        )
    }

    static func standardizedPath(_ path: String) -> String {
        URL(fileURLWithPath: path).standardizedFileURL.path
    }
}

private struct Payload: Codable {
    var schema: String
    var journalRootPath: String
    var pendingStep5HandoffID: String?

    private enum CodingKeys: String, CodingKey {
        case schema
        case journalRootPath
        case pendingStep5HandoffID
    }

    init(schema: String, journalRootPath: String, pendingStep5HandoffID: String?) {
        self.schema = schema
        self.journalRootPath = journalRootPath
        self.pendingStep5HandoffID = pendingStep5HandoffID
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        schema = try container.decode(String.self, forKey: .schema)
        journalRootPath = try container.decode(String.self, forKey: .journalRootPath)
        if try container.decodeNil(forKey: .pendingStep5HandoffID) {
            pendingStep5HandoffID = nil
        } else {
            pendingStep5HandoffID = try container.decode(String.self, forKey: .pendingStep5HandoffID)
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(schema, forKey: .schema)
        try container.encode(journalRootPath, forKey: .journalRootPath)
        if let pendingStep5HandoffID {
            try container.encode(pendingStep5HandoffID, forKey: .pendingStep5HandoffID)
        } else {
            try container.encodeNil(forKey: .pendingStep5HandoffID)
        }
    }
}

func journalSetupContinuationReplace(data: Data, to destination: URL) throws {
    try data.write(to: destination, options: .atomic)
}
