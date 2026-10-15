// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import CoreML
import FluidAudio
import Foundation

/// Load installed files without FluidAudio's delete-and-download recovery.
/// Model acquisition and repair belong to `solstone journal install-models`,
/// whose downloader validates the compiled asset set and every redirect hop.
package func loadInstalledModels(from directory: URL, version: AsrModelVersion) throws -> AsrModels {
    let names = ModelNames.ASR.self
    let configuration = AsrModels.defaultConfiguration()

    func load(_ name: String, units: MLComputeUnits) throws -> MLModel {
        let config = MLModelConfiguration()
        config.computeUnits = units
        config.allowLowPrecisionAccumulationOnGPU = true
        return try MLModel(contentsOf: directory.appendingPathComponent(name), configuration: config)
    }

    let vocabularyData = try Data(contentsOf: directory.appendingPathComponent(names.vocabularyFile))
    let rawVocabulary = try JSONDecoder().decode([String: String].self, from: vocabularyData)
    var vocabulary: [Int: String] = [:]
    for (key, token) in rawVocabulary {
        if let index = Int(key) { vocabulary[index] = token }
    }

    return try AsrModels(
        encoder: load(names.encoderFile, units: configuration.computeUnits),
        preprocessor: load(names.preprocessorFile, units: .cpuOnly),
        decoder: load(names.decoderFile, units: configuration.computeUnits),
        joint: load(names.jointFile, units: configuration.computeUnits),
        configuration: configuration,
        vocabulary: vocabulary,
        version: version
    )
}
