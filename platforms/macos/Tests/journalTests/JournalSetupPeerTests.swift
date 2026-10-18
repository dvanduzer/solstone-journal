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
    @Test func nonzeroSignalAndThrowRefuseWithoutASecondCommand() async {
        let url = URL(fileURLWithPath: "/Applications/Observer.app")
        let nonzero = FakeSubprocessRunner()
        nonzero.enqueue("codesign", .success(exitCode: 1))
        let nonzeroResolver = resolver(runner: nonzero)
        #expect(await nonzeroResolver.verify(url) == false)
        #expect(nonzero.invocations.count == 1)

        let signal = FakeSubprocessRunner()
        signal.enqueue("codesign", .success(delay: .milliseconds(5)))
        let signalResolver = resolver(runner: signal, timeout: .milliseconds(5))
        #expect(await signalResolver.verify(url) == false)
        #expect(signal.invocations.count == 1)

        let thrown = FakeSubprocessRunner()
        thrown.enqueue("codesign", .failure("codesign failed"))
        let thrownResolver = resolver(runner: thrown)
        #expect(await thrownResolver.verify(url) == false)
        #expect(thrown.invocations.count == 1)
    }

    @Test func designatedRequirementTokensPassAndSubstringsDoNot() async {
        let url = URL(fileURLWithPath: "/Applications/Observer.app", isDirectory: true)
        let seen = CapabilitySeen()
        let passing = FakeSubprocessRunner()
        passing.enqueue("codesign", .success(stdout: Data("verified\n".utf8)))
        passing.enqueue("codesign", .success(stdout: Data(passingRequirement.utf8)))
        let accepted = resolver(runner: passing) { candidate in
            seen.url = candidate
            return ["setup-sequence-v1"]
        }
        #expect(await accepted.verify(url))
        #expect(seen.url == url)
        #expect(passing.invocations.count == 2)

        let otherCapability = FakeSubprocessRunner()
        otherCapability.enqueue("codesign", .success())
        otherCapability.enqueue("codesign", .success(stdout: Data(passingRequirement.utf8)))
        let rejectedCapability = resolver(runner: otherCapability) { _ in ["other"] }
        #expect(await rejectedCapability.verify(url) == false)

        let substrings = FakeSubprocessRunner()
        substrings.enqueue("codesign", .success())
        substrings.enqueue("codesign", .success(stdout: Data("""
        Identifier=app.solstone.observer
        TeamIdentifier=7QCG8V4M6H
        designated => identifier "app.solstone.other" and certificate leaf[subject.OU] = "FFFFFFFFFF"
        """.utf8)))
        let calls = CapabilitySeen()
        let rejected = resolver(runner: substrings) { _ in
            calls.count += 1
            return ["setup-sequence-v1"]
        }
        #expect(await rejected.verify(url) == false)
        #expect(calls.count == 0)
        #expect(substrings.invocations.count == 2)
    }

    @Test func truncatedOutputRefuses() async {
        let url = URL(fileURLWithPath: "/Applications/Observer.app")
        let runner = FakeSubprocessRunner()
        runner.enqueue("codesign", .success(stdout: Data(count: JournalSetupPeerResolver.outputCap + 1)))
        let resolver = resolver(runner: runner)
        #expect(await resolver.verify(url) == false)
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

    private var passingRequirement: String {
        """
        Identifier=app.solstone.other
        TeamIdentifier=FFFFFFFFFF
        designated => identifier "app.solstone.observer" and certificate leaf[subject.OU] = "7QCG8V4M6H"
        """
    }
}
