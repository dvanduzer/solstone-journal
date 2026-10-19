// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import Foundation

@main struct CanaryMain {
    static func main() {
      do {
        let env = ProcessInfo.processInfo.environment
        guard let language = env["I18N_TEST_LANGUAGE"], let region = env["I18N_TEST_REGION"],
              let path = env["I18N_TEST_BUNDLE"], let bundle = Bundle(path: path) else {
            throw NSError(domain: "set I18N_TEST_LANGUAGE, I18N_TEST_REGION and I18N_TEST_BUNDLE", code: 1)
        }
        try AppleLocaleCanary.verify(language: language, region: region, bundle: bundle)
      } catch {
        FileHandle.standardError.write(Data("\(error)\n".utf8))
        exit(1)
      }
    }
}
