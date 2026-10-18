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

func journalSetupPeerEvidenceAccepts(combinedOutput: String, capabilities: Any?) -> Bool {
    journalSupportsSetupSequence(capabilities)
        && combinedOutput.contains("Identifier=app.solstone.observer")
        && combinedOutput.contains("TeamIdentifier=7QCG8V4M6H")
}

@MainActor
final class JournalSetupPeerResolver: JournalSetupPeerResolving {
    static let bundleIdentifier = "app.solstone.observer"

    private let runner: any SubprocessRunning
    private let lookup: (String) -> URL?
    private let capabilities: (URL) -> Any?
    private let launchApplication: (URL) async throws -> Void

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
        }
    ) {
        self.runner = runner
        self.lookup = lookup
        self.capabilities = capabilities
        self.launchApplication = launchApplication
    }

    func resolve() -> URL? {
        lookup(Self.bundleIdentifier)
    }

    func verify(_ url: URL) async -> Bool {
        let plist = capabilities(url)
        let output = await codesignOutput(for: url)
        return journalSetupPeerEvidenceAccepts(combinedOutput: output, capabilities: plist)
    }

    func launch(_ url: URL) async throws {
        try await launchApplication(url)
    }

    private func codesignOutput(for url: URL) async -> String {
        let binary = URL(fileURLWithPath: "/usr/bin/codesign")
        var combined = ""
        let argumentSets = [
            ["--verify", "--strict", "--deep", "--verbose=2", url.path],
            ["-dvvv", url.path],
        ]
        for arguments in argumentSets {
            let box = PeerOutputBox()
            do {
                _ = try await runner.run(
                    executable: binary,
                    arguments: arguments,
                    environment: nil,
                    timeout: .seconds(30),
                    stdoutHandler: { box.append($0) },
                    stderrHandler: { box.append($0) }
                )
            } catch {
                continue
            }
            combined += box.text
        }
        return combined
    }
}

private final class PeerOutputBox: @unchecked Sendable {
    private let lock = NSLock()
    private var storage = Data()

    var text: String {
        lock.lock()
        defer { lock.unlock() }
        return String(decoding: storage, as: UTF8.self)
    }

    func append(_ data: Data) {
        lock.lock()
        storage.append(data)
        lock.unlock()
    }
}
