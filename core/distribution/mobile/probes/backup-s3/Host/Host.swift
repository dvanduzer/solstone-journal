// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import CryptoKit
import Darwin
import Foundation
import Security
import SwiftUI

struct ProbeError: Error {
  let reason: String
  let retiredHandle: UInt64?
  init(reason: String, retiredHandle: UInt64? = nil) { self.reason = reason; self.retiredHandle = retiredHandle }
}
final class Probe: @unchecked Sendable {
  let files = FileManager.default
  let documents: URL = {
    let url = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    guard let pointer = realpath(url.path, nil) else { fatalError("documents_realpath_failed") }
    defer { free(pointer) }
    return URL(fileURLWithPath: String(cString: pointer))
  }()
  var root: URL { documents.appendingPathComponent("remote-backup-foundation-v1") }
  var ownerState: URL!
  var configuration: [String: String] = [:]
  var runNonce = ""
  var fixtureSHA = ""
  var expectedFixtureSHA = ""
  var phase = 0
  var inputDirectory: URL { documents.appendingPathComponent("remote-input") }
  var source: URL { root.appendingPathComponent("source") }
  var daily: String { configuration["daily_password"]! }
  var recovery: String { configuration["recovery_password"]! }
  func require(_ condition: @autoclosure () -> Bool, _ reason: String) throws {
    if !condition() { throw ProbeError(reason: reason) }
  }
  func log(_ event: String, _ value: Any) {
    do {
      let row: [String: Any] = ["event": event, "pid": getpid(), "nonce": runNonce, "fixture_sha256": fixtureSHA, "phase": phase, "time": Date().timeIntervalSince1970,
                                "uptime": ProcessInfo.processInfo.systemUptime, "footprint_bytes": backup_probe_footprint(), "value": value]
      let data = try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys]) + Data([10])
      let output = documents.appendingPathComponent("backup-s3-evidence.jsonl")
      if !files.fileExists(atPath: output.path) { files.createFile(atPath: output.path, contents: nil) }
      let stream = try FileHandle(forWritingTo: output)
      defer { try? stream.close() }
      try stream.seekToEnd(); try stream.write(contentsOf: data)
      if event == "kill.write.live" { try stream.synchronize() }
      try FileHandle.standardOutput.write(contentsOf: data)
    } catch { print("evidence_write_failed"); fflush(stdout); _exit(2) }
  }
  func metadataWorkspace() throws -> URL {
    let cache = files.urls(for: .cachesDirectory, in: .userDomainMask)[0]
    var workspace = cache.appendingPathComponent("callable-backup-metadata-v1")
    try files.createDirectory(at: workspace, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    var values = URLResourceValues(); values.isExcludedFromBackup = true
    try workspace.setResourceValues(values)
    let observed = try workspace.resourceValues(forKeys: [.isExcludedFromBackupKey])
    try require(observed.isExcludedFromBackup == true, "metadata_workspace_backup_exclusion_failed")
    guard let pointer = realpath(workspace.path, nil) else { throw ProbeError(reason: "metadata_workspace_realpath_failed") }
    defer { free(pointer) }
    workspace = URL(fileURLWithPath: String(cString: pointer))
    let workspaceDevice = workspace.path.withCString { backup_probe_device($0) }
    let sourceDevice = source.path.withCString { backup_probe_device($0) }
    try require(workspaceDevice != 0 && sourceDevice == workspaceDevice, "metadata_workspace_volume_mismatch")
    log("metadata.workspace", ["path": workspace.path, "excluded_from_backup": true,
        "workspace_device": workspaceDevice, "source_device": sourceDevice])
    return workspace
  }
  func validID(_ value: String) -> Bool {
    value.utf8.count == 64 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
  }
  func prepare(fresh: Bool) throws {
    ownerState = documents.deletingLastPathComponent().appendingPathComponent("Library/Application Support/callable-s3-owner-v1")
    let code = ownerState.path.withCString { owner in
      inputDirectory.path.withCString { input in backup_s3_prepare_roots(owner, input, fresh ? 1 : 0) }
    }
    try require(code == 0, "root_policy_or_census_refused")
    log("owner.root-policy", ["excluded_from_backup": true, "protection": FileProtectionType.complete.rawValue,
        "held_nofollow_roots": true, "fresh_empty_census": fresh])
  }
  func loadFixture() throws {
    var length: UInt32 = 0
    var code: Int32 = -1
    guard let pointer = backup_s3_load_fixture(&length, &code) else { throw ProbeError(reason: "fixture_custody_refused") }
    defer { backup_s3_fixture_free(pointer) }
    try require(code == 0 && length > 0 && length <= 32768, "fixture_custody_refused")
    var bytes = Data(bytes: pointer, count: Int(length))
    defer { bytes.resetBytes(in: bytes.indices) }
    fixtureSHA = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
    try require(fixtureSHA == expectedFixtureSHA, "fixture_commitment_refused")
    guard let value = try JSONSerialization.jsonObject(with: bytes) as? [String: String] else { throw ProbeError(reason: "fixture_shape_refused") }
    let required: Set<String> = ["endpoint", "region", "bucket", "prefix", "access_key", "secret_key", "daily_password", "recovery_password"]
    try require(required.isSubset(of: Set(value.keys)) && Set(value.keys).isSubset(of: required.union(["session_token"])) && required.allSatisfy { !(value[$0] ?? "").isEmpty }, "fixture_fields_refused")
    configuration = value
    log("fixture.bound", ["sha256": fixtureSHA, "endpoint": value["endpoint"]!, "region": value["region"]!,
        "bucket": value["bucket"]!, "prefix": value["prefix"]!, "access_key_id": value["access_key"]!,
        "held_nofollow_bounded_read": true])
  }
  func start(_ op: Int, password: String? = nil, restore: URL? = nil, snapshot: String = "") throws -> UInt64 {
    let workspacePath = op == 4 ? try metadataWorkspace().path : ""
    var request: [String: Any] = [:]
    for name in ["endpoint", "region", "bucket", "prefix", "access_key", "secret_key", "session_token"] {
      request[name] = configuration[name] ?? ""
    }
    request["operation"] = op
    request["password"] = op == 8 ? "" : password ?? daily
    request["recovery_password"] = (op == 1 || op == 3) ? recovery : ""
    request["state_path"] = ownerState.path
    request["source_path"] = (op == 4 || op == 6) ? source.path : ""
    request["restore_path"] = restore?.path ?? ""
    request["snapshot_id"] = snapshot
    request["workspace_path"] = workspacePath
    var encoded = try JSONSerialization.data(withJSONObject: request)
    defer { encoded.resetBytes(in: encoded.indices) }
    var code: Int32 = -1
    encoded.append(0)
    let id = encoded.withUnsafeBytes { raw in backup_probe_start(raw.baseAddress!.assumingMemoryBound(to: CChar.self), &code) }
    try require(code == 0 && id != 0, "start_refused_\(code)")
    return id
  }
  func wait(_ id: UInt64) async throws -> [String: Any] {
    try await withCheckedThrowingContinuation { continuation in
      DispatchQueue.global(qos: .userInitiated).async {
        var waitCode: Int32 = -1
        let pointer = backup_probe_wait(id, &waitCode)
        defer { if let pointer { backup_probe_json_free(pointer) } }
        let freeCode = waitCode == 0 ? backup_probe_free(id) : -1
        do {
          try self.require(waitCode == 0 && freeCode == 0, "wait_or_free_refused")
          guard let pointer else { throw ProbeError(reason:"wait_adapter_allocation_failed") }
          guard var value = try JSONSerialization.jsonObject(with: Data(String(cString: pointer).utf8)) as? [String: Any] else {
            throw ProbeError(reason:"wait_adapter_shape_failed")
          }
          try self.require(value["wait_code"] as? Int == Int(waitCode), "wait_evidence_mismatch")
          try self.require(value["adapter_overflow"] == nil && value["adapter_cleanup_failed"] == nil, "result_adapter_failed")
          value["free_code"] = freeCode
          continuation.resume(returning: value)
        } catch {
          self.log("handle.wait-failed", ["handle":id, "actual_wait_code":waitCode, "actual_free_code":freeCode,
            "terminal_confirmed":waitCode == 0, "retired":waitCode == 0 && freeCode == 0])
          continuation.resume(throwing: ProbeError(reason:(error as? ProbeError)?.reason ?? "WAIT_ADAPTER_FAILED",
            retiredHandle:waitCode == 0 && freeCode == 0 ? id : nil))
        }
      }
    }
  }
  func call(_ op: Int, _ label: String, password: String? = nil, restore: URL? = nil,
            snapshot: String = "", expectedStatus: Int = 0) async throws -> [String: Any] {
    let began = ProcessInfo.processInfo.systemUptime
    let id = try start(op, password: password, restore: restore, snapshot: snapshot)
    log(label + ".started", ["handle": id])
    let result = try await wait(id)
    log(label, ["handle": id, "seconds": ProcessInfo.processInfo.systemUptime - began, "result": result])
    try require(result["status"] as? Int == expectedStatus, label + "_unexpected_status")
    return result
  }
  func hashes(_ directory: URL) throws -> [String: String] {
    var result: [String: String] = [:]
    guard let iterator = files.enumerator(at: directory, includingPropertiesForKeys: [.isRegularFileKey, .isDirectoryKey, .isSymbolicLinkKey]) else {
      throw ProbeError(reason: "enumeration_failed")
    }
    for case let file as URL in iterator {
      let relative = String(file.path.dropFirst(directory.path.count + 1))
      let attrs = try file.resourceValues(forKeys: [.isRegularFileKey, .isDirectoryKey, .isSymbolicLinkKey])
      if attrs.isSymbolicLink == true {
        result[relative] = "symlink:" + (try files.destinationOfSymbolicLink(atPath: file.path))
      } else if attrs.isRegularFile == true {
        result[relative] = SHA256.hash(data: try Data(contentsOf: file)).map { String(format: "%02x", $0) }.joined()
      } else if attrs.isDirectory == true { result[relative] = "directory" }
    }
    return result
  }
  func verifyRestore(_ destination: URL, expected: [String: String], label: String) throws {
    let restored = destination.appendingPathComponent(source.lastPathComponent)
    let actual = try hashes(restored)
    try require(actual == expected, label + "_byte_mismatch")
    let metadataPath = restored.appendingPathComponent("sample.bin").path
    try require(metadataPath.withCString { backup_probe_check_fixture_metadata($0) } == 0, label + "_metadata_mismatch")
    log(label + ".metadata", ["mode": "0640", "mtime_seconds": 1700000000, "xattr_verified": true, "symlinks_verified": true])
    log(label, ["verified_entries": actual.count, "source_unchanged": try hashes(source)])
  }
  func first() async throws {
    try require(!files.fileExists(atPath: root.path), "fixture_already_exists")
    try files.createDirectory(at: source.appendingPathComponent("notes"), withIntermediateDirectories: true)
    try files.createDirectory(at: source.appendingPathComponent("empty"), withIntermediateDirectories: true)
    try Data("Synthetic copper kite\n".utf8).write(to: source.appendingPathComponent("notes/café.txt"))
    try Data((0..<16384).map { UInt8($0 % 256) }).write(to: source.appendingPathComponent("sample.bin"))
    log("fixture.paths", ["documents": documents.path, "root": root.path, "owner_state": ownerState.path, "source": source.path])
    try files.createSymbolicLink(atPath: source.appendingPathComponent("note-link").path, withDestinationPath: "notes/café.txt")
    try files.createSymbolicLink(atPath: source.appendingPathComponent("outside-link").path, withDestinationPath: "../outside-owner-fixture.txt")
    try Data("Synthetic outside source fixture\n".utf8).write(to: root.appendingPathComponent("outside-owner-fixture.txt"))
    let metadataPath = source.appendingPathComponent("sample.bin").path
    try require(metadataPath.withCString { backup_probe_set_fixture_metadata($0) } == 0, "fixture_metadata_set_failed")
    let original = try hashes(source)
    let empty = try await call(8, "owner.inspect-new")
    try require(empty["diagnostic"] as? String == "S3_STATE_NEW", "new_owner_category_changed")
    let initResult = try await call(1, "init")
    let dailyOpen = try await call(2, "daily.open")
    let dailyID = initResult["key_id"] as? String ?? ""
    try require(validID(dailyID) && dailyOpen["key_id"] as? String == dailyID, "daily_key_identity_mismatch")
    let added = try await call(3, "recovery.add")
    let recoveryOnly = try await call(2, "recovery.independent-open", password: recovery)
    let recoveryID = added["key_id"] as? String ?? ""
    try require(validID(recoveryID) && recoveryID != dailyID && recoveryOnly["key_id"] as? String == recoveryID, "recovery_key_identity_mismatch")
    let wrong = try await call(2, "wrong-key", password: "synthetic-wrong-password", expectedStatus: 2)
    try require(wrong["diagnostic"] as? String == "AUTH_FAILED", "wrong_key_reason_mismatch")
    let refusedInit = try await call(1, "existing-init.refused", expectedStatus: 2)
    try require(refusedInit["diagnostic"] as? String == "REPO_EXISTS", "existing_init_reason_changed")
    let first = try await call(4, "original.snapshot")
    try Data("Synthetic changed copper kite\n".utf8).write(to: source.appendingPathComponent("notes/café.txt"))
    try Data("Synthetic second file\n".utf8).write(to: source.appendingPathComponent("second.txt"))
    let changed = try hashes(source)
    let second = try await call(4, "changed.snapshot")
    guard let firstID = first["snapshot_id"] as? String, let secondID = second["snapshot_id"] as? String else {
      throw ProbeError(reason: "missing_snapshot_id")
    }
    try require(validID(firstID) && validID(secondID) && firstID != secondID, "snapshot_identity_invalid")
    let listed = try await call(5, "snapshots.list", password: recovery)
    try require(Set(listed["snapshot_ids"] as? [String] ?? []) == Set([firstID, secondID]), "snapshot_list_mismatch")
    let restore1 = root.appendingPathComponent("restore-original")
    let restore2 = root.appendingPathComponent("restore-changed")
    _ = try await call(6, "original.restore", password: recovery, restore: restore1, snapshot: firstID)
    try verifyRestore(restore1, expected: original, label: "original.bytes")
    _ = try await call(6, "changed.restore", password: recovery, restore: restore2, snapshot: secondID)
    try verifyRestore(restore2, expected: changed, label: "changed.bytes")
    let beforeRefusal = try hashes(restore2)
    var refused = false
    var unexpectedHandle: UInt64?
    do { unexpectedHandle = try start(6, password: recovery, restore: restore2, snapshot: secondID) }
    catch let error as ProbeError where error.reason == "start_refused_1" {
      refused = true; log("nonempty.restore.refused", ["observed": true])
    }
    if let id = unexpectedHandle { _ = backup_probe_cancel(id); _ = try await wait(id) }
    try require(refused, "nonempty_restore_admitted")
    let afterRefusal = try hashes(restore2)
    try require(beforeRefusal == afterRefusal, "nonempty_restore_modified_destination")
    let sourceAfterRestore = try hashes(source)
    try require(sourceAfterRestore == changed, "restore_changed_original_source")
    _ = try await call(7, "full-integrity", password: recovery)
    let state: [String: Any] = ["first_pid": getpid(), "fixture_sha256": fixtureSHA, "qualification_nonce": runNonce, "first_snapshot_id": firstID, "daily_key_id": dailyID,
                                "recovery_key_id": recoveryID, "snapshot_id": secondID, "expected": changed]
    try JSONSerialization.data(withJSONObject: state, options: [.sortedKeys]).write(to: root.appendingPathComponent("fixture-state.json"), options: .atomic)
    log("phase.finished", ["phase": 1, "passed": true])
  }
  func resumed() async throws {
    _ = try metadataWorkspace()
    let inspected = try await call(8, "restart.owner-inspect")
    try require(inspected["diagnostic"] as? String == "S3_STATE_READY", "restart_owner_not_ready")
    let state = try JSONSerialization.jsonObject(with: Data(contentsOf: root.appendingPathComponent("fixture-state.json"))) as! [String: Any]
    try require(state["first_pid"] as? Int != Int(getpid()), "restart_pid_reused")
    try require(state["fixture_sha256"] as? String == fixtureSHA && state["qualification_nonce"] as? String == runNonce, "restart_fixture_or_session_mismatch")
    let dailyOpen = try await call(2, "restart.daily-open")
    let recoveryOpen = try await call(2, "restart.recovery-open", password: recovery)
    let dailyID = state["daily_key_id"] as? String ?? ""
    let recoveryID = state["recovery_key_id"] as? String ?? ""
    try require(validID(dailyID) && validID(recoveryID) && dailyID != recoveryID && dailyOpen["key_id"] as? String == dailyID && recoveryOpen["key_id"] as? String == recoveryID,
        "restart_key_identity_mismatch")
    let listed = try await call(5, "restart.list", password: recovery)
    let firstID = state["first_snapshot_id"] as? String ?? ""
    let secondID = state["snapshot_id"] as? String ?? ""
    try require(validID(firstID) && validID(secondID) && firstID != secondID && Set(listed["snapshot_ids"] as? [String] ?? []) == Set([firstID, secondID]), "restart_snapshot_list_mismatch")
    let destination = root.appendingPathComponent("restore-after-restart")
    _ = try await call(6, "restart.restore", password: recovery, restore: destination, snapshot: state["snapshot_id"] as! String)
    try verifyRestore(destination, expected: state["expected"] as! [String: String], label: "restart.bytes")
    _ = try await call(7, "restart.full-integrity")
    log("phase.finished", ["phase": 2, "passed": true, "first_pid": state["first_pid"]!])
  }
  func run() async {
    do {
      let args = ProcessInfo.processInfo.arguments
      func argument(_ name: String) throws -> String {
        let indices = args.indices.filter { args[$0] == name }
        try require(indices.count == 1 && indices[0] + 1 < args.count, "run_argument_refused")
        return args[indices[0] + 1]
      }
      runNonce = try argument("--nonce")
      try require(runNonce.utf8.count == 32 && runNonce.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }, "run_nonce_refused")
      phase = args.contains("--prepare") ? 0 : (args.contains("--resume") ? 2 : 1)
      if args.contains("--lifecycle") {
        let selected = try argument("--lifecycle")
        try require(["3", "4", "5", "6", "7"].contains(selected) && !args.contains("--prepare") && !args.contains("--resume"), "lifecycle_argument_refused")
        phase = Int(selected)!
      }
      if phase != 0 {
        expectedFixtureSHA = try argument("--fixture-sha256")
        try require(validID(expectedFixtureSHA), "fixture_commitment_shape_refused")
      }
      guard let executable = Bundle.main.executableURL, let nativeSHA = Bundle.main.object(forInfoDictionaryKey: "BackupS3NativeSHA256") as? String else {
        throw ProbeError(reason: "build_binding_missing")
      }
      try require(validID(nativeSHA), "native_build_binding_refused")
      let executableSHA = SHA256.hash(data: try Data(contentsOf: executable)).map { String(format: "%02x", $0) }.joined()
      log("phase.started", ["phase": phase, "executable_sha256": executableSHA, "native_archive_sha256": nativeSHA])
      try prepare(fresh: phase == 0)
      if args.contains("--prepare") {
        log("phase.finished", ["phase": 0, "passed": true])
      } else {
        try loadFixture()
        if phase >= 3 { try await lifecycle() }
        else if args.contains("--resume") { try await resumed() } else { try await first() }
      }
      _exit(0)
    } catch {
      log("host.failed", ["error": (error as? ProbeError)?.reason ?? "HOST_FAILURE"])
      _exit(1)
    }
  }
}
@main struct BackupS3ProbeApp: App {
  @State private var started = false
  var body: some Scene {
    WindowGroup {
      Color.black.ignoresSafeArea().task {
        guard !started else { return }; started = true
        await Probe().run()
      }
    }
  }
}
