// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import MacTestSupport
import Testing
@testable import journal

@Suite("JournalSetupClient", .serialized)
struct JournalSetupClientTests {
    @Test func nullByteFieldsStayNilAndCompleteStaysComplete() async throws {
        let store = ObserverURLProtocolStore()
        store.enqueue(body: stateBody(downloadBytes: "null", bytesTotal: "null", speechState: "complete"))
        let client = makeClient(store: store)

        let state = try await client.state()

        #expect(state.thinking.downloadBytes == nil)
        #expect(state.downloads.speech.bytesTotal == nil)
        #expect(state.downloads.thinking.bytesTotal == nil)
        #expect(state.downloads.speech.bytesDone == 0)
        #expect(state.mark.state == .proposed)
        #expect(state.downloads.speech.state == .complete)
        #expect(state.schema == JournalSetupClient.successSchema)
    }

    @Test func redirectStatusFails() async throws {
        let store = ObserverURLProtocolStore()
        store.enqueue(statusCode: 302, body: stateBody())
        let client = makeClient(store: store)

        await #expect(throws: JournalSetupClientError.redirect) {
            try await client.state()
        }
    }

    @Test func responseURLDifferentFromRequestFails() async throws {
        let client = makeMismatchedClient()

        await #expect(throws: JournalSetupClientError.unexpectedResponseURL) {
            try await client.state()
        }
    }

    @Test func unknownMarkJobAndCapabilityFailWhileNullBytesStayNull() async throws {
        let proposed = ObserverURLProtocolStore()
        proposed.enqueue(body: stateBody(markState: "proposed"))
        let proposedState = try await makeClient(store: proposed).state()
        #expect(proposedState.mark.state == .proposed)
        #expect(proposedState.thinking.downloadBytes == nil)
        #expect(proposedState.downloads.speech.bytesTotal == nil)

        let proposal = ObserverURLProtocolStore()
        proposal.enqueue(body: stateBody(markState: "proposal"))
        await #expect(throws: JournalSetupClientError.decoding) {
            try await makeClient(store: proposal).state()
        }

        let job = ObserverURLProtocolStore()
        job.enqueue(body: stateBody(speechState: "paused"))
        await #expect(throws: JournalSetupClientError.decoding) {
            try await makeClient(store: job).state()
        }

        let capability = ObserverURLProtocolStore()
        capability.enqueue(body: stateBody(capability: "maybe"))
        await #expect(throws: JournalSetupClientError.decoding) {
            try await makeClient(store: capability).state()
        }
    }

    @Test func wrongSchemaFails() async throws {
        let store = ObserverURLProtocolStore()
        store.enqueue(body: stateBody(schema: "solstone.other"))
        let client = makeClient(store: store)

        await #expect(throws: JournalSetupClientError.wrongSchema) {
            try await client.state()
        }
    }

    @Test func errorEnvelopeExposesAdmissionAndConflict() async throws {
        let admission = ObserverURLProtocolStore()
        admission.enqueue(statusCode: 503, body: """
        {"error":"pending","reason_code":"speech_admission_pending","detail":"wait"}
        """)
        let conflict = ObserverURLProtocolStore()
        conflict.enqueue(statusCode: 409, body: """
        {"error":"conflict","reason_code":"proposal_conflict","detail":"stale"}
        """)

        await #expect(throws: JournalSetupClientError.server(.init(
            error: "pending",
            reasonCode: "speech_admission_pending",
            detail: "wait"
        ))) {
            try await makeClient(store: admission).state()
        }
        await #expect(throws: JournalSetupClientError.server(.init(
            error: "conflict",
            reasonCode: "proposal_conflict",
            detail: "stale"
        ))) {
            try await makeClient(store: conflict).perform(.regenerateMark)
        }
    }

    private func makeClient(store: ObserverURLProtocolStore) -> JournalSetupClient {
        JournalSetupClient(session: URLSession(configuration: observerURLProtocolConfiguration(store: store)))
    }

    private func makeMismatchedClient() -> JournalSetupClient {
        SetupMismatchURL.shared.value = URL(string: "http://127.0.0.1:5015/elsewhere")!
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [SetupMismatchProtocol.self]
        return JournalSetupClient(session: URLSession(configuration: configuration))
    }

    private func stateBody(
        schema: String = "solstone.setup-state.v1",
        downloadBytes: String = "null",
        bytesTotal: String = "null",
        speechState: String = "complete",
        markState: String = "proposed",
        capability: String = "ok"
    ) -> String {
        """
        {"schema":"\(schema)","journal_path":"/journal","done":false,"copy":{"heading":"ignored"},"mark":{"render":null,"state":"\(markState)","proposal_id":"proposal-1"},"thinking":{"choice":null,"saved":false,"capability":"\(capability)","download_bytes":\(downloadBytes)},"downloads":{"speech":{"state":"\(speechState)","attempt_id":null,"bytes_done":0,"bytes_total":\(bytesTotal)},"thinking":{"state":"idle","attempt_id":null,"bytes_done":0,"bytes_total":null}}}
        """
    }
}

private final class SetupMismatchURL: @unchecked Sendable {
    static let shared = SetupMismatchURL()
    private let lock = NSLock()
    private var stored = URL(string: "http://127.0.0.1:5015/elsewhere")!

    var value: URL {
        get { lock.withLock { stored } }
        set { lock.withLock { stored = newValue } }
    }
}

private final class SetupMismatchProtocol: URLProtocol, @unchecked Sendable {
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        let response = HTTPURLResponse(
            url: SetupMismatchURL.shared.value,
            statusCode: 200,
            httpVersion: nil,
            headerFields: ["Content-Type": "application/json"]
        )!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data("{\"schema\":\"solstone.setup-state.v1\"}".utf8))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}
