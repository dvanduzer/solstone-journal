// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Testing
@testable import journal

@Suite("JournalSetupPeer")
struct JournalSetupPeerTests {
    @Test func evidenceAcceptsObserverIdentifierTeamAndCapability() {
        let output = "Identifier=app.solstone.observer\nTeamIdentifier=7QCG8V4M6H\n"
        #expect(journalSetupPeerEvidenceAccepts(
            combinedOutput: output,
            capabilities: ["setup-sequence-v1"]
        ))
        #expect(!journalSetupPeerEvidenceAccepts(combinedOutput: output, capabilities: nil))
        #expect(!journalSetupPeerEvidenceAccepts(combinedOutput: output, capabilities: ["other"]))
        #expect(!journalSetupPeerEvidenceAccepts(
            combinedOutput: "Identifier=app.solstone.other\nTeamIdentifier=7QCG8V4M6H\n",
            capabilities: ["setup-sequence-v1"]
        ))
    }
}
