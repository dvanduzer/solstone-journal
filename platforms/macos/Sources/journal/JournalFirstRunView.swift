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
                Text(error)
                    .foregroundStyle(.red)
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

            ProgressView()
                .controlSize(.large)
                .accessibilityIdentifier(AXID.Journal.Ritual.setupProgress)

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
                Text(error)
                    .foregroundStyle(.red)
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
                Text(error)
                    .foregroundStyle(.red)
                AXStateCompanion(id: AXID.Journal.Adopt.errorState, value: error)
                Button {
                    Task { await model.adoptFromHandoff() }
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
        ritualCard {
            if let copy = model.setupCopy {
                Text("1")
                    .font(.title2.weight(.semibold))
                    .monospacedDigit()
                Text(copy.heading)
                    .font(.title2.weight(.semibold))
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
                    Text(failure)
                        .foregroundStyle(.red)
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
                    Button {
                        chooseJournalLocation()
                    } label: {
                        Label(copy.folder.change, systemImage: "folder")
                    }
                    .accessibilityIdentifier(AXID.Journal.Ritual.folderChoose)
                }
                Button {
                    Task { await model.continueFromFolder() }
                } label: {
                    Label(copy.folder.continueLabel, systemImage: "arrow.right")
                }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier(AXID.Journal.Ritual.folderContinue)
            }
        }
    }

    private var markView: some View {
        ritualCard {
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
                    JournalMarkView(mark: mark, isConfirmed: model.setupState?.mark.state == "locked")
                } else {
                    Text(model.markFailure ?? copy.mark.error)
                        .foregroundStyle(.red)
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
                if model.setupState?.mark.state == "locked" {
                    Text(copy.mark.permanence)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if model.showsMarkRegenerate {
                    Button {
                        Task { await model.regenerateDisplayedMark() }
                    } label: {
                        Label(copy.mark.tryAnother, systemImage: "arrow.triangle.2.circlepath")
                    }
                    .accessibilityIdentifier(AXID.Journal.Ritual.markRegenerate)
                }
                Button {
                    Task { await model.commitDisplayedMark() }
                } label: {
                    Label(copy.mark.continueLabel, systemImage: "arrow.right")
                }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier(AXID.Journal.Ritual.markContinue)
                downloadFooter(copy)
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
                if model.setupState?.thinking.saved == true {
                    savedThinking(copy)
                } else if model.offersLocalReady {
                    Text(model.thinkingSizeLine)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Button {
                        Task { await model.saveThinking("local") }
                    } label: {
                        Label(copy.thinking.continueLabel, systemImage: "arrow.right")
                    }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier(AXID.Journal.Ritual.thinkingContinue)
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
                if model.choosingThinking {
                    ForEach(copy.thinking.choices, id: \.id) { choice in
                        Button {
                            Task { await model.saveThinking(choice.id) }
                        } label: {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(choice.label)
                                Text(choice.sub)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .disabled(choice.id == "local" && model.setupState?.thinking.capability != "ok")
                    }
                    Button {
                        Task { await model.saveThinking("later") }
                    } label: {
                        Text(copy.thinking.later)
                    }
                    .accessibilityIdentifier(AXID.Journal.Ritual.thinkingLater)
                }
                AXStateCompanion(
                    id: AXID.Journal.Ritual.thinkingChoiceState,
                    value: model.setupState?.thinking.saved == true ? (model.setupState?.thinking.choice ?? "") : ""
                )
                AXStateCompanion(
                    id: AXID.Journal.Ritual.thinkingErrorState,
                    value: model.thinkingFailure ?? ""
                )
                if let failure = model.thinkingFailure, !failure.isEmpty {
                    Text(failure)
                        .foregroundStyle(.red)
                }
                if model.canFinalizeThinking {
                    Button {
                        Task { await model.finalizeThinking() }
                    } label: {
                        Label(copy.thinking.continueLabel, systemImage: "arrow.right")
                    }
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier(AXID.Journal.Ritual.thinkingContinue)
                }
                downloadFooter(copy)
            }
        }
    }

    private func handoffRetryCard(_ message: String) -> some View {
        ritualCard {
            Text(message)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
            AXStateCompanion(id: AXID.Journal.Ritual.handoffErrorState, value: message)
            Button {
                Task { await model.retryHandoff() }
            } label: {
                Label(model.setupCopy?.downloads.retry ?? "", systemImage: "arrow.clockwise")
            }
            .accessibilityIdentifier(AXID.Journal.Ritual.handoffRetry)
            if let copy = model.setupCopy {
                downloadFooter(copy)
            }
        }
    }

    @ViewBuilder
    private func savedThinking(_ copy: JournalSetupCopy) -> some View {
        let choice = model.setupState?.thinking.choice
        if choice == "later" {
            Text(copy.thinking.later)
        } else if let row = copy.thinking.choices.first(where: { $0.id == choice }) {
            Text(row.label)
            Text(row.sub)
                .foregroundStyle(.secondary)
        }
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
            downloadRow(
                title: copy.downloads.speech,
                job: model.setupState?.downloads.speech,
                canRetry: model.showsSpeechRetry,
                canCancel: model.showsSpeechCancel,
                retryID: AXID.Journal.Ritual.downloadSpeechRetry,
                cancelID: AXID.Journal.Ritual.downloadSpeechCancel,
                stateID: AXID.Journal.Ritual.downloadSpeechState,
                retry: { await model.retrySpeechDownload() },
                cancel: { await model.cancelSpeechDownload() }
            )
            downloadRow(
                title: copy.downloads.thinking,
                job: model.setupState?.downloads.thinking,
                canRetry: model.showsThinkingRetry,
                canCancel: model.showsThinkingCancel,
                retryID: AXID.Journal.Ritual.downloadThinkingRetry,
                cancelID: AXID.Journal.Ritual.downloadThinkingCancel,
                stateID: AXID.Journal.Ritual.downloadThinkingState,
                retry: { await model.retryThinkingDownload() },
                cancel: { await model.cancelThinkingDownload() }
            )
        }
    }

    private func downloadRow(
        title: String,
        job: JournalSetupState.Job?,
        canRetry: Bool,
        canCancel: Bool,
        retryID: String,
        cancelID: String,
        stateID: String,
        retry: @escaping () async -> Void,
        cancel: @escaping () async -> Void
    ) -> some View {
        let wire = job?.state ?? ""
        return VStack(alignment: .leading, spacing: 4) {
            Text(title)
            Text(model.downloadLabel(for: wire))
                .foregroundStyle(.secondary)
            if let job {
                Text(byteLine(done: job.bytesDone, total: job.bytesTotal))
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            AXStateCompanion(id: stateID, value: wire)
            if canCancel {
                Button {
                    Task { await cancel() }
                } label: {
                    Text(model.setupCopy?.downloads.cancel ?? "")
                }
                .accessibilityIdentifier(cancelID)
            }
            if canRetry {
                Button {
                    Task { await retry() }
                } label: {
                    Text(model.setupCopy?.downloads.retry ?? "")
                }
                .accessibilityIdentifier(retryID)
            }
        }
    }

    private func byteLine(done: UInt64, total: UInt64?) -> String {
        if let total {
            return "\(done) \(total)"
        }
        return "\(done)"
    }

    private func ritualCard<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            content()
        }
        .padding(32)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .frame(maxWidth: 620, maxHeight: .infinity, alignment: .center)
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
