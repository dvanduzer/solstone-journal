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
    static let signingRequirement =
        "=anchor apple generic and identifier \"\(requiredIdentifier)\" "
            + "and certificate leaf[subject.OU] = \"\(requiredTeam)\""

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
        guard await codesignSucceeds([
            "--verify", "--strict", "--deep", "--verbose=2",
            "-R", Self.signingRequirement, url.path,
        ]) != nil else {
            return false
        }
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
