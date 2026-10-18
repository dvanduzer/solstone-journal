// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import JournalMarkKit
import JournalRuntime
import SolstoneCore
import Testing
@testable import journal

@MainActor
extension JournalFirstRunModelTests {
    @Test func unqualifiedLaunchStillOpensInit() async throws {
        let world = try makeSetupWorld(capability: false, configureRoot: true)
        defer { world.refresh.release() }
        try writeStep1(world.fixture.mailbox)

        await world.fixture.model.decideLaunchRoute()

        #expect(world.fixture.browser.opened == [JournalFirstRunModel.defaultInitPageURL])
        #expect(world.fixture.model.reopenCount == 0)
        #expect(world.fixture.openJournalCalls == 0)
        #expect(world.fixture.backupOpens == 0)
        #expect(world.loads.calls == 0)
        #expect(world.peer.resolveCount == 0)
        #expect(world.peer.launched.isEmpty)
        #expect(world.fixture.continuation.load() == nil)
    }

    @Test func qualifiedFreshLaunchShowsWelcomeThenFolderWithoutBrowser() async throws {
        let world = try makeSetupWorld()
        defer { world.refresh.release() }

        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.model.route == .ritual(.welcome))
        world.fixture.model.continueFromWelcome()

        #expect(world.fixture.model.route == .ritual(.folder))
        #expect(world.fixture.config.journalRoot == nil)
        #expect(world.peer.launched.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func loginFailureStaysOnScreenAndIsNotStored() async throws {
        let suite = "app.solstone.journal.first-run.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        let config = JournalAppConfig(defaults: defaults, loginItemManager: ThrowingLoginItems())
        let world = try makeSetupWorld(config: config)
        defer { world.refresh.release() }

        await world.fixture.model.decideLaunchRoute()
        world.fixture.model.setWelcomeLaunchAtLogin(true)

        #expect(world.fixture.model.route == .ritual(.welcome))
        #expect(world.fixture.model.loginFailure?.isEmpty == false)
        #expect(defaults.object(forKey: "launchAtLoginEnabled") == nil)
        #expect(world.fixture.model.launchAtLoginEnabled)
        expectQuietLaunch(world)
    }

    @Test func folderContinueIsTheOnlyRootWriteAndSkipsModelInstall() async throws {
        let world = try makeSetupWorld()
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        world.fixture.model.continueFromWelcome()
        #expect(world.fixture.config.journalRoot == nil)
        world.fixture.model.journalRoot = world.root

        await world.fixture.model.continueFromFolder()

        #expect(world.fixture.config.journalRoot?.standardizedFileURL.path == world.root.standardizedFileURL.path)
        #expect(world.fixture.setupRunner.skipModelInstallFlags == [true])
        #expect(world.fixture.setupRunner.calls == 1)
        #expect(world.peer.launched.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func malformedHandoffStoresNoRoot() async throws {
        let world = try makeSetupWorld()
        defer { world.refresh.release() }
        try Data("not-json".utf8).write(
            to: world.fixture.mailbox.directory.appendingPathComponent("journal-setup-handoff.json")
        )

        await world.fixture.model.decideLaunchRoute()

        #expect(world.fixture.model.route == .ritual(.welcome))
        #expect(world.fixture.config.journalRoot == nil)
        #expect(world.fixture.continuation.load() == nil)
        expectQuietLaunch(world)
    }

    @Test func existingRootIgnoresHandoffRootHint() async throws {
        let world = try makeSetupWorld(configureRoot: true)
        defer { world.refresh.release() }
        let hinted = world.root.deletingLastPathComponent().appendingPathComponent("hinted", isDirectory: true)
        try writeStep1(world.fixture.mailbox, rootHint: hinted)

        await world.fixture.model.decideLaunchRoute()

        #expect(world.fixture.config.journalRoot?.standardizedFileURL.path == world.root.standardizedFileURL.path)
        #expect(world.fixture.model.journalRoot.standardizedFileURL.path == world.root.standardizedFileURL.path)
        #expect(world.fixture.continuation.load(matching: world.root) != nil)
        #expect(world.fixture.continuation.load(matching: hinted) == nil)
        #expect(world.peer.launched.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func completedOwnerWithoutIntentOrStep1LandsHome() async throws {
        let world = try makeSetupWorld(configureRoot: true, done: true)
        defer { world.refresh.release() }

        await world.fixture.model.decideLaunchRoute()

        #expect(world.fixture.model.route == .home)
        #expect(world.fixture.continuation.load() == nil)
        #expect(world.peer.launched.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func validStep1OnCompletedJournalLaunchesVerifiedPeer() async throws {
        let world = try makeSetupWorld(configureRoot: true, done: true)
        defer { world.refresh.release() }
        try writeStep1(world.fixture.mailbox)

        await world.fixture.model.decideLaunchRoute()

        guard case .ready(let handoff) = world.fixture.mailbox.loadStep5() else {
            Issue.record("expected step 5")
            return
        }
        #expect(handoff.stepIndex == 5)
        #expect(handoff.journalRootPath == world.root.standardizedFileURL.path)
        #expect(handoff.expectedInstanceID == setupInstanceID)
        #expect(handoff.expectedCAFingerprint == setupFingerprint)
        #expect(world.peer.launched == [setupPeerURL])
        #expect(world.peer.resolveCount == 3)
        #expect(world.fixture.model.route != .ritual(.mark))
        expectQuietLaunch(world)
    }

    @Test func replacedStep1LeavesTheNewerHandoff() async throws {
        let world = try makeSetupWorld(configureRoot: true)
        defer { world.refresh.release() }
        let first = UUID()
        let second = UUID()
        try writeStep1(world.fixture.mailbox, id: first)
        world.fixture.model.onBeforeStep1Commit = {
            try? writeStep1(world.fixture.mailbox, id: second)
        }

        await world.fixture.model.decideLaunchRoute()

        guard case .ready(let handoff) = world.fixture.mailbox.loadStep1() else {
            Issue.record("expected the replacement handoff")
            return
        }
        #expect(handoff.handoffID == second)
        #expect(try world.fixture.mailbox.readStep1Receipt() == nil)
        #expect(world.fixture.model.handoffError?.isEmpty == false)
        #expect(world.fixture.model.route == .ritual(.thinking))
        #expect(world.peer.launched.isEmpty)
        #expect(world.client.actions.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func continuationInProgressDoesNotRewind() async throws {
        let world = try makeSetupWorld(configureRoot: true)
        defer { world.refresh.release() }
        let pending = UUID()
        try world.fixture.continuation.save(root: world.root, pending: pending)
        try writeAcceptedReceipt(world.fixture.mailbox, id: UUID(), step: 1)
        try writeStep1(world.fixture.mailbox, id: UUID())
        let before = continuationBytes(world.fixture.continuation)

        await world.fixture.model.decideLaunchRoute()

        #expect(continuationBytes(world.fixture.continuation) == before)
        #expect(world.fixture.config.journalRoot?.standardizedFileURL.path == world.root.standardizedFileURL.path)
        #expect(world.fixture.model.journalRoot.standardizedFileURL.path == world.root.standardizedFileURL.path)
        #expect(world.fixture.model.route == .ritual(.thinking))
        #expect(world.fixture.model.handoffError?.isEmpty == false)
        #expect(world.peer.launched.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func onlyAcceptedAndRepublishedAdvance() async throws {
        let fresh = try makeSetupWorld()
        defer { fresh.refresh.release() }
        try writeStep1(fresh.fixture.mailbox)
        await fresh.fixture.model.decideLaunchRoute()
        #expect(fresh.fixture.model.route == .ritual(.welcome))
        #expect(fresh.fixture.config.journalRoot == nil)
        #expect(fresh.fixture.continuation.load() == nil)
        expectQuietLaunch(fresh)

        let republished = try makeSetupWorld(configureRoot: true)
        defer { republished.refresh.release() }
        let id = UUID()
        let pending = UUID()
        try republished.fixture.continuation.save(root: republished.root, pending: pending)
        try writeStep1(republished.fixture.mailbox, id: id)
        try writeAcceptedReceipt(republished.fixture.mailbox, id: id, step: 1)
        let before = continuationBytes(republished.fixture.continuation)
        await republished.fixture.model.decideLaunchRoute()
        #expect(continuationBytes(republished.fixture.continuation) == before)
        #expect(republished.fixture.model.route == .ritual(.mark))
        #expect(republished.fixture.model.handoffError == nil)
        #expect(republished.peer.launched.isEmpty)
        expectQuietLaunch(republished)
    }

    @Test func intentIsSavedBeforeFinalizeAndFailedWriteSkipsPost() async throws {
        let world = try makeSetupWorld(configureRoot: true, markState: "locked", saved: true, choice: "local")
        defer { world.refresh.release() }
        let seen = Flag()
        let store = world.fixture.continuation
        let root = world.root
        world.client.setHook { action in
            if case .finalize = action {
                seen.set(store.load(matching: root) != nil)
            }
        }
        await world.fixture.model.decideLaunchRoute()
        let done = setupState(root: world.root, done: true, markState: "locked", saved: true, choice: "local")
        world.client.setGets([done])
        world.client.setPerforms([done])

        await world.fixture.model.finalizeThinking()

        #expect(seen.value)
        #expect(world.client.actions == [.finalize(timezone: TimeZone.autoupdatingCurrent.identifier)])
        #expect(world.fixture.model.canFinalizeThinking)
        #expect(world.peer.launched == [setupPeerURL])
        guard case .ready = world.fixture.mailbox.loadStep5() else {
            Issue.record("expected step 5 after finalize")
            return
        }
        expectQuietLaunch(world)

        let failing = try makeSetupWorld(
            configureRoot: true,
            markState: "locked",
            saved: true,
            choice: "local",
            continuationReplace: { _, _ in throw CocoaError(.fileWriteUnknown) }
        )
        defer { failing.refresh.release() }
        await failing.fixture.model.decideLaunchRoute()
        #expect(failing.fixture.model.canFinalizeThinking)
        await failing.fixture.model.finalizeThinking()
        #expect(failing.client.actions.isEmpty)
        #expect(failing.peer.launched.isEmpty)
        #expect(failing.fixture.model.thinkingFailure?.isEmpty == false)
        expectQuietLaunch(failing)
    }

    @Test func pendingStep5IdentifierIsReused() async throws {
        let world = try makeSetupWorld(configureRoot: true, done: true)
        defer { world.refresh.release() }
        let pending = UUID()
        try world.fixture.continuation.save(root: world.root, pending: pending)

        await world.fixture.model.decideLaunchRoute()

        guard case .ready(let handoff) = world.fixture.mailbox.loadStep5() else {
            Issue.record("expected step 5")
            return
        }
        #expect(handoff.handoffID == pending)
        #expect(world.peer.launched == [setupPeerURL])
        expectQuietLaunch(world)
    }

    @Test func matchingReceiptRemovesIntentAndReopenDoesNotLaunch() async throws {
        let world = try makeSetupWorld(configureRoot: true, done: true)
        defer { world.refresh.release() }
        let pending = UUID()
        try world.fixture.continuation.save(root: world.root, pending: pending)
        try writeAcceptedReceipt(world.fixture.mailbox, id: pending, step: 5)

        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.continuation.load() == nil)
        #expect(world.fixture.model.route == .home)
        #expect(world.peer.launched.isEmpty)
        #expect(await supervisorCount(world.trace) == 0)

        await world.fixture.model.decideLaunchRoute()
        #expect(world.peer.launched.isEmpty)
        #expect(world.fixture.model.route == .home)
        expectQuietLaunch(world)
    }

    @Test func retryBeforeReceiptDoesNotShowMarkOrRestartLiveSupervisor() async throws {
        let world = try makeSetupWorld(configureRoot: true, done: true, startResults: [false])
        defer { world.refresh.release() }
        try world.fixture.continuation.save(root: world.root, pending: UUID())

        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.model.route == .ritual(.thinking))
        #expect(world.fixture.model.handoffError?.isEmpty == false)
        #expect(world.client.actions.isEmpty)
        #expect(await supervisorCount(world.trace) == 1)

        world.live.value = true
        await world.fixture.model.retryHandoff()

        #expect(world.fixture.model.route != .ritual(.mark))
        #expect(world.peer.launched == [setupPeerURL])
        #expect(await supervisorCount(world.trace) == 1)
        #expect(!world.client.actions.contains { action in
            if case .regenerateMark = action { return true }
            if case .commitMark = action { return true }
            return false
        })
        expectQuietLaunch(world)
    }

    @Test func lostFinalizeDeliversStep5OnNextDecide() async throws {
        let world = try makeSetupWorld(configureRoot: true, done: true)
        defer { world.refresh.release() }
        try world.fixture.continuation.save(root: world.root, pending: nil)
        #expect(world.fixture.mailbox.loadStep5() == .missing)

        await world.fixture.model.decideLaunchRoute()

        guard case .ready = world.fixture.mailbox.loadStep5() else {
            Issue.record("expected step 5")
            return
        }
        #expect(world.peer.launched == [setupPeerURL])
        #expect(world.fixture.model.route != .ritual(.mark))
        expectQuietLaunch(world)
    }

    @Test func savedIntentWithIncompleteSetupDoesNotLaunch() async throws {
        let world = try makeSetupWorld(configureRoot: true, done: false)
        defer { world.refresh.release() }
        try world.fixture.continuation.save(root: world.root, pending: UUID())

        await world.fixture.model.decideLaunchRoute()

        #expect(world.fixture.model.route == .ritual(.mark))
        #expect(world.peer.launched.isEmpty)
        #expect(world.fixture.mailbox.loadStep5() == .missing)
        expectQuietLaunch(world)
    }

    @Test func peerURLReplacedBeforeLaunchDoesNotLaunch() async throws {
        let peer = ScriptedPeer()
        peer.repeatLast = false
        peer.queue = [
            URL(fileURLWithPath: "/Applications/Qualification.app"),
            URL(fileURLWithPath: "/Applications/Selection.app"),
            URL(fileURLWithPath: "/Applications/Launch.app"),
        ]
        let world = try makeSetupWorld(configureRoot: true, done: true, peer: peer)
        defer { world.refresh.release() }
        try writeStep1(world.fixture.mailbox)

        await world.fixture.model.decideLaunchRoute()

        #expect(world.peer.launched.isEmpty)
        #expect(world.peer.resolveCount == 3)
        #expect(world.fixture.continuation.load(matching: world.root) != nil)
        #expect(world.fixture.model.handoffError?.isEmpty == false)
        #expect(world.fixture.browser.opened.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func absentUnverifiedAndIncapablePeersUseTheLegacyRoute() async throws {
        let incapable = try makeSetupWorld(capability: false, configureRoot: true)
        defer { incapable.refresh.release() }
        try writeStep1(incapable.fixture.mailbox)
        await incapable.fixture.model.decideLaunchRoute()
        #expect(incapable.fixture.browser.opened == [JournalFirstRunModel.defaultInitPageURL])
        #expect(incapable.loads.calls == 0)
        #expect(incapable.peer.resolveCount == 0)

        let missingCopy = try makeSetupWorld(configureRoot: true, provideCopy: false)
        defer { missingCopy.refresh.release() }
        await missingCopy.fixture.model.decideLaunchRoute()
        #expect(missingCopy.loads.calls == 1)
        #expect(missingCopy.peer.resolveCount == 0)
        #expect(missingCopy.fixture.browser.opened == [JournalFirstRunModel.defaultInitPageURL])

        let absent = ScriptedPeer()
        absent.repeatLast = false
        absent.queue = [nil]
        let missingPeer = try makeSetupWorld(configureRoot: true, peer: absent)
        defer { missingPeer.refresh.release() }
        await missingPeer.fixture.model.decideLaunchRoute()
        #expect(missingPeer.peer.resolveCount == 1)
        #expect(missingPeer.peer.verifyCount == 0)
        #expect(missingPeer.fixture.browser.opened == [JournalFirstRunModel.defaultInitPageURL])

        let unverified = ScriptedPeer()
        unverified.queue = [setupPeerURL]
        unverified.verifySticky = false
        let rejectedPeer = try makeSetupWorld(configureRoot: true, peer: unverified)
        defer { rejectedPeer.refresh.release() }
        await rejectedPeer.fixture.model.decideLaunchRoute()
        #expect(rejectedPeer.peer.verifyCount == 1)
        #expect(rejectedPeer.fixture.browser.opened == [JournalFirstRunModel.defaultInitPageURL])
        #expect(rejectedPeer.peer.launched.isEmpty)
        #expect(rejectedPeer.fixture.continuation.load() == nil)
        #expect(incapable.fixture.openJournalCalls == 0)
        #expect(incapable.fixture.backupOpens == 0)
        #expect(incapable.fixture.model.reopenCount == 0)
        #expect(missingCopy.fixture.openJournalCalls == 0)
        #expect(missingPeer.fixture.openJournalCalls == 0)
        #expect(rejectedPeer.fixture.openJournalCalls == 0)
    }

    @Test func lateResponseDoesNotChangeRootStepOrProposal() async throws {
        let world = try makeSetupWorld(configureRoot: true)
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        let gate = PerformGate()
        world.client.gate = gate
        world.client.setHold(true)
        let task = Task { await world.fixture.model.commitDisplayedMark() }
        await gate.waitUntilEntered()
        let root = world.fixture.model.journalRoot
        let route = world.fixture.model.route
        let proposal = world.fixture.model.displayedProposalID
        world.fixture.model.advanceSetupRevision()
        world.client.setPerforms([
            setupState(root: world.root.deletingLastPathComponent(), proposalID: "proposal-2")
        ])
        gate.release()
        await task.value

        #expect(world.fixture.model.journalRoot == root)
        #expect(world.fixture.model.route == route)
        #expect(world.fixture.model.displayedProposalID == proposal)
        #expect(world.client.actions == [.commitMark(proposalID: "proposal-1")])
        expectQuietLaunch(world)
    }

    @Test func proposalConflictWaitsForStateAndDoesNotCommitTheRefresh() async throws {
        let world = try makeSetupWorld(configureRoot: true)
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        world.client.setGets([setupState(root: world.root, proposalID: "proposal-2")])
        world.client.setError(.server(.init(
            error: "conflict",
            reasonCode: "proposal_conflict",
            detail: "stale"
        )))

        await world.fixture.model.commitDisplayedMark()

        #expect(world.fixture.model.displayedProposalID == "proposal-2")
        #expect(world.fixture.model.route == .ritual(.mark))
        #expect(world.fixture.model.handoffError == nil)
        #expect(world.fixture.model.markFailure == nil)
        #expect(world.client.actions == [.commitMark(proposalID: "proposal-1")])
        expectQuietLaunch(world)
    }

    @Test func lockedMarkHasNoRegenerate() async throws {
        let world = try makeSetupWorld(configureRoot: true, markState: "locked", saved: false)
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()

        #expect(world.fixture.model.route == .ritual(.thinking))
        #expect(!world.fixture.model.showsMarkRegenerate)
        await world.fixture.model.regenerateDisplayedMark()
        #expect(world.client.actions.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func doubleContinueCannotCommitASecondIdentifier() async throws {
        let world = try makeSetupWorld(configureRoot: true)
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        let gate = PerformGate()
        world.client.gate = gate
        world.client.setHold(true)
        let task = Task { await world.fixture.model.commitDisplayedMark() }
        await gate.waitUntilEntered()
        let revision = world.fixture.model.revision

        await world.fixture.model.commitDisplayedMark()

        #expect(world.fixture.model.revision == revision)
        #expect(world.client.actions.count == 1)
        world.client.setHold(false)
        gate.release()
        await task.value
        #expect(world.client.actions == [.commitMark(proposalID: "proposal-1")])
        expectQuietLaunch(world)
    }

    @Test func localIsNotOfferedReadyUnlessCapabilityIsOk() async throws {
        for capability in ["blocked", "unknown"] {
            let world = try makeSetupWorld(
                configureRoot: true,
                markState: "locked",
                saved: false,
                capabilityName: capability
            )
            defer { world.refresh.release() }
            await world.fixture.model.decideLaunchRoute()
            #expect(!world.fixture.model.offersLocalReady)
            await world.fixture.model.saveThinking("local")
            #expect(world.client.actions.isEmpty)
            expectQuietLaunch(world)
        }
    }

    @Test func saveThinkingLocalIsTheLocalSave() async throws {
        let world = try makeSetupWorld(configureRoot: true, markState: "locked", saved: false, capabilityName: "ok")
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.model.offersLocalReady)
        world.client.setPerforms([
            setupState(root: world.root, markState: "locked", saved: true, choice: "local")
        ])

        await world.fixture.model.saveThinking("local")

        #expect(world.client.actions == [.saveThinking(choice: "local")])
        #expect(world.peer.launched.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func finalizeSendsTimezoneWithoutWaitingForDownloads() async throws {
        let world = try makeSetupWorld(
            configureRoot: true,
            markState: "locked",
            saved: true,
            choice: "later",
            speech: "idle",
            thinkingDownload: "idle"
        )
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.model.canFinalizeThinking)
        #expect(world.fixture.model.setupState?.downloads.speech.state == "idle")
        let done = setupState(
            root: world.root,
            done: true,
            markState: "locked",
            saved: true,
            choice: "later",
            speech: "idle",
            thinkingDownload: "idle"
        )
        world.client.setGets([done])
        world.client.setPerforms([done])

        await world.fixture.model.finalizeThinking()

        #expect(world.client.actions == [.finalize(timezone: TimeZone.autoupdatingCurrent.identifier)])
        #expect(world.peer.launched == [setupPeerURL])
        expectQuietLaunch(world)
    }

    @Test func speechAdmissionPendingRefreshesWithoutFailing() async throws {
        let world = try makeSetupWorld(configureRoot: true, speech: "error")
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.model.showsSpeechRetry)
        world.client.setGets([setupState(root: world.root, speech: "idle")])
        world.client.setError(.server(.init(
            error: "pending",
            reasonCode: "speech_admission_pending",
            detail: "wait"
        )))

        await world.fixture.model.retrySpeechDownload()

        #expect(world.fixture.model.handoffError == nil)
        #expect(world.fixture.model.markFailure == nil)
        #expect(world.fixture.model.errorMessage == nil)
        #expect(world.fixture.model.setupState?.downloads.speech.state == "idle")
        #expect(world.fixture.setupRunner.calls == 0)
        #expect(world.client.actions == [.retrySpeechDownload])
        expectQuietLaunch(world)
    }

    @Test func refreshDoesNotRetryIdleSpeech() async throws {
        let world = try makeSetupWorld(configureRoot: true, speech: "idle", supervisorLive: true)
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.model.isRefreshLoopRunning)
        let before = world.client.stateCalls
        world.refresh.release()
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        while world.client.stateCalls == before, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }

        #expect(world.client.stateCalls > before)
        #expect(world.client.actions.isEmpty)
        #expect(!world.fixture.model.showsSpeechRetry)
        await world.fixture.model.prepareForTermination()
        #expect(!world.fixture.model.isRefreshLoopRunning)
    }

    @Test func speechRetryIsNotOfferedForIdle() async throws {
        let world = try makeSetupWorld(configureRoot: true, speech: "idle")
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()

        #expect(!world.fixture.model.showsSpeechRetry)
        await world.fixture.model.retrySpeechDownload()
        #expect(world.client.actions.isEmpty)
        expectQuietLaunch(world)
    }

    @Test func cancelSendsTheDisplayedAttemptIdentifier() async throws {
        let world = try makeSetupWorld(
            configureRoot: true,
            thinkingDownload: "downloading",
            thinkingAttempt: "attempt-9"
        )
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()
        #expect(world.fixture.model.showsThinkingCancel)

        await world.fixture.model.cancelThinkingDownload()
        await world.fixture.model.cancelSpeechDownload()

        #expect(world.client.actions == [.cancelThinkingDownload(attemptID: "attempt-9")])
        expectQuietLaunch(world)
    }

    @Test func windowFreeTerminationCancelsRefreshAndClosingDoesNot() async throws {
        let world = try makeSetupWorld(configureRoot: true, supervisorLive: true, attachWindow: false)
        defer { world.refresh.release() }
        await world.fixture.model.decideLaunchRoute()

        #expect(world.fixture.model.isRefreshLoopRunning)
        await world.fixture.model.prepareForTermination()
        #expect(!world.fixture.model.isRefreshLoopRunning)
    }
}

@MainActor
private struct SetupWorld {
    var fixture: FirstRunModelFixture
    var peer: ScriptedPeer
    var client: ScriptedSetupClient
    var refresh: RefreshGate
    var root: URL
    var loads: CopyLoads
    var live: LiveSwitch
    var trace: FirstRunTrace
}

@MainActor
private func makeSetupWorld(
    capability: Bool = true,
    configureRoot: Bool = false,
    provideCopy: Bool = true,
    done: Bool = false,
    markState: String = "proposal",
    saved: Bool = false,
    choice: String? = nil,
    capabilityName: String = "ok",
    speech: String = "idle",
    thinkingDownload: String = "idle",
    thinkingAttempt: String? = nil,
    proposalID: String = "proposal-1",
    supervisorLive: Bool = false,
    startResults: [Bool] = [],
    attachWindow: Bool = true,
    peer suppliedPeer: ScriptedPeer? = nil,
    config suppliedConfig: JournalAppConfig? = nil,
    continuationReplace: (@Sendable (Data, URL) throws -> Void)? = nil
) throws -> SetupWorld {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("journal-setup-root-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    let config = suppliedConfig ?? makeConfig()
    if configureRoot {
        config.journalRoot = root
    }
    let copy = try loadSetupFixtureCopy()
    let loads = CopyLoads()
    let peer = suppliedPeer ?? {
        let created = ScriptedPeer()
        created.queue = [setupPeerURL]
        return created
    }()
    let client = ScriptedSetupClient()
    client.setNetwork(JournalNetworkStatus(caFingerprint: setupFingerprint, instanceID: setupInstanceID))
    client.setGets([
        setupState(
            root: root,
            done: done,
            markState: markState,
            proposalID: proposalID,
            saved: saved,
            choice: choice,
            capability: capabilityName,
            speech: speech,
            thinkingDownload: thinkingDownload,
            thinkingAttempt: thinkingAttempt
        )
    ])
    let identity = ScriptedIdentity()
    identity.resolution = .mark(.uiTestSample, instanceID: setupInstanceID)
    let refresh = RefreshGate()
    let live = LiveSwitch()
    live.value = supervisorLive
    let trace = FirstRunTrace()
    var continuation: JournalSetupContinuationStore?
    if let continuationReplace {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("journal-setup-continuation-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        continuation = JournalSetupContinuationStore(directory: directory, replace: continuationReplace)
    }
    let fixture = makeModel(
        trace: trace,
        config: config,
        startResults: startResults,
        probeResults: [.incomplete, .complete],
        localSetupCapability: { capability },
        loadSetupCopy: {
            loads.bump()
            return provideCopy ? copy : nil
        },
        peer: peer,
        setupClient: client,
        identity: identity,
        continuation: continuation,
        supervisorIsLive: { _ in live.value },
        externalSupervisorIsLive: { _ in false },
        refreshSleep: { await refresh.sleep() },
        attachWindow: attachWindow
    )
    return SetupWorld(
        fixture: fixture,
        peer: peer,
        client: client,
        refresh: refresh,
        root: root,
        loads: loads,
        live: live,
        trace: trace
    )
}

private func setupState(
    root: URL,
    done: Bool = false,
    markState: String = "proposal",
    proposalID: String? = "proposal-1",
    saved: Bool = false,
    choice: String? = nil,
    capability: String = "ok",
    speech: String = "idle",
    thinkingDownload: String = "idle",
    thinkingAttempt: String? = nil
) -> JournalSetupState {
    JournalSetupState(
        journalPath: root.standardizedFileURL.path,
        done: done,
        mark: .init(render: .uiTestSample, state: markState, proposalID: proposalID),
        thinking: .init(choice: choice, saved: saved, capability: capability, downloadBytes: nil),
        downloads: .init(
            speech: .init(state: speech, attemptID: nil, bytesDone: 0, bytesTotal: nil),
            thinking: .init(state: thinkingDownload, attemptID: thinkingAttempt, bytesDone: 0, bytesTotal: nil)
        )
    )
}

private func loadSetupFixtureCopy() throws -> JournalSetupCopy {
    let url = URL(filePath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .appendingPathComponent("JournalRuntimeTests/Fixtures/setup-copy.json")
    return try JournalSetupCopy.decodeQualified(Data(contentsOf: url))
}

private func writeStep1(_ mailbox: SetupMailboxStore, id: UUID = UUID(), rootHint: URL? = nil) throws {
    if let rootHint {
        let handoff = SetupHandoff(handoffID: id, stepIndex: 1, journalRootPath: rootHint.path)
        let data = try JSONEncoder().encode(handoff)
        try data.write(to: mailbox.directory.appendingPathComponent("journal-setup-handoff.json"))
        return
    }
    try mailbox.writeStep1(handoffID: id)
}

private func writeAcceptedReceipt(_ mailbox: SetupMailboxStore, id: UUID, step: Int) throws {
    let data = try JSONEncoder().encode(SetupHandoffReceipt(handoffID: id, stepIndex: step))
    let name = step == 5 ? "solstone-setup-handoff-accepted.json" : "journal-setup-handoff-accepted.json"
    try data.write(to: mailbox.directory.appendingPathComponent(name))
}

private func continuationBytes(_ store: JournalSetupContinuationStore) -> Data? {
    let url = store.directory.appendingPathComponent(JournalSetupContinuationStore.fileName)
    return try? Data(contentsOf: url)
}

private func supervisorCount(_ trace: FirstRunTrace) async -> Int {
    await trace.snapshot().filter { $0 == "supervisor" }.count
}

@MainActor
private func expectQuietLaunch(_ world: SetupWorld) {
    #expect(world.fixture.browser.opened.isEmpty)
    #expect(world.fixture.model.reopenCount == 0)
    #expect(world.fixture.openJournalCalls == 0)
    #expect(world.fixture.backupOpens == 0)
}

private let setupFingerprint = String(repeating: "0123456789abcdef", count: 4)
private let setupInstanceID = "instance-1"
private let setupPeerURL = URL(fileURLWithPath: "/Applications/Solstone.app")

@MainActor
private final class ScriptedPeer: JournalSetupPeerResolving {
    var queue: [URL?] = []
    var sticky: URL?
    var repeatLast = true
    var verifyQueue: [Bool] = []
    var verifySticky = true
    private(set) var launched: [URL] = []
    private(set) var resolveCount = 0
    private(set) var verifyCount = 0

    func resolve() -> URL? {
        resolveCount += 1
        if !queue.isEmpty {
            sticky = queue.removeFirst()
            return sticky
        }
        return repeatLast ? sticky : nil
    }

    func verify(_ url: URL) async -> Bool {
        _ = url
        verifyCount += 1
        if !verifyQueue.isEmpty {
            return verifyQueue.removeFirst()
        }
        return verifySticky
    }

    func launch(_ url: URL) async throws {
        launched.append(url)
    }
}

private final class ScriptedSetupClient: JournalSetupClienting, @unchecked Sendable {
    private let lock = NSLock()
    private var gets: [JournalSetupState] = []
    private var performs: [JournalSetupState] = []
    private var recorded: [JournalSetupAction] = []
    private var getsCount = 0
    private var thrown: JournalSetupClientError?
    private var hold = false
    private var network = JournalNetworkStatus(caFingerprint: nil, instanceID: nil)
    private var hook: (@Sendable (JournalSetupAction) -> Void)?
    var gate: PerformGate?

    var actions: [JournalSetupAction] { lock.withLock { recorded } }
    var stateCalls: Int { lock.withLock { getsCount } }

    func setGets(_ values: [JournalSetupState]) { lock.withLock { gets = values } }
    func setPerforms(_ values: [JournalSetupState]) { lock.withLock { performs = values } }
    func setError(_ error: JournalSetupClientError?) { lock.withLock { thrown = error } }
    func setHold(_ value: Bool) { lock.withLock { hold = value } }
    func setNetwork(_ value: JournalNetworkStatus) { lock.withLock { network = value } }
    func setHook(_ hook: (@Sendable (JournalSetupAction) -> Void)?) { lock.withLock { self.hook = hook } }

    func state() async throws -> JournalSetupState {
        let value: JournalSetupState? = lock.withLock {
            getsCount += 1
            guard !gets.isEmpty else { return nil }
            if gets.count == 1 { return gets[0] }
            return gets.removeFirst()
        }
        guard let value else { throw JournalSetupClientError.decoding }
        return value
    }

    func perform(_ action: JournalSetupAction) async throws -> JournalSetupState {
        let callback: (@Sendable (JournalSetupAction) -> Void)? = lock.withLock { hook }
        callback?(action)
        let shouldHold: Bool = lock.withLock {
            recorded.append(action)
            return hold
        }
        if shouldHold {
            await gate?.enterAndWait()
        }
        let error: JournalSetupClientError? = lock.withLock {
            let value = thrown
            thrown = nil
            return value
        }
        if let error { throw error }
        let value: JournalSetupState? = lock.withLock {
            if !performs.isEmpty {
                if performs.count == 1 { return performs[0] }
                return performs.removeFirst()
            }
            return gets.last
        }
        guard let value else { throw JournalSetupClientError.decoding }
        return value
    }

    func networkStatus() async throws -> JournalNetworkStatus {
        lock.withLock { network }
    }
}

private final class ScriptedIdentity: JournalIdentityResolving, @unchecked Sendable {
    private let lock = NSLock()
    private var stored: JournalIdentityResolution = .absent

    var resolution: JournalIdentityResolution {
        get { lock.withLock { stored } }
        set { lock.withLock { stored = newValue } }
    }

    func resolveIdentity(baseURL: String) async -> JournalIdentityResolution {
        _ = baseURL
        return resolution
    }
}

private final class RefreshGate: @unchecked Sendable {
    private let lock = NSLock()
    private var open = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func sleep() async {
        await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
            lock.lock()
            if open {
                open = false
                lock.unlock()
                cont.resume()
                return
            }
            waiters.append(cont)
            lock.unlock()
        }
    }

    func release() {
        let waiter: CheckedContinuation<Void, Never>? = lock.withLock {
            if waiters.isEmpty {
                open = true
                return nil
            }
            return waiters.removeFirst()
        }
        waiter?.resume()
    }
}

private final class PerformGate: @unchecked Sendable {
    private let lock = NSLock()
    private var open = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private var entered = 0
    private var enteredWaiters: [CheckedContinuation<Void, Never>] = []

    func enterAndWait() async {
        await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
            lock.lock()
            entered += 1
            let notify = enteredWaiters
            enteredWaiters.removeAll()
            if open {
                open = false
                lock.unlock()
                notify.forEach { $0.resume() }
                cont.resume()
                return
            }
            waiters.append(cont)
            lock.unlock()
            notify.forEach { $0.resume() }
        }
    }

    func waitUntilEntered() async {
        await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
            lock.lock()
            if entered > 0 {
                lock.unlock()
                cont.resume()
                return
            }
            enteredWaiters.append(cont)
            lock.unlock()
        }
    }

    func release() {
        let waiter: CheckedContinuation<Void, Never>? = lock.withLock {
            if waiters.isEmpty {
                open = true
                return nil
            }
            return waiters.removeFirst()
        }
        waiter?.resume()
    }
}

private final class CopyLoads: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    var calls: Int { lock.withLock { count } }
    func bump() { lock.withLock { count += 1 } }
}

private final class LiveSwitch: @unchecked Sendable {
    var value = false
}

private final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var stored = false
    var value: Bool { lock.withLock { stored } }
    func set(_ value: Bool) { lock.withLock { stored = value } }
}

@MainActor
private final class ThrowingLoginItems: LoginItemManaging {
    func register() throws { throw CocoaError(.fileWriteUnknown) }
    func unregister() throws { throw CocoaError(.fileWriteUnknown) }
}
