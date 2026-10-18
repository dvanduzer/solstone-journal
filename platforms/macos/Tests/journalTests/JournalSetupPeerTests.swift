// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import JournalRuntime
import JournalRuntimeTestSupport
import Testing
@testable import journal

@MainActor
@Suite("JournalSetupPeer")
struct JournalSetupPeerTests {
    private let requirement =
        "=anchor apple generic and identifier \"app.solstone.observer\" and certificate leaf[subject.OU] = \"7QCG8V4M6H\""

    private let described = Data("""
    Identifier=app.solstone.observer
    TeamIdentifier=7QCG8V4M6H
    designated => identifier "app.solstone.observer" and certificate leaf[subject.OU] = "7QCG8V4M6H"
    """.utf8)

    @Test func oneCodesignEvaluationTrustsOnlyTheExitStatus() async {
        let url = URL(fileURLWithPath: "/Applications/Observer.app", isDirectory: true)
        let runner = FakeSubprocessRunner()
        runner.enqueue("codesign", .success(stdout: described, exitCode: 0))
        let seen = CapabilitySeen()
        let accepted = resolver(runner: runner) { candidate in
            seen.url = candidate
            seen.count += 1
            return ["setup-sequence-v1"]
        }

        #expect(await accepted.verify(url))
        #expect(seen.url == url)
        #expect(seen.count == 1)
        #expect(runner.invocations.count == 1)
        let invocation = runner.invocations[0]
        #expect(invocation.executable == URL(fileURLWithPath: "/usr/bin/codesign"))
        #expect(invocation.arguments == [
            "--verify", "--strict", "--deep", "--verbose=2",
            "-R", requirement, url.path,
        ])
    }

    @Test func exitSignalThrowAndTimeoutRefuseBeforeCapability() async {
        let url = URL(fileURLWithPath: "/Applications/Observer.app")

        let exit3 = FakeSubprocessRunner()
        exit3.enqueue("codesign", .success(stdout: described, exitCode: 3))
        let exitCalls = CapabilitySeen()
        let exitResolver = resolver(runner: exit3) { _ in
            exitCalls.count += 1
            return ["setup-sequence-v1"]
        }
        #expect(await exitResolver.verify(url) == false)
        #expect(exitCalls.count == 0)
        #expect(exit3.invocations.count == 1)

        let signal = FakeSubprocessRunner()
        signal.enqueue("codesign", .success(stdout: described, delay: .milliseconds(5)))
        let signalCalls = CapabilitySeen()
        let signalResolver = resolver(runner: signal, timeout: .milliseconds(5)) { _ in
            signalCalls.count += 1
            return ["setup-sequence-v1"]
        }
        #expect(await signalResolver.verify(url) == false)
        #expect(signalCalls.count == 0)
        #expect(signal.invocations.count == 1)

        let thrown = FakeSubprocessRunner()
        thrown.enqueue("codesign", .failure("codesign failed"))
        let thrownCalls = CapabilitySeen()
        let thrownResolver = resolver(runner: thrown) { _ in
            thrownCalls.count += 1
            return ["setup-sequence-v1"]
        }
        #expect(await thrownResolver.verify(url) == false)
        #expect(thrownCalls.count == 0)
        #expect(thrown.invocations.count == 1)

        let timedOut = FakeSubprocessRunner()
        timedOut.enqueue("codesign", .success(delay: .milliseconds(20)))
        let timeoutCalls = CapabilitySeen()
        let timeoutResolver = resolver(runner: timedOut, timeout: .milliseconds(20)) { _ in
            timeoutCalls.count += 1
            return ["setup-sequence-v1"]
        }
        #expect(await timeoutResolver.verify(url) == false)
        #expect(timeoutCalls.count == 0)
        #expect(timedOut.invocations.count == 1)
    }

    @Test func exitZeroWithTheWrongCapabilityRefusesAfterOneCommand() async {
        let url = URL(fileURLWithPath: "/Applications/Observer.app", isDirectory: true)
        let runner = FakeSubprocessRunner()
        runner.enqueue("codesign", .success(stdout: described, exitCode: 0))
        let seen = CapabilitySeen()
        let rejected = resolver(runner: runner) { candidate in
            seen.url = candidate
            seen.count += 1
            return ["other"]
        }

        #expect(await rejected.verify(url) == false)
        #expect(seen.url == url)
        #expect(seen.count == 1)
        #expect(runner.invocations.count == 1)
        #expect(runner.invocations[0].arguments == [
            "--verify", "--strict", "--deep", "--verbose=2",
            "-R", requirement, url.path,
        ])
    }

    @Test func truncatedOutputRefuses() async {
        let url = URL(fileURLWithPath: "/Applications/Observer.app")
        let runner = FakeSubprocessRunner()
        runner.enqueue("codesign", .success(stdout: Data(count: JournalSetupPeerResolver.outputCap + 1)))
        let seen = CapabilitySeen()
        let resolver = resolver(runner: runner) { _ in
            seen.count += 1
            return ["setup-sequence-v1"]
        }

        #expect(await resolver.verify(url) == false)
        #expect(seen.count == 0)
        #expect(runner.invocations.count == 1)
    }

    private func resolver(
        runner: FakeSubprocessRunner,
        timeout: Duration = .seconds(30),
        capabilities: @escaping (URL) -> Any? = { _ in ["setup-sequence-v1"] }
    ) -> JournalSetupPeerResolver {
        JournalSetupPeerResolver(
            runner: runner,
            lookup: { _ in URL(fileURLWithPath: "/Applications/Observer.app") },
            capabilities: capabilities,
            launchApplication: { _ in },
            commandTimeout: timeout
        )
    }

    private final class CapabilitySeen: @unchecked Sendable {
        var url: URL?
        var count = 0
    }
}
