// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import AppKit
import Foundation
import JournalRuntime
import SolstoneCore

@MainActor
protocol JournalSetupPeerResolving: AnyObject {
    func resolve() -> URL?
    func verify(_ url: URL) async -> Bool
    func launch(_ url: URL) async throws
}

@MainActor
final class JournalSetupPeerResolver: JournalSetupPeerResolving {
    static let bundleIdentifier = "app.solstone.observer"
    static let requiredIdentifier = "app.solstone.observer"
    static let requiredTeam = "7QCG8V4M6H"
    static let outputCap = 64 * 1024

    private let runner: any SubprocessRunning
    private let lookup: (String) -> URL?
    private let capabilities: (URL) -> Any?
    private let launchApplication: (URL) async throws -> Void
    private let commandTimeout: Duration

    init(
        runner: any SubprocessRunning = SubprocessRunner(),
        lookup: @escaping (String) -> URL? = { NSWorkspace.shared.urlForApplication(withBundleIdentifier: $0) },
        capabilities: @escaping (URL) -> Any? = { url in
            Bundle(url: url)?.object(forInfoDictionaryKey: "SolstoneSetupCapabilities")
        },
        launchApplication: @escaping (URL) async throws -> Void = { url in
            let configuration = NSWorkspace.OpenConfiguration()
            configuration.activates = true
            _ = try await NSWorkspace.shared.openApplication(at: url, configuration: configuration)
        },
        commandTimeout: Duration = .seconds(30)
    ) {
        self.runner = runner
        self.lookup = lookup
        self.capabilities = capabilities
        self.launchApplication = launchApplication
        self.commandTimeout = commandTimeout
    }

    func resolve() -> URL? {
        lookup(Self.bundleIdentifier)
    }

    func verify(_ url: URL) async -> Bool {
        guard await codesignSucceeds(["--verify", "--strict", "--deep", "--verbose=2", url.path]) != nil else {
            return false
        }
        guard let described = await codesignSucceeds(["-dvvv", "-r-", url.path]) else {
            return false
        }
        guard Self.designatedRequirementAccepts(described) else { return false }
        return journalSupportsSetupSequence(capabilities(url))
    }

    func launch(_ url: URL) async throws {
        try await launchApplication(url)
    }

    private func codesignSucceeds(_ arguments: [String]) async -> String? {
        let box = PeerOutputBox(limit: Self.outputCap)
        let result: SubprocessResult
        do {
            result = try await runner.run(
                executable: URL(fileURLWithPath: "/usr/bin/codesign"),
                arguments: arguments,
                environment: nil,
                timeout: commandTimeout,
                stdoutHandler: { box.append($0) },
                stderrHandler: { box.append($0) }
            )
        } catch {
            return nil
        }
        guard result.terminationReason == .exit, result.exitCode == 0, !box.truncated else {
            return nil
        }
        return box.text
    }

    static func designatedRequirementAccepts(_ output: String) -> Bool {
        guard let marker = output.range(of: "designated =>") else { return false }
        let requirement = String(output[marker.upperBound...])
        let identifiers = quotedValues(in: requirement, label: "identifier")
        let teams = quotedValues(in: requirement, label: "subject.OU")
        return identifiers == [requiredIdentifier] && teams == [requiredTeam]
    }

    private static func quotedValues(in text: String, label: String) -> [String] {
        var values: [String] = []
        var search = text.startIndex
        while let range = text.range(of: label, range: search..<text.endIndex) {
            if !isTokenStart(text, range.lowerBound) {
                search = range.upperBound
                continue
            }
            var index = range.upperBound
            while index < text.endIndex {
                let character = text[index]
                if character == "]" || character == " " || character == "\t" || character == "=" {
                    index = text.index(after: index)
                    continue
                }
                break
            }
            guard index < text.endIndex, text[index] == "\"" else {
                search = range.upperBound
                continue
            }
            let valueStart = text.index(after: index)
            guard let end = text[valueStart...].firstIndex(of: "\"") else { break }
            values.append(String(text[valueStart..<end]))
            search = text.index(after: end)
        }
        return values
    }

    private static func isTokenStart(_ text: String, _ index: String.Index) -> Bool {
        guard index > text.startIndex else { return true }
        let previous = text[text.index(before: index)]
        return !previous.isLetter && !previous.isNumber && previous != "_" && previous != "."
    }
}

private final class PeerOutputBox: @unchecked Sendable {
    private let lock = NSLock()
    private var storage = Data()
    private let limit: Int
    private(set) var truncated = false

    init(limit: Int) {
        self.limit = limit
    }

    var text: String {
        lock.lock()
        defer { lock.unlock() }
        return String(decoding: storage, as: UTF8.self)
    }

    func append(_ data: Data) {
        lock.lock()
        defer { lock.unlock() }
        if truncated || storage.count >= limit {
            truncated = true
            return
        }
        let room = limit - storage.count
        if data.count > room {
            storage.append(data.prefix(room))
            truncated = true
        } else {
            storage.append(data)
        }
    }
}
