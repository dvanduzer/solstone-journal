// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import Foundation

// Compile into a test harness only. This file and localization declarations are never app resources.
enum AppleLocaleCanary {
    static func verify(language: String, region: String, bundle: Bundle) throws {
        let resolved = Locale.current.identifier.replacingOccurrences(of: "-", with: "_")
        let expected = "\(language)_\(region)"
        guard resolved == expected else {
            throw NSError(domain: "requested \(expected), process resolved \(resolved)", code: 1)
        }
        guard let declared = bundle.infoDictionary?["CFBundleLocalizations"] as? [String], declared.contains(language) else {
            throw NSError(domain: "test bundle does not declare requested localization \(language)", code: 2)
        }
        guard bundle.preferredLocalizations.first == language else {
            throw NSError(domain: "bundle resolved \(bundle.preferredLocalizations), requested \(language)", code: 3)
        }
        FileHandle.standardError.write(Data("locale-canary: process=\(resolved) bundle=\(bundle.preferredLocalizations)\n".utf8))
    }
}
