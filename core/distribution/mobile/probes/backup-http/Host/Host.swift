// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import Darwin
import Foundation
import SwiftUI

func runProbe() {
  let files = FileManager.default
  let documents = files.urls(for: .documentDirectory, in: .userDomainMask)[0]
  let credential = documents.appendingPathComponent("http-credential.json")
  var result: [String: Any] = ["passed": false, "failed_stage": "fixture_read"]
  let began = ProcessInfo.processInfo.systemUptime
  let before = http_probe_footprint()
  do {
    var fixture = try Data(contentsOf: credential)
    guard fixture.count > 0 && fixture.count <= 32768 else { throw CocoaError(.fileReadTooLarge) }
    var excluded = credential
    var resource = URLResourceValues(); resource.isExcludedFromBackup = true
    try excluded.setResourceValues(resource)
    try files.setAttributes([.posixPermissions: 0o600, .protectionKey: FileProtectionType.completeUntilFirstUserAuthentication], ofItemAtPath: credential.path)
    defer { fixture.resetBytes(in: fixture.indices) }
    let pointer = fixture.withUnsafeMutableBytes { (bytes: UnsafeMutableRawBufferPointer) in
      restic_bridge_http_probe(bytes.baseAddress?.assumingMemoryBound(to: CChar.self), UInt32(bytes.count))
    }
    guard let pointer else { throw CocoaError(.coderInvalidValue) }
    defer { restic_bridge_http_probe_free(pointer) }
    result = try JSONSerialization.jsonObject(with: Data(String(cString: pointer).utf8)) as! [String: Any]
  } catch { }
  do { if files.fileExists(atPath: credential.path) { try files.removeItem(at: credential) } }
  catch { result["passed"] = false; result["failed_stage"] = "fixture_remove" }
  result["credential_file_absent"] = !files.fileExists(atPath: credential.path)
  result["pid"] = getpid()
  result["seconds"] = ProcessInfo.processInfo.systemUptime - began
  result["footprint_before_bytes"] = before
  result["footprint_after_bytes"] = http_probe_footprint()
  result["utc"] = ISO8601DateFormatter().string(from: Date())
  do {
    let encoded = try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys]) + Data([10])
    try encoded.write(to: documents.appendingPathComponent("http-evidence.json"), options: .atomic)
    try FileHandle.standardOutput.write(contentsOf: encoded)
    fflush(stdout)
  } catch { _exit(2) }
  _exit(result["passed"] as? Bool == true && result["credential_file_absent"] as? Bool == true ? 0 : 1)
}

@main struct HTTPProbeApp: App {
  var body: some Scene {
    WindowGroup {
      Color.black.task {
        DispatchQueue.global(qos: .userInitiated).async { runProbe() }
      }
    }
  }
}
