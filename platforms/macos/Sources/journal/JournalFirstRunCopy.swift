// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

enum JournalFirstRunCopy {
    static let nameLocationTitle = "let's create your journal on this mac"
    static let adoptLandingLine = "nothing moved. your journal was always here. you know it by its mark."

    static let locationField = "location"
    static let chooseLocation = "choose"
    static let continueButton = "continue"
    static let setupTitle = "setting up your journal"
    static let setupSubtitle = "this can take a minute."
    static let finishInBrowserTitle = "finish setting up in your browser"
    static let finishInBrowserLine = "this window moves on when you finish."
    static let reopenInBrowserButton = "open the setup page"
    static let adoptTitle = "journal found"
    static let adoptOpening = "opening this journal"
    static let adoptFailed = "couldn't open this journal"
    static let tryAgain = "try again"
    static let setupProgressNotSaved = "setup progress couldn't be saved."
    static let loginItemChangeFailed = "start at login couldn't be changed."
    static let loginItemUnavailable = "start at login isn't available from this app location."
    static let handoffHeldPairing = "the solstone app is already connected to a journal. open the solstone app to change that connection, or open this journal here."
    static let handoffCompetingSetup = "the solstone app is already finishing another setup. finish it there, or open this journal here."
    static let handoffPending = "setup hasn't continued in the solstone app. open it to continue, or try again here."
    static let openYourJournal = "open your journal"
    static let handoffNotDelivered = "couldn't continue setup in the solstone app."
    static let thinkingFailed = "couldn't finish this step. try again."

    static let all = [
        nameLocationTitle,
        adoptLandingLine,
        locationField,
        chooseLocation,
        continueButton,
        setupTitle,
        setupSubtitle,
        finishInBrowserTitle,
        finishInBrowserLine,
        reopenInBrowserButton,
        adoptTitle,
        adoptOpening,
        adoptFailed,
        tryAgain,
        setupProgressNotSaved,
        loginItemChangeFailed,
        loginItemUnavailable,
        handoffNotDelivered,
        handoffHeldPairing,
        handoffCompetingSetup,
        handoffPending,
        openYourJournal,
        thinkingFailed,
    ]
}
