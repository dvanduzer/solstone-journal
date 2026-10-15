// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import CryptoKit
import Darwin
import Foundation
import SwiftUI

@main struct BackupS3OwnerPolicyProbeApp: App {
  @State private var started = false
  func run() {
    let arguments = ProcessInfo.processInfo.arguments
    let indices = arguments.indices.filter { arguments[$0] == "--nonce" }
    guard indices.count == 1, indices[0] + 1 < arguments.count else { _exit(1) }
    let nonce = arguments[indices[0] + 1]
    guard nonce.utf8.count == 32, nonce.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
      let executable = Bundle.main.executableURL,
      let nativeSHA = Bundle.main.object(forInfoDictionaryKey: "BackupS3NativeSHA256") as? String,
      nativeSHA.utf8.count == 64 else { _exit(1) }
    do {
      let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
      guard let pointer = realpath(documents.path, nil) else { _exit(1) }
      let canonical = URL(fileURLWithPath: String(cString: pointer)); free(pointer)
      let base = canonical.appendingPathComponent("owner-policy-" + nonce)
      let began = ProcessInfo.processInfo.systemUptime
      guard let resultPointer = base.path.withCString({ backup_owner_policy_run($0) }) else { _exit(1) }
      defer { backup_owner_policy_free(resultPointer) }
      let result = try JSONSerialization.jsonObject(with: Data(String(cString: resultPointer).utf8)) as! [String: Any]
      let row: [String: Any] = ["nonce":nonce, "pid":getpid(), "time":Date().timeIntervalSince1970,
        "executable_sha256":SHA256.hash(data: try Data(contentsOf: executable)).map { String(format:"%02x", $0) }.joined(),
        "native_archive_sha256":nativeSHA, "qualification_kind":"private-owner-policy", "seconds":ProcessInfo.processInfo.systemUptime - began,
        "result":result]
      let bytes = try JSONSerialization.data(withJSONObject:row, options:[.sortedKeys]) + Data([10])
      try bytes.write(to:canonical.appendingPathComponent("owner-policy-evidence.json"), options:.atomic)
      try FileHandle.standardOutput.write(contentsOf:bytes)
      _exit(result["passed"] as? Bool == true ? 0 : 1)
    } catch { _exit(1) }
  }
  var body: some Scene {
    WindowGroup {
      Color.black.ignoresSafeArea().task {
        guard !started else { return }; started = true
        run()
      }
    }
  }
}
