// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import CryptoKit
import Darwin
import Foundation
import SwiftUI

enum RootedProbe {
  static func run() {
    let arguments = CommandLine.arguments
    guard arguments.count == 4, arguments[1] == "--run", arguments[2] == "--nonce",
      arguments[3].utf8.count == 32,
      arguments[3].utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else { return }
    let nonce = arguments[3]
    let files = FileManager.default
    let documents = files.urls(for: .documentDirectory, in: .userDomainMask)[0]
    guard let pointer = realpath(documents.path, nil) else { _exit(2) }
    let canonical = URL(fileURLWithPath: String(cString: pointer)); free(pointer)
    let evidence = canonical.appendingPathComponent("backup-rooted-evidence.jsonl")
    var identity: [String: Any] = ["nonce": nonce, "pid": getpid()]
    func log(_ event: String, _ value: [String: Any]) {
      do {
        var row = identity
        row["event"] = event; row["value"] = value
        row["time"] = Date().timeIntervalSince1970
        row["uptime"] = ProcessInfo.processInfo.systemUptime
        row["footprint_bytes"] = backup_rooted_footprint()
        let data = try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys]) + Data([10])
        if !files.fileExists(atPath: evidence.path) {
          guard files.createFile(atPath: evidence.path, contents: nil,
            attributes: [.posixPermissions: 0o600, .protectionKey: FileProtectionType.complete]) else { _exit(2) }
        }
        let stream = try FileHandle(forWritingTo: evidence)
        try stream.seekToEnd(); try stream.write(contentsOf: data); try stream.synchronize(); try stream.close()
        try FileHandle.standardOutput.write(contentsOf: data)
      } catch { _exit(2) }
    }
    do {
      guard let executable = Bundle.main.executableURL,
        let nativeSHA = Bundle.main.object(forInfoDictionaryKey: "BackupRootedNativeSHA256") as? String,
        nativeSHA.utf8.count == 64 else { throw CocoaError(.fileReadCorruptFile) }
      let attributes = try files.attributesOfItem(atPath: executable.path)
      guard let size = attributes[.size] as? NSNumber, size.uint64Value <= 128 * 1024 * 1024 else { throw CocoaError(.fileReadTooLarge) }
      let executableSHA = SHA256.hash(data: try Data(contentsOf: executable)).map { String(format: "%02x", $0) }.joined()
      identity["executable_sha256"] = executableSHA
      identity["native_archive_sha256"] = nativeSHA
      // These are observed build markers; independent controller receipts bind
      // authority. This standalone app installs no normal-runtime capability.
      var root = canonical.appendingPathComponent("rooted-local-" + nonce)
      guard !files.fileExists(atPath: root.path) else { throw CocoaError(.fileWriteFileExists) }
      try files.createDirectory(at: root, withIntermediateDirectories: false,
        attributes: [.posixPermissions: 0o700, .protectionKey: FileProtectionType.complete])
      var values = URLResourceValues(); values.isExcludedFromBackup = true
      try root.setResourceValues(values)
      let actual = try root.resourceValues(forKeys: [.isExcludedFromBackupKey])
      let policy = try files.attributesOfItem(atPath: root.path)
      guard actual.isExcludedFromBackup == true,
        policy[.protectionKey] as? FileProtectionType == .complete else { throw CocoaError(.fileWriteNoPermission) }
      log("phase.started", ["fixture_root": root.path, "excluded_from_backup": true,
        "protection": FileProtectionType.complete.rawValue])
      let code = root.path.withCString { backup_rooted_run($0) }
      log("phase.finished", ["caller_exit": code, "passed": code == 0,
        "boundary": "local exported C caller only; no provider or normal-app qualification"])
      _exit(code == 0 ? 0 : 1)
    } catch {
      log("phase.failed", ["reason": "local_probe_setup_failed"])
      _exit(1)
    }
  }
}

@main struct BackupRootedProbe: App {
  @State private var launched = false
  var body: some Scene {
    WindowGroup {
      Color.clear.task {
        guard !launched else { return }
        launched = true
        Task.detached(priority: .userInitiated) { RootedProbe.run() }
      }
    }
  }
}
