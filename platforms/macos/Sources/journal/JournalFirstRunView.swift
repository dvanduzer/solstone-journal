// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import AppKit
import JournalMarkKit
import JournalRuntime
import SolstoneCore
import SwiftUI

struct JournalFirstRunView: View {
    @Bindable var model: JournalFirstRunModel

    var body: some View {
        Group {
            switch model.route {
            case .deciding:
                decidingView
            case .ritual(let step):
                ritualView(step)
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier(AXID.Journal.Ritual.root)
                    .overlay {
                        AXStateCompanion(id: AXID.Journal.Ritual.routeState, value: model.route.axToken)
                    }
            case .adopting:
                adoptView
                    .accessibilityElement(children: .contain)
                    .accessibilityIdentifier(AXID.Journal.Adopt.root)
            case .home:
                EmptyView()
            }
        }
        .frame(minWidth: 720, minHeight: 500)
        .overlay {
            AXStateCompanion(
                id: AXID.Journal.Ritual.operationPendingState,
                value: model.pendingOperation.axToken
            )
        }
        .overlay {
            AXStateCompanion(
                id: AXID.Journal.Ritual.operationErrorState,
                value: model.operationError.axToken
            )
        }
        .overlay {
            AXStateCompanion(
                id: AXID.Journal.Ritual.outboundState,
                value: model.outbound.axToken
            )
        }
        .overlay(alignment: .top) {
            if model.userOperationInFlight || model.pendingOperation != .none {
                ProgressView()
                    .padding(.top, 12)
            }
        }
    }

    private var decidingView: some View {
        VStack {
            ProgressView()
                .controlSize(.large)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .overlay {
            AXStateCompanion(id: AXID.Journal.Ritual.routeState, value: model.route.axToken)
        }
    }

    @ViewBuilder
    private func ritualView(_ step: JournalFirstRunStep) -> some View {
        if let handoffError = model.handoffError, !handoffError.isEmpty {
            handoffRetryCard(handoffError)
        } else {
            switch step {
            case .welcome:
                welcomeView
            case .folder:
                folderView
            case .nameLocation:
                nameLocationView
            case .mark:
                markView
            case .thinking:
                thinkingView
            case .setupProgress:
                setupProgressView
            case .finishInBrowser:
                finishInBrowserView
            }
        }
    }

    private var nameLocationView: some View {
        VStack(alignment: .leading, spacing: 22) {
            Text(JournalFirstRunCopy.nameLocationTitle)
                .font(.title2.weight(.semibold))

            VStack(alignment: .leading, spacing: 12) {
                Text(JournalFirstRunCopy.locationField)
                    .font(.headline)
                HStack(spacing: 8) {
                    TextField(
                        JournalFirstRunCopy.locationField,
                        text: Binding(
                            get: { model.journalRoot.path },
                            set: { model.journalRoot = URL(fileURLWithPath: $0, isDirectory: true) }
                        )
                    )
                    .textFieldStyle(.roundedBorder)
                    .accessibilityIdentifier(AXID.Journal.Ritual.locationField)

                    Button {
                        chooseJournalLocation()
                    } label: {
                        Label(JournalFirstRunCopy.chooseLocation, systemImage: "folder")
                    }
                    .accessibilityIdentifier(AXID.Journal.Ritual.locationChoose)
                }
            }

            if let error = model.errorMessage {
                errorNotice(error)
                AXStateCompanion(id: AXID.Journal.Ritual.nameLocationErrorState, value: error)
            } else {
                AXStateCompanion(id: AXID.Journal.Ritual.nameLocationErrorState, value: "")
            }

            Button {
                Task { await model.continueFromNameLocation() }
            } label: {
                Label(JournalFirstRunCopy.continueButton, systemImage: "arrow.right")
            }
            .keyboardShortcut(.defaultAction)
            .accessibilityIdentifier(AXID.Journal.Ritual.nameLocationContinue)
        }
        .padding(32)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .frame(maxWidth: 620, maxHeight: .infinity, alignment: .center)
    }

    private var setupProgressView: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(JournalFirstRunCopy.setupTitle)
                .font(.title2.weight(.semibold))
            Text(JournalFirstRunCopy.setupSubtitle)
                .foregroundStyle(.secondary)

            if model.errorMessage == nil {
                ProgressView()
                    .controlSize(.large)
                    .accessibilityIdentifier(AXID.Journal.Ritual.setupProgress)
            }

            Text(model.currentStep ?? JournalFirstRunCopy.setupTitle)
                .foregroundStyle(.secondary)
            AXStateCompanion(id: AXID.Journal.Ritual.setupStepState, value: model.currentStep ?? "")

            if !model.setupRenderedLog.isEmpty {
                ScrollView {
                    Text(model.setupRenderedLog)
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 160)
            }
            AXStateCompanion(id: AXID.Journal.Ritual.setupLogState, value: model.setupRenderedLog)

            if let error = model.errorMessage {
                errorNotice(error)
                AXStateCompanion(id: AXID.Journal.Ritual.setupErrorState, value: error)
                Button {
                    Task { await model.retrySetup() }
                } label: {
                    Label(JournalFirstRunCopy.tryAgain, systemImage: "arrow.clockwise")
                }
                .accessibilityIdentifier(AXID.Journal.Ritual.setupRetry)
            } else {
                AXStateCompanion(id: AXID.Journal.Ritual.setupErrorState, value: "")
            }
        }
        .padding(32)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .frame(maxWidth: 620, maxHeight: .infinity, alignment: .center)
    }

    private var finishInBrowserView: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(JournalFirstRunCopy.finishInBrowserTitle)
                .font(.title2.weight(.semibold))

            HStack(alignment: .firstTextBaseline, spacing: 10) {
                ProgressView()
                    .controlSize(.small)
                Text(JournalFirstRunCopy.finishInBrowserLine)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Button {
                model.reopenInBrowser()
            } label: {
                Label(JournalFirstRunCopy.reopenInBrowserButton, systemImage: "safari")
            }
            .keyboardShortcut(.defaultAction)
            .accessibilityIdentifier(AXID.Journal.Ritual.browserReopen)
        }
        .padding(32)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .frame(maxWidth: 620, maxHeight: .infinity, alignment: .center)
    }

    private var adoptView: some View {
        VStack(alignment: .leading, spacing: 20) {
            Text(JournalFirstRunCopy.adoptTitle)
                .font(.title2.weight(.semibold))

            if model.errorMessage == nil, model.adoptState != .landed {
                ProgressView()
                    .controlSize(.large)
            }

            Text(model.adoptMessage ?? JournalFirstRunCopy.adoptOpening)
                .foregroundStyle(.secondary)
            AXStateCompanion(id: AXID.Journal.Adopt.statusState, value: model.adoptState.axToken)
            AXStateCompanion(id: AXID.Journal.Adopt.messageState, value: model.adoptMessage ?? "")
            AXStateCompanion(id: AXID.Journal.Adopt.locationPathState, value: model.journalRoot.path)

            if let error = model.errorMessage {
                errorNotice(error)
                AXStateCompanion(id: AXID.Journal.Adopt.errorState, value: error)
                Button {
                    Task { await model.retryAdopt() }
                } label: {
                    Label(JournalFirstRunCopy.tryAgain, systemImage: "arrow.clockwise")
                }
                .accessibilityIdentifier(AXID.Journal.Adopt.continueButton)
            } else {
                AXStateCompanion(id: AXID.Journal.Adopt.errorState, value: "")
            }
        }
        .padding(32)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .frame(maxWidth: 620, maxHeight: .infinity, alignment: .center)
    }

    private var welcomeView: some View {
        ritualCard(spacing: 12) {
            if let copy = model.setupCopy {
                Text("1")
                    .font(.title2.weight(.semibold))
                    .monospacedDigit()
                Text(copy.welcome.title)
                    .font(.title2.weight(.semibold))
                Text(copy.welcome.body)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if let close = copy.welcome.howItFits.last,
                   close != copy.welcome.title, close != copy.welcome.body {
                    Text(close)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Text(copy.fits.title)
                    .font(.headline)
                copyRow(term: copy.fits.journalTerm, body: copy.fits.journal)
                copyRow(term: copy.fits.appTerm, body: copy.fits.app)
                copyRow(term: copy.fits.thinkingTerm, body: copy.fits.thinking)
                Toggle(copy.login.label, isOn: Binding(
                    get: { model.launchAtLoginEnabled },
                    set: { model.setWelcomeLaunchAtLogin($0) }
                ))
                .accessibilityIdentifier(AXID.Journal.Ritual.loginToggle)
                Text(copy.login.help)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                AXStateCompanion(
                    id: AXID.Journal.Ritual.loginFailureState,
                    value: model.loginFailure ?? ""
                )
                if let failure = model.loginFailure, !failure.isEmpty {
                    errorNotice(failure)
                }
                Button {
                    model.continueFromWelcome()
                } label: {
                    Label(copy.folder.continueLabel, systemImage: "arrow.right")
                }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier(AXID.Journal.Ritual.welcomeContinue)
            }
        }
    }

    private var folderView: some View {
        ritualCard {
            if let copy = model.setupCopy {
                Text("2")
                    .font(.title2.weight(.semibold))
                    .monospacedDigit()
                Text(copy.folder.title)
                    .font(.title2.weight(.semibold))
                Text(copy.folder.help)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 8) {
                    TextField(
                        copy.folder.title,
                        text: Binding(
                            get: { model.journalRoot.path },
                            set: { model.journalRoot = URL(fileURLWithPath: $0, isDirectory: true) }
                        )
                    )
                    .textFieldStyle(.roundedBorder)
                    .accessibilityIdentifier(AXID.Journal.Ritual.folderField)
                    .disabled(model.userOperationInFlight || model.pendingOperation != .none)
                    Button {
                        chooseJournalLocation()
                    } label: {
                        Label(copy.folder.change, systemImage: "folder")
                    }
                    .accessibilityIdentifier(AXID.Journal.Ritual.folderChoose)
                    .disabled(model.userOperationInFlight || model.pendingOperation != .none)
                }
                if let error = model.errorMessage, !error.isEmpty {
                    errorNotice(error)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Button {
                    Task { await model.continueFromFolder() }
                } label: {
                    Label(copy.folder.continueLabel, systemImage: "arrow.right")
                }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier(AXID.Journal.Ritual.folderContinue)
                .disabled(model.userOperationInFlight || model.pendingOperation != .none)
            }
        }
    }

    private var markView: some View {
        ritualCard(spacing: 12) {
            if let copy = model.setupCopy {
                Text("3")
                    .font(.title2.weight(.semibold))
                    .monospacedDigit()
                Text(copy.mark.title)
                    .font(.title2.weight(.semibold))
                Text(copy.mark.help)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if let mark = model.validatedMark {
                    JournalMarkView(mark: mark, isConfirmed: model.setupState?.mark.state == .locked)
                }
                if let failure = model.markFailure, !failure.isEmpty {
                    errorNotice(failure)
                        .fixedSize(horizontal: false, vertical: true)
                } else if model.validatedMark == nil {
                    errorNotice(copy.mark.error)
                        .fixedSize(horizontal: false, vertical: true)
                }
                AXStateCompanion(
                    id: AXID.Journal.Ritual.markProposalState,
                    value: model.displayedProposalID ?? ""
                )
                AXStateCompanion(
                    id: AXID.Journal.Ritual.markErrorState,
                    value: model.markFailure ?? ""
                )
                if model.validatedMark != nil {
                    Text(copy.mark.permanence)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                HStack(spacing: 12) {
                    if model.showsMarkRegenerate {
                        Button {
                            Task { await model.regenerateDisplayedMark() }
                        } label: {
                            Label(copy.mark.tryAnother, systemImage: "arrow.triangle.2.circlepath")
                        }
                        .accessibilityIdentifier(AXID.Journal.Ritual.markRegenerate)
                        .disabled(model.userOperationInFlight || model.pendingOperation != .none)
                    }
                    Button {
                        Task { await model.commitDisplayedMark() }
                    } label: {
                        Label(copy.mark.continueLabel, systemImage: "arrow.right")
                    }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier(AXID.Journal.Ritual.markContinue)
                    .disabled(!model.canCommitDisplayedMark || model.userOperationInFlight || model.pendingOperation != .none)
                }
            }
        }
    }

    private var thinkingView: some View {
        ritualCard {
            if let copy = model.setupCopy {
                Text("4")
                    .font(.title2.weight(.semibold))
                    .monospacedDigit()
                Text(copy.thinking.title)
                    .font(.title2.weight(.semibold))
                if model.displayedThinkingChoice == .local {
                    Text(model.thinkingSizeLine)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let description = model.selectedThinkingDescription {
                        Text(description)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                } else if let choice = model.displayedThinkingChoice, choice != .later {
                    savedThinking(copy)
                } else {
                    Text(copy.thinking.chooseLater)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Button {
                    model.choosingThinking.toggle()
                } label: {
                    Label(copy.thinking.change, systemImage: "list.bullet")
                }
                .accessibilityIdentifier(AXID.Journal.Ritual.thinkingChange)
                .disabled(model.userOperationInFlight || model.pendingOperation != .none)
                if model.choosingThinking {
                    ForEach(
                        copy.thinking.choices.filter { $0.id != JournalThinkingChoice.local.rawValue || model.offersLocalThinking },
                        id: \.id
                    ) { choice in
                        let selected = model.displayedThinkingChoice?.rawValue == choice.id
                        Button {
                            model.selectThinking(choice.id)
                        } label: {
                            HStack(alignment: .firstTextBaseline, spacing: 8) {
                                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                                    .accessibilityHidden(true)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(choice.label)
                                    Text(choice.sub)
                                        .foregroundStyle(.secondary)
                                    Text(choice.description)
                                        .foregroundStyle(.secondary)
                                        .fixedSize(horizontal: false, vertical: true)
                                }
                            }
                        }
                        .accessibilityAddTraits(selected ? .isSelected : [])
                        .disabled(model.userOperationInFlight || model.pendingOperation != .none)
                        .modifier(ThinkingChoiceIdentifier(choiceID: choice.id))
                    }
                    Button {
                        model.selectThinking("later")
                    } label: {
                        Label(
                            copy.thinking.later,
                            systemImage: model.displayedThinkingChoice == .later ? "checkmark.circle.fill" : "circle"
                        )
                    }
                    .accessibilityAddTraits(model.displayedThinkingChoice == .later ? .isSelected : [])
                    .accessibilityIdentifier(AXID.Journal.Ritual.thinkingLater)
                    .disabled(model.userOperationInFlight || model.pendingOperation != .none)
                }
                AXStateCompanion(
                    id: AXID.Journal.Ritual.thinkingChoiceState,
                    value: model.displayedThinkingChoice?.rawValue ?? ""
                )
                AXStateCompanion(
                    id: AXID.Journal.Ritual.thinkingErrorState,
                    value: model.thinkingFailure ?? ""
                )
                if let failure = model.thinkingFailure, !failure.isEmpty {
                    errorNotice(failure)
                }
                if model.canContinueThinking {
                    Button {
                        Task { await model.continueThinking() }
                    } label: {
                        Label(copy.thinking.continueLabel, systemImage: "arrow.right")
                    }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier(AXID.Journal.Ritual.thinkingContinue)
                    .disabled(model.userOperationInFlight || model.pendingOperation != .none)
                }
            }
        }
    }

    private func handoffRetryCard(_ message: String) -> some View {
        ritualCard {
            errorNotice(message)
                .fixedSize(horizontal: false, vertical: true)
            AXStateCompanion(id: AXID.Journal.Ritual.handoffErrorState, value: message)
            Button {
                Task { await model.retryHandoff() }
            } label: {
                Label(model.setupCopy?.downloads.retry ?? "", systemImage: "arrow.clockwise")
            }
            .accessibilityIdentifier(AXID.Journal.Ritual.handoffRetry)
        }
    }

    @ViewBuilder
    private func savedThinking(_ copy: JournalSetupCopy) -> some View {
        let choice = model.displayedThinkingChoice
        if let row = copy.thinking.choices.first(where: { $0.id == choice?.rawValue }) {
            Text(row.label)
            Text(row.sub)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let description = model.selectedThinkingDescription {
                Text(description)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func errorNotice(_ message: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: "exclamationmark.circle.fill")
                .accessibilityHidden(true)
            Text(message)
        }
        .foregroundStyle(.primary)
        .accessibilityElement(children: .combine)
    }

    private func copyRow(term: String, body: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(term)
                .font(.headline)
            Text(body)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func downloadFooter(_ copy: JournalSetupCopy) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(copy.footer)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack(alignment: .top, spacing: 16) {
                downloadRow(
                    title: copy.downloads.speech,
                    job: model.setupState?.downloads.speech,
                    canRetry: model.showsSpeechRetry,
                    retryID: AXID.Journal.Ritual.downloadSpeechRetry,
                    cancelID: nil,
                    stateID: AXID.Journal.Ritual.downloadSpeechState,
                    failure: model.speechFailure,
                    retry: { await model.retrySpeechDownload() },
                    cancel: {}
                )
                .frame(maxWidth: .infinity, alignment: .leading)
                downloadRow(
                    title: copy.downloads.thinking,
                    job: model.setupState?.downloads.thinking,
                    canRetry: model.showsThinkingRetry,
                    canCancel: model.showsThinkingCancel,
                    retryID: AXID.Journal.Ritual.downloadThinkingRetry,
                    cancelID: AXID.Journal.Ritual.downloadThinkingCancel,
                    stateID: AXID.Journal.Ritual.downloadThinkingState,
                    failure: model.operationError == .thinkingDownload ? model.thinkingFailure : nil,
                    retry: { await model.retryThinkingDownload() },
                    cancel: { await model.cancelThinkingDownload() }
                )
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    private func downloadRow(
        title: String,
        job: JournalSetupState.Job?,
        canRetry: Bool,
        canCancel: Bool = false,
        retryID: String,
        cancelID: String?,
        stateID: String,
        failure: String?,
        retry: @escaping () async -> Void,
        cancel: @escaping () async -> Void
    ) -> some View {
        let wire = job?.state.rawValue ?? ""
        let retryLabel = model.setupCopy?.downloads.retry ?? ""
        let cancelLabel = model.setupCopy?.downloads.cancel ?? ""
        let locked = model.userOperationInFlight || model.pendingOperation != .none
        return VStack(alignment: .leading, spacing: 4) {
            Text(title)
            if let job {
                Text(model.downloadLabel(for: job.state))
                    .foregroundStyle(.secondary)
                if job.state == .downloading {
                    if let total = job.bytesTotal, total > 0 {
                        ProgressView(value: min(1, Double(job.bytesDone) / Double(total)))
                            .accessibilityLabel(title)
                    } else {
                        ProgressView().controlSize(.small).accessibilityLabel(title)
                    }
                }
                if let progress = model.downloadProgressText(for: job) {
                    Text(progress)
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
            }
            if let failure, !failure.isEmpty {
                errorNotice(failure)
                    .lineLimit(2)
                    .help(failure)
                    .accessibilityLabel(failure)
                    .fixedSize(horizontal: false, vertical: true)
            } else if let detail = job?.error?.detail, !detail.isEmpty {
                errorNotice(detail)
                    .lineLimit(2)
                    .help(detail)
                    .accessibilityLabel(detail)
                    .fixedSize(horizontal: false, vertical: true)
            }
            AXStateCompanion(id: stateID, value: wire)
            if canCancel, let cancelID {
                Button {
                    Task { await cancel() }
                } label: {
                    Text(cancelLabel)
                }
                .accessibilityIdentifier(cancelID)
                .accessibilityLabel("\(title) \(cancelLabel)")
                .disabled(locked)
            }
            if canRetry {
                Button {
                    Task { await retry() }
                } label: {
                    Text(retryLabel)
                }
                .accessibilityIdentifier(retryID)
                .accessibilityLabel("\(title) \(retryLabel)")
                .disabled(locked)
            }
        }
    }

    private func ritualCard<Content: View>(spacing: CGFloat = 18, @ViewBuilder content: () -> Content) -> some View {
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: spacing) {
                    content()
                }
                .padding(32)
                .frame(maxWidth: 620, alignment: .leading)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                .frame(maxWidth: .infinity)
            }
            if showsDownloadFooter, let copy = model.setupCopy {
                downloadFooter(copy)
                    .font(.caption)
                    .padding(16)
                    .frame(maxWidth: 620, alignment: .leading)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var showsDownloadFooter: Bool {
        guard model.setupState != nil else { return false }
        switch model.route {
        case .ritual(.mark), .ritual(.thinking), .ritual(.setupProgress): return true
        default: return false
        }
    }

    private func chooseJournalLocation() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.directoryURL = model.journalRoot.deletingLastPathComponent()
        if panel.runModal() == .OK, let url = panel.url {
            model.journalRoot = url.standardizedFileURL
        }
    }
}

private struct ThinkingChoiceIdentifier: ViewModifier {
    let choiceID: String

    func body(content: Content) -> some View {
        if let identifier = AXID.Journal.Ritual.thinkingChoice(choiceID) {
            content.accessibilityIdentifier(identifier)
        } else {
            content
        }
    }
}
