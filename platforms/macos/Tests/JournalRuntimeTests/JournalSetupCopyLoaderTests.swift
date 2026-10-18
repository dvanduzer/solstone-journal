// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import JournalRuntimeTestSupport
import Testing
@testable import JournalRuntime

@Suite("JournalSetupCopyLoader")
struct JournalSetupCopyLoaderTests {
    @Test func fixtureDocumentHasRequiredKeys() throws {
        let object = try JSONSerialization.jsonObject(with: fixtureData()) as? [String: Any]
        let document = try #require(object)
        #expect(document["schema"] as? String == "solstone.setup-copy.v1")
        #expect(document["heading"] is String)
        #expect(document["footer"] is String)

        let fits = try #require(document["fits"] as? [String: Any])
        for key in ["title", "journal_term", "journal", "app_term", "app", "thinking_term", "thinking"] {
            #expect(fits[key] is String)
        }
        let login = try #require(document["login"] as? [String: Any])
        #expect(login["label"] is String)
        #expect(login["help"] is String)
        let folder = try #require(document["folder"] as? [String: Any])
        for key in ["title", "help", "change", "continue"] {
            #expect(folder[key] is String)
        }
        let mark = try #require(document["mark"] as? [String: Any])
        for key in ["title", "help", "error", "permanence", "try_another", "continue"] {
            #expect(mark[key] is String)
        }
        let thinking = try #require(document["thinking"] as? [String: Any])
        #expect((thinking["default_line"] as? String)?.contains("{size}") == true)
        for key in ["size_unknown", "later", "change", "continue", "choose_later"] {
            #expect(thinking[key] is String)
        }
        let choices = try #require(thinking["choices"] as? [[String: Any]])
        #expect(choices.compactMap { $0["id"] as? String } == ["local", "spp", "byo"])
        let downloads = try #require(document["downloads"] as? [String: Any])
        for key in ["speech", "thinking", "progress", "cancel", "retry", "error", "idle", "complete", "cancelled"] {
            #expect(downloads[key] is String)
        }
    }

    @Test func successRecordsSetupCopyInvocation() async throws {
        let runtime = try makeRuntime()
        defer { try? FileManager.default.removeItem(at: runtime.layout.rootURL) }
        let subprocess = FakeSubprocessRunner()
        subprocess.enqueue("setup-copy", .success(stdout: try fixtureData()))
        let loader = JournalSetupCopyLoader(
            materializer: MockRuntimeMaterializer(result: .success(runtime)),
            subprocessRunner: subprocess
        )

        let copy = try await loader.load()

        let invocation = try #require(subprocess.invocations.first { $0.arguments.first == "journal" })
        #expect(invocation.arguments == ["journal", "setup-copy", "--json"])
        #expect(!invocation.arguments.contains("--journal"))
        #expect(invocation.timeout == JournalSetupCopyLoader.timeout)
        let path = try #require(invocation.environment?["PATH"])
        #expect(path.split(separator: ":", maxSplits: 1).first.map(String.init) == runtime.layout.binDir.path)
        #expect(copy.thinking.choices.map(\.id) == ["local", "spp", "byo"])
        #expect(copy.thinking.defaultLine.contains("{size}"))
    }

    @Test func nonZeroExitFails() async throws {
        let loader = try makeLoader(response: .success(exitCode: 1))
        await #expect(throws: JournalSetupCopyLoaderError.failed) {
            try await loader.load()
        }
    }

    @Test func crashFails() async throws {
        let loader = try makeLoader(response: .failure("setup-copy crashed"))
        await #expect(throws: JournalSetupCopyLoaderError.failed) {
            try await loader.load()
        }
    }

    @Test func overrunCancelsAndFails() async throws {
        let subprocess = FakeSubprocessRunner()
        let runtime = try makeRuntime()
        defer { try? FileManager.default.removeItem(at: runtime.layout.rootURL) }
        let oversized = Data(repeating: 0x61, count: JournalSetupCopyLoader.stdoutCap + 1)
        subprocess.enqueue("setup-copy", .success(stdout: oversized))
        let loader = JournalSetupCopyLoader(
            materializer: MockRuntimeMaterializer(result: .success(runtime)),
            subprocessRunner: subprocess
        )

        await #expect(throws: JournalSetupCopyLoaderError.failed) {
            try await loader.load()
        }
        #expect(subprocess.cancelCount >= 1)
    }

    @Test func missingFieldIsUnqualified() async throws {
        var object = try #require(JSONSerialization.jsonObject(with: fixtureData()) as? [String: Any])
        var thinking = try #require(object["thinking"] as? [String: Any])
        thinking.removeValue(forKey: "choose_later")
        object["thinking"] = thinking
        let data = try JSONSerialization.data(withJSONObject: object)
        let loader = try makeLoader(response: .success(stdout: data))

        await #expect(throws: JournalSetupCopyLoaderError.unqualified) {
            try await loader.load()
        }
    }

    @Test func cancelCallsCancelAllAndFails() async throws {
        let subprocess = FakeSubprocessRunner()
        let runtime = try makeRuntime()
        defer { try? FileManager.default.removeItem(at: runtime.layout.rootURL) }
        subprocess.enqueue("setup-copy", .success(stdout: try fixtureData(), delay: .seconds(30)))
        let loader = JournalSetupCopyLoader(
            materializer: MockRuntimeMaterializer(result: .success(runtime)),
            subprocessRunner: subprocess
        )
        let task = Task {
            try await loader.load()
        }
        task.cancel()

        await #expect(throws: CancellationError.self) {
            try await task.value
        }
        #expect(subprocess.cancelCount >= 1)
    }

    private func makeLoader(response: FakeSubprocessRunner.Response) throws -> JournalSetupCopyLoader {
        let runtime = try makeRuntime()
        let subprocess = FakeSubprocessRunner()
        subprocess.enqueue("setup-copy", response)
        return JournalSetupCopyLoader(
            materializer: MockRuntimeMaterializer(result: .success(runtime)),
            subprocessRunner: subprocess
        )
    }

    private func fixtureData() throws -> Data {
        let url = URL(filePath: #filePath)
            .deletingLastPathComponent()
            .appendingPathComponent("Fixtures/setup-copy.json")
        return try Data(contentsOf: url)
    }
}
