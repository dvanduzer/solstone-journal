// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import Foundation
import XCTest
@testable import InstalledModels

final class InstalledModelTests: XCTestCase {
    func testMissingModelDoesNotCreateAReplacement() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        XCTAssertThrowsError(try loadInstalledModels(from: root.appendingPathComponent("parakeet-tdt-0.6b-v3"), version: .v3))
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), [])
    }

    func testCorruptModelDoesNotDeleteInstalledFiles() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let modelRoot = root.appendingPathComponent("parakeet-tdt-0.6b-v3")
        let encoder = modelRoot.appendingPathComponent("Encoder.mlmodelc")
        try FileManager.default.createDirectory(at: encoder, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let vocabulary = Data(#"{"0":"hello"}"#.utf8)
        let corruptModel = Data("invalid compiled model".utf8)
        let incumbent = Data("keep the installed bytes".utf8)
        let vocabularyPath = modelRoot.appendingPathComponent("parakeet_vocab.json")
        let modelPath = encoder.appendingPathComponent("coremldata.bin")
        let incumbentPath = modelRoot.appendingPathComponent("incumbent")
        try vocabulary.write(to: vocabularyPath)
        try corruptModel.write(to: modelPath)
        try incumbent.write(to: incumbentPath)

        XCTAssertThrowsError(try loadInstalledModels(from: root.appendingPathComponent("parakeet-tdt-0.6b-v3"), version: .v3))
        XCTAssertEqual(try Data(contentsOf: vocabularyPath), vocabulary)
        XCTAssertEqual(try Data(contentsOf: modelPath), corruptModel)
        XCTAssertEqual(try Data(contentsOf: incumbentPath), incumbent)
    }
}
