// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import AppKit
import Foundation
import JournalMarkKit
import JournalRuntime
import Observation
import os
import SolstoneCore

enum JournalFirstRunRoute: Equatable {
    case deciding
    case ritual(JournalFirstRunStep)
    case adopting
    case home
}

enum JournalFirstRunStep: Equatable {
    case welcome
    case folder
    case nameLocation
    case mark
    case thinking
    case setupProgress
    case finishInBrowser
}

protocol JournalSetupRunning: Sendable {
    func run(
        journalRoot: URL,
        skipService: Bool,
        skipModelInstall: Bool,
        progress: @escaping @Sendable (JournalSetupProgressEvent) async -> Void
    ) async throws -> JournalSetupResult
}

extension JournalSetupRunner: JournalSetupRunning {}

protocol JournalInitClienting: Sendable {
    func probeSetupComplete() async throws -> JournalInitSetupProbe
}

extension JournalInitClient: JournalInitClienting {}

@MainActor
@Observable
final class JournalFirstRunModel {
    typealias SupervisorStart = @MainActor @Sendable (URL) async -> Bool
    typealias URLOpener = @MainActor (URL) -> Bool

    static let defaultInitPageURL = URL(string: "http://127.0.0.1:5015/init")!
    static let setupBaseURL = "http://127.0.0.1:5015"

    @ObservationIgnored private let config: JournalAppConfig
    @ObservationIgnored private let setupRunner: any JournalSetupRunning
    @ObservationIgnored private let initClient: any JournalInitClienting
    @ObservationIgnored private let startSupervisor: SupervisorStart
    @ObservationIgnored private let handoffStore: any JournalHandoffStoring
    @ObservationIgnored private let journalFileReader: any OnDiskJournalFileReading
    @ObservationIgnored private let discoveryQualificationTimeout: TimeInterval
    @ObservationIgnored private let initPageURL: URL
    @ObservationIgnored private let openURL: URLOpener
    @ObservationIgnored private let browserPollInterval: Duration
    @ObservationIgnored private weak var windowModel: JournalWindowModel?
    @ObservationIgnored private let supervisorIsLive: @MainActor (URL) -> Bool
    @ObservationIgnored private let externalSupervisorIsLive: @Sendable (URL) -> Bool
    @ObservationIgnored private let loadSetupCopy: @Sendable () async -> JournalSetupCopy?
    @ObservationIgnored private let localSetupCapability: @Sendable () -> Bool
    @ObservationIgnored private let peer: any JournalSetupPeerResolving
    @ObservationIgnored private let setupClient: any JournalSetupClienting
    @ObservationIgnored private let identity: any JournalIdentityResolving
    @ObservationIgnored private let mailbox: SetupMailboxStore
    @ObservationIgnored private let continuation: JournalSetupContinuationStore
    @ObservationIgnored private let refreshSleep: @Sendable () async -> Void

    var route: JournalFirstRunRoute = .deciding
    var journalRoot: URL
    var setupEvents: [JournalSetupProgressEvent] = []
    var setupRenderedLog = ""
    var currentStep: String?
    var adoptMessage: String?
    var errorMessage: String?
    var handoffError: String?
    var loginFailure: String?
    var markFailure: String?
    var thinkingFailure: String?
    var nativeFolderSetup = false
    var choosingThinking = false
    private(set) var reopenCount = 0
    private(set) var setupCopy: JournalSetupCopy?
    private(set) var setupState: JournalSetupState?
    private(set) var displayedProposalID: String?
    private(set) var revision: UInt64 = 0

    @ObservationIgnored private var setupTask: Task<JournalSetupResult, Error>?
    @ObservationIgnored private var browserWaitTask: Task<Void, Never>?
    @ObservationIgnored private var browserWaitGeneration = 0
    @ObservationIgnored private var refreshTask: Task<Void, Never>?
    @ObservationIgnored private var actionInFlight = false
    @ObservationIgnored private var nativeQualified = false
    @ObservationIgnored private var didAutoRegenerate = false
    @ObservationIgnored var onBeforeStep1Commit: (@MainActor () -> Void)?
    private var isTerminating = false

    private var pendingDiscoveryHandoff: JournalHandoff?

    init(
        config: JournalAppConfig,
        setupRunner: any JournalSetupRunning = JournalSetupRunner(materializer: NativeJournalRuntimeMaterializer()),
        initClient: any JournalInitClienting = JournalInitClient(),
        startSupervisor: @escaping SupervisorStart,
        handoffStore: any JournalHandoffStoring = JournalHandoffStore(),
        journalFileReader: any OnDiskJournalFileReading = LiveOnDiskJournalFileReader(),
        discoveryQualificationTimeout: TimeInterval = 1.0,
        initPageURL: URL = JournalFirstRunModel.defaultInitPageURL,
        openURL: @escaping URLOpener = { NSWorkspace.shared.open($0) },
        browserPollInterval: Duration = .seconds(2),
        windowModel: JournalWindowModel? = nil,
        supervisorIsLive: @escaping @MainActor (URL) -> Bool = { _ in false },
        externalSupervisorIsLive: @escaping @Sendable (URL) -> Bool = {
            liveSupervisorMatchesReadyFiles(journalRoot: $0)
        },
        loadSetupCopy: @escaping @Sendable () async -> JournalSetupCopy? = {
            do {
                return try await JournalSetupCopyLoader().load()
            } catch {
                return nil
            }
        },
        localSetupCapability: @escaping @Sendable () -> Bool = {
            journalSupportsSetupSequence(Bundle.main.object(forInfoDictionaryKey: "SolstoneSetupCapabilities"))
        },
        peer: any JournalSetupPeerResolving = JournalSetupPeerResolver(),
        setupClient: any JournalSetupClienting = JournalSetupClient(),
        identity: any JournalIdentityResolving = JournalIdentityResolver(),
        mailbox: SetupMailboxStore = SetupMailboxStore(),
        continuation: JournalSetupContinuationStore = JournalSetupContinuationStore(),
        refreshSleep: @escaping @Sendable () async -> Void = {
            try? await Task.sleep(for: .seconds(1))
        }
    ) {
        self.config = config
        self.setupRunner = setupRunner
        self.initClient = initClient
        self.startSupervisor = startSupervisor
        self.handoffStore = handoffStore
        self.journalFileReader = journalFileReader
        self.discoveryQualificationTimeout = discoveryQualificationTimeout
        self.initPageURL = initPageURL
        self.openURL = openURL
        self.browserPollInterval = browserPollInterval
        self.windowModel = windowModel
        self.supervisorIsLive = supervisorIsLive
        self.externalSupervisorIsLive = externalSupervisorIsLive
        self.loadSetupCopy = loadSetupCopy
        self.localSetupCapability = localSetupCapability
        self.peer = peer
        self.setupClient = setupClient
        self.identity = identity
        self.mailbox = mailbox
        self.continuation = continuation
        self.refreshSleep = refreshSleep

        self.journalRoot = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("journal", isDirectory: true)
    }

    func decideLaunchRoute() async {
        pendingDiscoveryHandoff = nil
        didAutoRegenerate = false
        guard !isTerminating, !Task.isCancelled else { return }
        if await qualifyForNativeSetup() {
            nativeQualified = true
            await decideQualifiedLaunchRoute()
            return
        }
        nativeQualified = false
        setupCopy = nil
        guard !isTerminating, !Task.isCancelled else { return }
        await decideLegacyLaunchRoute()
    }

    private func decideLegacyLaunchRoute() async {
        do {
            guard let handoff = try handoffStore.load() else {
                await continueWithoutHandoff()
                return
            }

            switch handoff.provenance {
            case JournalHandoffProvenance.bundledMigration:
                route = .adopting
                await adoptFromHandoff()
            case JournalHandoffProvenance.observerDiscovery:
                await routeDiscoveryHandoff(handoff)
            default:
                consumeHandoffBestEffort()
                beginCreateAtDefaultLocation()
            }
        } catch {
            consumeHandoffBestEffort()
            beginCreateAtDefaultLocation()
        }
    }

    private func continueWithoutHandoff() async {
        guard let root = config.journalRoot else {
            beginCreate()
            return
        }

        await resumeConfiguredRoot(root)
    }

    private func routeDiscoveryHandoff(_ handoff: JournalHandoff) async {
        let root = URL(fileURLWithPath: handoff.journalRootPath, isDirectory: true)
            .standardizedFileURL
        let fileReader = journalFileReader
        let timeout = discoveryQualificationTimeout
        let qualifies: Bool
        do {
            qualifies = try await withTimeout(seconds: timeout) {
                await journalDirectoryQualifies(at: root.path, using: fileReader)
            }
        } catch {
            qualifies = false
        }

        guard qualifies else {
            consumeHandoffBestEffort()
            beginCreateAtDefaultLocation()
            return
        }

        journalRoot = root
        pendingDiscoveryHandoff = handoff
        errorMessage = nil
        adoptMessage = nil
        route = .ritual(.nameLocation)
    }

    private func consumeHandoffBestEffort() {
        try? handoffStore.consume()
    }

    private func beginCreateAtDefaultLocation() {
        errorMessage = nil
        adoptMessage = nil
        journalRoot = defaultJournalRoot
        config.journalRoot = nil
        route = .ritual(.nameLocation)
    }

    private var defaultJournalRoot: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("journal", isDirectory: true)
            .standardizedFileURL
    }

    func beginCreate() {
        errorMessage = nil
        adoptMessage = nil
        if let configuredRoot = config.journalRoot {
            journalRoot = configuredRoot.standardizedFileURL
        } else {
            journalRoot = defaultJournalRoot
        }
        route = .ritual(.nameLocation)
    }

    func continueFromNameLocation() async {
        if pendingDiscoveryHandoff != nil {
            consumeHandoffBestEffort()
            pendingDiscoveryHandoff = nil
        }
        config.journalRoot = journalRoot.standardizedFileURL
        if nativeQualified {
            revision &+= 1
            await runQualifiedSetup(skipModelInstall: false)
            return
        }
        await runSetupThenStartSupervisor()
    }

    func runSetupThenStartSupervisor() async {
        route = .ritual(.setupProgress)
        errorMessage = nil

        do {
            try await runSetup(at: journalRoot, skipModelInstall: false)
            guard await startSupervisor(journalRoot.standardizedFileURL) else {
                errorMessage = supervisorFailureMessage()
                return
            }
            await routeAfterProbe()
        } catch {
            errorMessage = readableMessage(for: error)
        }
    }

    func prepareForTermination() async {
        isTerminating = true
        stopRefresh()
        browserWaitTask?.cancel()
        browserWaitTask = nil
        let task = setupTask
        task?.cancel()
        _ = try? await task?.value
    }

    func resetAfterFailedUpdaterInstall() {
        isTerminating = false
        if route == .ritual(.finishInBrowser) {
            startBrowserWaitIfNeeded()
        }
    }

    private func runSetup(at journalRoot: URL, skipModelInstall: Bool) async throws {
        guard !isTerminating, setupTask == nil else { throw CancellationError() }
        setupEvents = []
        setupRenderedLog = ""
        currentStep = nil

        let task = Task { [setupRunner, weak self] in
            try await setupRunner.run(
                journalRoot: journalRoot,
                skipService: true,
                skipModelInstall: skipModelInstall,
                progress: { [weak self] event in
                    await MainActor.run { self?.applySetupEvent(event) }
                }
            )
        }
        setupTask = task
        defer { setupTask = nil }
        let result = try await withTaskCancellationHandler {
            try await task.value
        } onCancel: {
            task.cancel()
        }
        try Task.checkCancellation()
        guard !isTerminating, !task.isCancelled else { throw CancellationError() }
        setupRenderedLog = result.renderedLog
    }

    func resumeConfiguredRoot(_ root: URL) async {
        journalRoot = root.standardizedFileURL
        config.journalRoot = journalRoot
        errorMessage = nil

        guard await startSupervisor(journalRoot) else {
            await runSetupThenStartSupervisor()
            return
        }

        await routeAfterProbe()
    }

    func adoptFromHandoff() async {
        route = .adopting
        adoptMessage = JournalFirstRunCopy.adoptOpening
        errorMessage = nil

        do {
            guard let handoff = try handoffStore.load() else {
                beginCreate()
                return
            }
            let root = URL(fileURLWithPath: handoff.journalRootPath, isDirectory: true).standardizedFileURL
            journalRoot = root
            config.journalRoot = root

            try await runSetup(at: root, skipModelInstall: false)
            guard await startSupervisor(root) else {
                errorMessage = supervisorFailureMessage()
                return
            }

            try await completeAdoptAfterSuccessfulStart()
        } catch {
            errorMessage = readableMessage(for: error)
        }
    }

    private func completeAdoptAfterSuccessfulStart() async throws {
        let probe = try await initClient.probeSetupComplete()
        try handoffStore.consume()
        adoptMessage = JournalFirstRunCopy.adoptLandingLine
        switch probe {
        case .complete:
            await landHome()
        case .incomplete:
            await finishInBrowser()
        }
    }

    private func routeAfterProbe() async {
        let probe: JournalInitSetupProbe
        do {
            probe = try await initClient.probeSetupComplete()
        } catch {
            errorMessage = readableMessage(for: error)
            return
        }
        switch probe {
        case .complete:
            await landHome()
        case .incomplete:
            await finishInBrowser()
        }
    }

    /// The journal's own first-run page finishes creating the journal: it asks for the mark and
    /// the owner's choices, then records the first run as done. This app only opens it in the
    /// owner's browser and waits for the journal to report it done.
    private func finishInBrowser() async {
        errorMessage = nil
        route = .ritual(.finishInBrowser)
        openInitPage()
        await startBrowserWaitIfNeeded().value
    }

    /// Opens the page again, for an owner who closed it before finishing.
    func reopenInBrowser() {
        reopenCount += 1
        openInitPage()
        startBrowserWaitIfNeeded()
    }

    private func openInitPage() {
        if !openURL(initPageURL) {
            Logger.journalApp.error("failed to open \(self.initPageURL.absoluteString, privacy: .public) in the browser")
        }
    }

    @discardableResult
    private func startBrowserWaitIfNeeded() -> Task<Void, Never> {
        if let browserWaitTask {
            return browserWaitTask
        }
        browserWaitGeneration += 1
        let generation = browserWaitGeneration
        let task = Task { @MainActor [weak self] in
            await self?.waitForSetupComplete()
            if self?.browserWaitGeneration == generation {
                self?.browserWaitTask = nil
            }
        }
        browserWaitTask = task
        return task
    }

    private func waitForSetupComplete() async {
        while !Task.isCancelled, !isTerminating {
            do {
                if try await initClient.probeSetupComplete() == .complete {
                    guard !Task.isCancelled, !isTerminating else { return }
                    await landHome()
                    return
                }
            } catch {
                Logger.journalApp.debug("first-run probe while waiting on the browser: \(String(describing: error), privacy: .public)")
            }
            do {
                try await Task.sleep(for: browserPollInterval)
            } catch {
                return
            }
        }
    }

    private func landHome() async {
        revision &+= 1
        stopRefresh()
        actionInFlight = false
        handoffError = nil
        windowModel?.applyFirstRunLanding()
        route = .home
        await windowModel?.fetchIdentityIfNeeded()
    }

    private func applySetupEvent(_ event: JournalSetupProgressEvent) {
        setupEvents.append(event)
        switch event {
        case .stepStarted(let step, _, _):
            currentStep = step
        case .stepFailed(let step, _, let message):
            currentStep = step
            errorMessage = message
        case .warning, .completed:
            break
        }
    }

    private func supervisorFailureMessage() -> String {
        windowModel?.supervisor.blockedReason
            ?? JournalFirstRunCopy.adoptFailed
    }

    private func readableMessage(for error: Error) -> String {
        if let runnerError = error as? JournalSetupRunnerError {
            switch runnerError {
            case .gateBlocked(let blockage):
                return blockage.ownerMessage
            case .materializeFailed(let message),
                 .runtimeDirectoryFailed(let message),
                 .subprocessLaunchFailed(let message),
                 .setupFailed(_, let message):
                return message
            case .timedOut:
                return "setup timed out"
            case .duplicateCompletion:
                return "setup reported completion twice"
            case .missingCompletion:
                return "setup did not finish"
            }
        }
        return error.localizedDescription
    }

    var isRefreshLoopRunning: Bool { refreshTask != nil }

    var launchAtLoginEnabled: Bool { config.launchAtLoginEnabled }

    var validatedMark: JournalMark? {
        guard let render = setupState?.mark.render else { return nil }
        return JournalMark.validate(render)
    }

    var offersLocalReady: Bool {
        guard handoffError == nil, case .ritual(.thinking) = route else { return false }
        guard setupState?.thinking.saved == false else { return false }
        return setupState?.thinking.capability == "ok"
    }

    var canFinalizeThinking: Bool {
        handoffError == nil
            && setupState?.mark.state == "locked"
            && setupState?.thinking.saved == true
    }

    var showsMarkRegenerate: Bool {
        guard handoffError == nil, case .ritual(.mark) = route else { return false }
        return setupState?.mark.state != "locked"
    }

    var showsSpeechRetry: Bool { setupState?.downloads.speech.state == "error" }

    var showsThinkingRetry: Bool {
        let wire = setupState?.downloads.thinking.state
        return wire == "error" || wire == "cancelled"
    }

    var showsSpeechCancel: Bool { canCancel(setupState?.downloads.speech) }

    var showsThinkingCancel: Bool { canCancel(setupState?.downloads.thinking) }

    var thinkingSizeLine: String {
        guard let copy = setupCopy else { return "" }
        guard let bytes = setupState?.thinking.downloadBytes else { return copy.thinking.sizeUnknown }
        return copy.thinking.defaultLine.replacingOccurrences(of: "{size}", with: String(bytes))
    }

    func downloadLabel(for wire: String) -> String {
        guard let downloads = setupCopy?.downloads else { return "" }
        switch wire {
        case "downloading": return downloads.progress
        case "idle": return downloads.idle
        case "complete": return downloads.complete
        case "cancelled": return downloads.cancelled
        case "error": return downloads.error
        default: return ""
        }
    }

    func setWelcomeLaunchAtLogin(_ enabled: Bool) {
        do {
            try config.setLaunchAtLoginEnabledReportingFailure(enabled)
            loginFailure = nil
        } catch {
            loginFailure = error.localizedDescription
        }
    }

    func continueFromWelcome() {
        handoffError = nil
        if config.journalRoot == nil {
            journalRoot = defaultJournalRoot
        }
        route = .ritual(.folder)
    }

    func continueFromFolder() async {
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        let root = journalRoot.standardizedFileURL
        journalRoot = root
        config.journalRoot = root
        nativeFolderSetup = true
        await runQualifiedSetup(skipModelInstall: true)
    }

    func retrySetup() async {
        if nativeQualified {
            if nativeFolderSetup {
                await continueFromFolder()
            } else {
                await runQualifiedSetup(skipModelInstall: false)
            }
        } else {
            await runSetupThenStartSupervisor()
        }
    }

    func retryHandoff() async {
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        handoffError = nil
        if let root = config.journalRoot?.standardizedFileURL,
           continuation.load(matching: root) != nil {
            await resumeContinuation(root)
            return
        }
        if case .ready(let handoff) = mailbox.loadStep1() {
            await acceptStep1(handoff)
            return
        }
        presentHandoffFailure()
    }

    func commitDisplayedMark() async {
        guard let proposalID = displayedProposalID, !proposalID.isEmpty else { return }
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        let captured = revision
        do {
            let state = try await setupClient.perform(.commitMark(proposalID: proposalID))
            guard revision == captured else { return }
            await presentInteractiveState(state)
        } catch {
            await handleActionError(error, captured: captured, conflictStaysOnMark: true)
        }
    }

    func regenerateDisplayedMark() async {
        guard showsMarkRegenerate else { return }
        await regenerateMark(userInitiated: true)
    }

    func saveThinking(_ choice: String) async {
        if choice == "local", setupState?.thinking.capability != "ok" { return }
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        let captured = revision
        do {
            let state = try await setupClient.perform(.saveThinking(choice: choice))
            guard revision == captured else { return }
            choosingThinking = false
            await presentInteractiveState(state)
        } catch {
            await handleActionError(error, captured: captured, conflictStaysOnMark: false)
        }
    }

    func finalizeThinking() async {
        guard canFinalizeThinking else { return }
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        let captured = revision
        guard let root = config.journalRoot?.standardizedFileURL else {
            presentHandoffFailure()
            return
        }
        let pending = continuation.load(matching: root)?.pendingStep5HandoffID ?? UUID()
        do {
            try continuation.save(root: root, pending: pending)
        } catch {
            thinkingFailure = error.localizedDescription
            return
        }
        do {
            let state = try await setupClient.perform(
                .finalize(timezone: TimeZone.autoupdatingCurrent.identifier)
            )
            guard revision == captured else { return }
            setupState = state
            if state.done {
                await deliverStep5(root: root, captured: captured)
                return
            }
            await presentInteractiveState(state)
        } catch {
            await handleActionError(error, captured: captured, conflictStaysOnMark: false)
        }
    }

    func cancelThinkingDownload() async {
        guard let job = setupState?.downloads.thinking,
              job.state == "downloading",
              let attemptID = job.attemptID,
              !attemptID.isEmpty else { return }
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        let captured = revision
        do {
            let state = try await setupClient.perform(.cancelThinkingDownload(attemptID: attemptID))
            guard revision == captured else { return }
            setupState = state
            displayedProposalID = state.mark.proposalID
        } catch {
            await handleActionError(error, captured: captured, conflictStaysOnMark: false)
        }
    }

    func cancelSpeechDownload() async {}

    func retrySpeechDownload() async {
        guard showsSpeechRetry else { return }
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        let captured = revision
        do {
            let state = try await setupClient.perform(.retrySpeechDownload)
            guard revision == captured else { return }
            setupState = state
            displayedProposalID = state.mark.proposalID
        } catch {
            await handleActionError(error, captured: captured, conflictStaysOnMark: false)
        }
    }

    func retryThinkingDownload() async {
        guard showsThinkingRetry else { return }
        guard beginUserAction() else { return }
        defer { actionInFlight = false }
        let captured = revision
        do {
            let state = try await setupClient.perform(.startThinkingDownload)
            guard revision == captured else { return }
            setupState = state
            displayedProposalID = state.mark.proposalID
        } catch {
            await handleActionError(error, captured: captured, conflictStaysOnMark: false)
        }
    }

    func advanceSetupRevision() {
        revision &+= 1
    }

    private func qualifyForNativeSetup() async -> Bool {
        guard localSetupCapability() else { return false }
        guard let copy = await loadSetupCopy() else { return false }
        guard let url = peer.resolve(), await peer.verify(url) else { return false }
        setupCopy = copy
        return true
    }

    private func decideQualifiedLaunchRoute() async {
        switch mailbox.loadStep1() {
        case .malformed:
            if config.journalRoot == nil {
                showWelcome()
                return
            }
        case .ready(let handoff):
            await acceptStep1(handoff)
            return
        case .missing:
            break
        }

        if let root = config.journalRoot?.standardizedFileURL,
           continuation.load(matching: root) != nil {
            await resumeContinuation(root)
            return
        }

        let hasIntent = continuation.load() != nil
        let hasRoot = config.journalRoot != nil
        do {
            if let legacy = try handoffStore.load() {
                switch legacy.provenance {
                case JournalHandoffProvenance.bundledMigration:
                    if !hasRoot && !hasIntent {
                        await adoptQualified()
                        return
                    }
                    consumeHandoffBestEffort()
                case JournalHandoffProvenance.observerDiscovery:
                    if !hasRoot && !hasIntent {
                        await routeDiscoveryHandoff(legacy)
                        return
                    }
                    consumeHandoffBestEffort()
                default:
                    consumeHandoffBestEffort()
                }
            }
        } catch {
            consumeHandoffBestEffort()
        }

        if let root = config.journalRoot?.standardizedFileURL {
            await resumeQualifiedOwner(root)
        } else {
            showWelcome()
        }
    }

    private func showWelcome() {
        errorMessage = nil
        handoffError = nil
        adoptMessage = nil
        markFailure = nil
        if config.journalRoot == nil {
            journalRoot = defaultJournalRoot
        }
        route = .ritual(.welcome)
        syncRefresh()
    }

    private func adoptQualified() async {
        route = .adopting
        adoptMessage = JournalFirstRunCopy.adoptOpening
        errorMessage = nil
        do {
            guard let handoff = try handoffStore.load() else {
                showWelcome()
                return
            }
            let root = URL(fileURLWithPath: handoff.journalRootPath, isDirectory: true).standardizedFileURL
            journalRoot = root
            config.journalRoot = root
            revision &+= 1
            try await runSetup(at: root, skipModelInstall: false)
            guard await ensureSupervisor(root) else {
                errorMessage = supervisorFailureMessage()
                return
            }
            try handoffStore.consume()
            adoptMessage = JournalFirstRunCopy.adoptLandingLine
            await routeAfterQualifiedState(root, allowStep5: continuation.load(matching: root) != nil)
        } catch {
            errorMessage = readableMessage(for: error)
        }
    }

    private func runQualifiedSetup(skipModelInstall: Bool) async {
        revision &+= 1
        route = .ritual(.setupProgress)
        errorMessage = nil
        handoffError = nil
        do {
            try await runSetup(at: journalRoot, skipModelInstall: skipModelInstall)
            let root = journalRoot.standardizedFileURL
            guard await ensureSupervisor(root) else {
                errorMessage = supervisorFailureMessage()
                return
            }
            await routeAfterQualifiedState(root, allowStep5: continuation.load(matching: root) != nil)
        } catch {
            errorMessage = readableMessage(for: error)
        }
    }

    private func resumeQualifiedOwner(_ root: URL) async {
        journalRoot = root
        guard await ensureSupervisor(root) else {
            errorMessage = supervisorFailureMessage()
            presentHandoffFailure()
            return
        }
        await routeAfterQualifiedState(root, allowStep5: false)
    }

    private func resumeContinuation(_ root: URL) async {
        journalRoot = root
        if receiptMatchesIntent(root) {
            try? continuation.remove()
            await landHome()
            return
        }
        guard await ensureSupervisor(root) else {
            errorMessage = supervisorFailureMessage()
            presentHandoffFailure()
            return
        }
        await routeAfterQualifiedState(root, allowStep5: true)
    }

    private func acceptStep1(_ handoff: SetupHandoff) async {
        onBeforeStep1Commit?()
        let configured = config.journalRoot?.standardizedFileURL
        if let configured {
            journalRoot = configured
        }
        let receipt = try? mailbox.readStep1Receipt()
        let alreadyAccepted = receipt?.handoffID == handoff.handoffID
        let matching = configured.flatMap { continuation.load(matching: $0) }
        let throwInProgress = matching != nil && receipt != nil && receipt?.handoffID != handoff.handoffID
        let pending = matching?.pendingStep5HandoffID
        let continuationStore = continuation
        do {
            let result = try mailbox.commitStep1(
                loadedID: handoff.handoffID,
                alreadyAccepted: alreadyAccepted,
                refuseNewAcceptance: false,
                storeProgress: {
                    if throwInProgress {
                        throw SetupMailboxError.continuationInProgress
                    }
                    if let configured {
                        try continuationStore.save(root: configured, pending: pending)
                    }
                }
            )
            switch result {
            case .accepted, .republished:
                if let configured {
                    journalRoot = configured
                    guard await ensureSupervisor(configured) else {
                        errorMessage = supervisorFailureMessage()
                        presentHandoffFailure()
                        return
                    }
                    await routeAfterQualifiedState(
                        configured,
                        allowStep5: continuation.load(matching: configured) != nil
                    )
                } else {
                    showWelcome()
                }
            default:
                presentHandoffFailure()
            }
        } catch {
            presentHandoffFailure()
        }
    }

    private func routeAfterQualifiedState(_ root: URL, allowStep5: Bool) async {
        let captured = revision
        do {
            let state = try await setupClient.state()
            guard revision == captured else { return }
            if state.done {
                if allowStep5, continuation.load(matching: root) != nil {
                    await deliverStep5(root: root, captured: captured)
                } else {
                    await landHome()
                }
                return
            }
            await presentInteractiveState(state)
        } catch {
            guard revision == captured else { return }
            await handleStateError(error)
        }
    }

    private func deliverStep5(root: URL, captured: UInt64) async {
        guard revision == captured else { return }
        guard let selected = peer.resolve(), await peer.verify(selected) else {
            presentHandoffFailure()
            return
        }
        let selectedIdentity = peerIdentity(selected)
        guard revision == captured else { return }
        let state: JournalSetupState
        do {
            state = try await setupClient.state()
        } catch {
            guard revision == captured else { return }
            await handleStateError(error)
            return
        }
        guard revision == captured, samePath(state.journalPath, root) else {
            if revision == captured { presentHandoffFailure() }
            return
        }
        let resolution = await identity.resolveIdentity(baseURL: Self.setupBaseURL)
        guard case .mark(_, let instanceID) = resolution else {
            presentHandoffFailure()
            return
        }
        let network: JournalNetworkStatus
        do {
            network = try await setupClient.networkStatus()
        } catch {
            guard revision == captured else { return }
            presentHandoffFailure()
            return
        }
        guard revision == captured,
              network.instanceID == instanceID,
              let fingerprint = network.caFingerprint,
              isFingerprint(fingerprint) else {
            if revision == captured { presentHandoffFailure() }
            return
        }
        let pending = continuation.load(matching: root)?.pendingStep5HandoffID ?? UUID()
        do {
            try continuation.save(root: root, pending: pending)
            try mailbox.writeStep5(SetupHandoff(
                handoffID: pending,
                stepIndex: 5,
                journalRootPath: root.standardizedFileURL.path,
                expectedInstanceID: instanceID,
                expectedCAFingerprint: fingerprint
            ))
        } catch {
            presentHandoffFailure()
            return
        }
        guard revision == captured else { return }
        guard let again = peer.resolve(), await peer.verify(again) else {
            presentHandoffFailure()
            return
        }
        guard peerIdentity(again) == selectedIdentity else {
            presentHandoffFailure()
            return
        }
        do {
            try await peer.launch(again)
        } catch {
            presentHandoffFailure()
        }
    }

    private func presentInteractiveState(_ state: JournalSetupState) async {
        setupState = state
        displayedProposalID = state.mark.proposalID
        if state.mark.state == "unavailable", !didAutoRegenerate {
            didAutoRegenerate = true
            await regenerateMark(userInitiated: false)
            return
        }
        handoffError = nil
        markFailure = nil
        if state.mark.state == "locked" {
            route = .ritual(.thinking)
        } else {
            route = .ritual(.mark)
        }
        syncRefresh()
    }

    private func regenerateMark(userInitiated: Bool) async {
        if userInitiated {
            guard beginUserAction() else { return }
            defer { actionInFlight = false }
            await sendRegenerate()
        } else {
            await sendRegenerate()
        }
    }

    private func sendRegenerate() async {
        let captured = revision
        do {
            let state = try await setupClient.perform(.regenerateMark)
            guard revision == captured else { return }
            await presentInteractiveState(state)
        } catch {
            await handleActionError(error, captured: captured, conflictStaysOnMark: true)
        }
    }

    private func handleStateError(_ error: Error) async {
        if let clientError = error as? JournalSetupClientError,
           case .server(let envelope) = clientError,
           envelope.reasonCode == "link_identity_error" {
            applyLinkIdentity(envelope)
            return
        }
        presentHandoffFailure()
    }

    private func handleActionError(_ error: Error, captured: UInt64, conflictStaysOnMark: Bool) async {
        guard revision == captured else { return }
        guard let clientError = error as? JournalSetupClientError,
              case .server(let envelope) = clientError else {
            presentHandoffFailure()
            return
        }
        switch envelope.reasonCode {
        case "speech_admission_pending":
            await refreshSameRevision(captured, forceMark: false)
        case "proposal_conflict":
            await refreshSameRevision(captured, forceMark: conflictStaysOnMark)
        case "link_identity_error":
            applyLinkIdentity(envelope)
        default:
            presentHandoffFailure()
        }
    }

    private func refreshSameRevision(_ captured: UInt64, forceMark: Bool) async {
        do {
            let state = try await setupClient.state()
            guard revision == captured else { return }
            setupState = state
            displayedProposalID = state.mark.proposalID
            markFailure = nil
            errorMessage = nil
            if forceMark {
                handoffError = nil
                route = .ritual(.mark)
            }
            syncRefresh()
        } catch {
            guard revision == captured else { return }
        }
    }

    private func applyLinkIdentity(_ envelope: JournalDevicesErrorEnvelope) {
        let base = setupCopy?.mark.error ?? ""
        let detail = envelope.detail ?? ""
        markFailure = detail.isEmpty ? base : base + "\n" + detail
        handoffError = nil
        errorMessage = nil
        route = .ritual(.mark)
        syncRefresh()
    }

    private func presentHandoffFailure() {
        handoffError = setupCopy?.mark.error ?? ""
        if case .ritual = route {
        } else {
            route = .ritual(.thinking)
        }
        syncRefresh()
    }

    private func applyRefreshedState(_ state: JournalSetupState) {
        setupState = state
        if handoffError == nil {
            displayedProposalID = state.mark.proposalID
        }
        syncRefresh()
    }

    private func ensureSupervisor(_ root: URL) async -> Bool {
        if supervisorIsLive(root) || externalSupervisorIsLive(root) {
            return true
        }
        return await startSupervisor(root)
    }

    private func receiptMatchesIntent(_ root: URL) -> Bool {
        guard let intent = continuation.load(matching: root),
              let pending = intent.pendingStep5HandoffID,
              let receipt = try? mailbox.readStep5Receipt() else {
            return false
        }
        return receipt.schema == SetupHandoffReceipt.schemaV1
            && receipt.stepIndex == 5
            && receipt.handoffID == pending
    }

    private func beginUserAction() -> Bool {
        guard !actionInFlight else { return false }
        actionInFlight = true
        revision &+= 1
        return true
    }

    private func syncRefresh() {
        if refreshRouteActive {
            startRefreshIfNeeded()
        } else {
            stopRefresh()
        }
    }

    private var refreshRouteActive: Bool {
        guard nativeQualified else { return false }
        switch route {
        case .ritual(.mark), .ritual(.thinking):
            return supervisorIsLive(journalRoot) || externalSupervisorIsLive(journalRoot)
        default:
            return false
        }
    }

    private func startRefreshIfNeeded() {
        guard refreshTask == nil else { return }
        let sleep = refreshSleep
        refreshTask = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                await sleep()
                if Task.isCancelled { return }
                await self?.refreshOnce()
            }
        }
    }

    private func stopRefresh() {
        refreshTask?.cancel()
        refreshTask = nil
    }

    private func refreshOnce() async {
        let captured = revision
        do {
            let state = try await setupClient.state()
            guard !Task.isCancelled, revision == captured else { return }
            applyRefreshedState(state)
        } catch {
            return
        }
    }

    private func canCancel(_ job: JournalSetupState.Job?) -> Bool {
        guard let job else { return false }
        return job.state == "downloading" && !(job.attemptID ?? "").isEmpty
    }

    private func samePath(_ path: String, _ root: URL) -> Bool {
        JournalSetupContinuationStore.standardizedPath(path) == root.standardizedFileURL.path
    }

    private func peerIdentity(_ url: URL) -> URL {
        url.standardizedFileURL.resolvingSymlinksInPath()
    }

    private func isFingerprint(_ value: String) -> Bool {
        value.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil
    }
}
