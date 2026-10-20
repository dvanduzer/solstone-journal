// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import JournalRuntime
import SolstoneCore

enum JournalDownloadActionKind: Equatable, Sendable {
    case cancelThinking
    case retrySpeech
    case retryThinking
}

struct JournalDownloadRowModel: Equatable, Sendable {
    let name: String
    let detail: String?
    let fraction: Double?
    let indeterminate: Bool
    let actionTitle: String?
    let actionEnabled: Bool
    let actionIdentifier: String?
    let actionKind: JournalDownloadActionKind?
}

struct JournalProgressInfo: Equatable, Sendable {
    let index: Int
    let count: Int
    let label: String
}

enum JournalSetupChrome {
    static func progress(
        route: JournalFirstRunRoute,
        hasRefusal: Bool,
        handoffError: String?
    ) -> JournalProgressInfo? {
        if hasRefusal { return nil }
        if let error = handoffError, !error.isEmpty { return nil }
        guard case .ritual(let step) = route else { return nil }
        let index: Int
        switch step {
        case .welcome: index = 1
        case .folder: index = 2
        case .mark: index = 3
        case .thinking: index = 4
        case .nameLocation, .setupProgress, .finishInBrowser: return nil
        }
        return JournalProgressInfo(
            index: index,
            count: 7,
            label: JournalFirstRunCopy.stepProgress(index)
        )
    }

    static func canGoBackFromFolder(
        route: JournalFirstRunRoute,
        userOperationInFlight: Bool,
        pendingOperation: JournalRitualOperationToken,
        outbound: JournalOutboundToken,
        hasRefusal: Bool,
        handoffError: String?
    ) -> Bool {
        guard route == .ritual(.folder) else { return false }
        guard !userOperationInFlight else { return false }
        guard pendingOperation == .none else { return false }
        guard outbound == .none else { return false }
        guard !hasRefusal else { return false }
        guard handoffError == nil || handoffError?.isEmpty == true else { return false }
        return true
    }

    static func speechDownloadRow(
        job: JournalSetupState.Job?,
        copy: JournalSetupCopy.Downloads,
        speechFailure: String?,
        locked: Bool,
        downloadLabel: (JournalDownloadJobState) -> String
    ) -> JournalDownloadRowModel? {
        guard let job else { return nil }
        guard job.state == .downloading || job.state == .error else { return nil }

        let detail: String?
        let fraction: Double?
        let indeterminate: Bool

        if job.state == .downloading {
            let formatter = ByteCountFormatter()
            formatter.countStyle = .file
            if let total = job.bytesTotal, total > 0 {
                let doneStr = formatter.string(fromByteCount: Int64(clamping: job.bytesDone))
                let totalStr = formatter.string(fromByteCount: Int64(clamping: total))
                detail = "\(doneStr) of \(totalStr)"
                fraction = min(1.0, Double(job.bytesDone) / Double(total))
                indeterminate = false
            } else {
                detail = formatter.string(fromByteCount: Int64(clamping: job.bytesDone))
                fraction = nil
                indeterminate = true
            }
        } else {
            fraction = nil
            indeterminate = false
            if let failure = speechFailure, !failure.isEmpty {
                detail = failure
            } else if let errorDetail = job.error?.detail, !errorDetail.isEmpty {
                detail = errorDetail
            } else {
                detail = downloadLabel(job.state)
            }
        }

        let actionTitle: String?
        let actionIdentifier: String?
        let actionKind: JournalDownloadActionKind?

        if job.state == .error {
            actionTitle = copy.retry
            actionIdentifier = AXID.Journal.Ritual.downloadSpeechRetry
            actionKind = .retrySpeech
        } else {
            actionTitle = nil
            actionIdentifier = nil
            actionKind = nil
        }

        return JournalDownloadRowModel(
            name: copy.speech,
            detail: detail,
            fraction: fraction,
            indeterminate: indeterminate,
            actionTitle: actionTitle,
            actionEnabled: !locked,
            actionIdentifier: actionIdentifier,
            actionKind: actionKind
        )
    }

    static func thinkingDownloadRow(
        job: JournalSetupState.Job?,
        copy: JournalSetupCopy.Downloads,
        thinkingFailure: String?,
        operationError: JournalRitualOperationToken,
        locked: Bool,
        downloadLabel: (JournalDownloadJobState) -> String
    ) -> JournalDownloadRowModel? {
        guard let job else { return nil }
        guard job.state == .downloading || job.state == .error || job.state == .cancelled else { return nil }

        let detail: String?
        let fraction: Double?
        let indeterminate: Bool

        if job.state == .downloading {
            let formatter = ByteCountFormatter()
            formatter.countStyle = .file
            if let total = job.bytesTotal, total > 0 {
                let doneStr = formatter.string(fromByteCount: Int64(clamping: job.bytesDone))
                let totalStr = formatter.string(fromByteCount: Int64(clamping: total))
                detail = "\(doneStr) of \(totalStr)"
                fraction = min(1.0, Double(job.bytesDone) / Double(total))
                indeterminate = false
            } else {
                detail = formatter.string(fromByteCount: Int64(clamping: job.bytesDone))
                fraction = nil
                indeterminate = true
            }
        } else if job.state == .cancelled {
            fraction = nil
            indeterminate = false
            if operationError == .thinkingDownload, let failure = thinkingFailure, !failure.isEmpty {
                detail = failure
            } else {
                detail = downloadLabel(.cancelled)
            }
        } else {
            fraction = nil
            indeterminate = false
            if operationError == .thinkingDownload, let failure = thinkingFailure, !failure.isEmpty {
                detail = failure
            } else if let errorDetail = job.error?.detail, !errorDetail.isEmpty {
                detail = errorDetail
            } else {
                detail = downloadLabel(job.state)
            }
        }

        let actionTitle: String?
        let actionIdentifier: String?
        let actionKind: JournalDownloadActionKind?

        let canCancel = job.state == .downloading && (job.attemptID?.isEmpty == false)
        if canCancel {
            actionTitle = copy.cancel
            actionIdentifier = AXID.Journal.Ritual.downloadThinkingCancel
            actionKind = .cancelThinking
        } else if job.state == .error || job.state == .cancelled || operationError == .thinkingDownload {
            actionTitle = copy.retry
            actionIdentifier = AXID.Journal.Ritual.downloadThinkingRetry
            actionKind = .retryThinking
        } else {
            actionTitle = nil
            actionIdentifier = nil
            actionKind = nil
        }

        return JournalDownloadRowModel(
            name: copy.thinking,
            detail: detail,
            fraction: fraction,
            indeterminate: indeterminate,
            actionTitle: actionTitle,
            actionEnabled: !locked,
            actionIdentifier: actionIdentifier,
            actionKind: actionKind
        )
    }

    static func filteredThinkingChoices(
        choices: [JournalSetupCopy.Choice],
        offersLocalThinking: Bool
    ) -> [JournalSetupCopy.Choice] {
        choices.filter { $0.id != JournalThinkingChoice.local.rawValue || offersLocalThinking }
    }

    static func localChoiceRow(
        downloadBytes: UInt64?,
        sizeUnknownText: String
    ) -> (label: String, subLabel: String, description: String) {
        let sizeText: String
        if let bytes = downloadBytes, bytes > 0 {
            let formatter = ByteCountFormatter()
            formatter.countStyle = .file
            sizeText = formatter.string(fromByteCount: Int64(clamping: bytes))
        } else {
            sizeText = sizeUnknownText
        }
        return (
            label: JournalFirstRunCopy.onThisMac,
            subLabel: sizeText,
            description: JournalFirstRunCopy.onThisMacDescription
        )
    }

    static func shouldShowChooseLater(
        offersLocalThinking: Bool,
        displayedChoice: JournalThinkingChoice?
    ) -> Bool {
        !offersLocalThinking || displayedChoice == .later
    }
}
