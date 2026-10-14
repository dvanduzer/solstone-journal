// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import CryptoKit
import Darwin
import Foundation
import Security
import SwiftUI

struct ProbeError: Error { let reason: String }
final class CancelWitness: @unchecked Sendable {
  let lock = NSLock()
  var handle: UInt64 = 0
  var fired = false
  var cancelCode: Int32 = -1
  var freeCode: Int32 = -1
  func publish(_ id: UInt64) { lock.lock(); handle = id; lock.unlock() }
  func event() {
    lock.lock(); defer { lock.unlock() }
    guard handle != 0 && !fired else { return }
    fired = true
    cancelCode = backup_probe_cancel(handle)
    freeCode = backup_probe_free(handle)
  }
  func result() -> (Bool, Int32, Int32) {
    lock.lock(); defer { lock.unlock() }
    return (fired, cancelCode, freeCode)
  }
}
final class Probe: @unchecked Sendable {
  let files = FileManager.default
  let documents: URL = {
    let url = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    guard let pointer = realpath(url.path, nil) else { fatalError("documents_realpath_failed") }
    defer { free(pointer) }
    return URL(fileURLWithPath: String(cString: pointer))
  }()
  var root: URL { documents.appendingPathComponent("local-backup-foundation-v3") }
  var repo: URL { root.appendingPathComponent("repo") }
  var source: URL { root.appendingPathComponent("source") }
  let daily = "synthetic-daily-fixture-password"
  let recovery = "synthetic-distinct-recovery-fixture-password"
  func require(_ condition: @autoclosure () -> Bool, _ reason: String) throws {
    if !condition() { throw ProbeError(reason: reason) }
  }
  func log(_ event: String, _ value: Any) {
    do {
      let row: [String: Any] = ["event": event, "pid": getpid(), "time": Date().timeIntervalSince1970,
                                "uptime": ProcessInfo.processInfo.systemUptime, "footprint_bytes": backup_probe_footprint(), "value": value]
      let data = try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys]) + Data([10])
      let output = documents.appendingPathComponent("backup-evidence.jsonl")
      if !files.fileExists(atPath: output.path) { files.createFile(atPath: output.path, contents: nil) }
      let stream = try FileHandle(forWritingTo: output)
      defer { try? stream.close() }
      try stream.seekToEnd(); try stream.write(contentsOf: data)
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
    let repositoryDevice = repo.path.withCString { backup_probe_device($0) }
    try require(workspaceDevice != 0 && sourceDevice == workspaceDevice, "metadata_workspace_volume_mismatch")
    log("metadata.workspace", ["path": workspace.path, "excluded_from_backup": true,
        "workspace_device": workspaceDevice, "source_device": sourceDevice, "repository_device": repositoryDevice])
    return workspace
  }
  func start(_ op: Int, password: String? = nil, restore: URL? = nil, snapshot: String = "") throws -> UInt64 {
    let workspacePath: String
    if op == 4 { workspacePath = try metadataWorkspace().path } else { workspacePath = "" }
    let request: [String: Any] = ["workspace_path": workspacePath, "operation": op, "password": password ?? daily, "recovery_password": recovery,
                                  "repo_path": repo.path, "source_path": source.path,
                                  "restore_path": restore?.path ?? "", "snapshot_id": snapshot]
    let encoded = try JSONSerialization.data(withJSONObject: request)
    var code: Int32 = -1
    let id = String(decoding: encoded, as: UTF8.self).withCString { backup_probe_start($0, &code) }
    try require(code == 0 && id != 0, "start_refused_\(code)")
    return id
  }
  func wait(_ id: UInt64) async throws -> [String: Any] {
    try await withCheckedThrowingContinuation { continuation in
      DispatchQueue.global(qos: .userInitiated).async {
        guard let pointer = backup_probe_wait(id) else {
          continuation.resume(throwing: ProbeError(reason: "wait_allocation_failed")); return
        }
        defer { backup_probe_json_free(pointer) }
        do {
          let value = try JSONSerialization.jsonObject(with: Data(String(cString: pointer).utf8)) as! [String: Any]
          let freeCode = backup_probe_free(id)
          try self.require(value["wait_code"] as? Int == 0 && freeCode == 0, "wait_or_free_refused")
          try self.require(value["adapter_overflow"] == nil, "result_overflow")
          continuation.resume(returning: value)
        } catch { continuation.resume(throwing: error) }
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
    try files.createDirectory(at: repo, withIntermediateDirectories: false)
    try Data("Synthetic copper kite\n".utf8).write(to: source.appendingPathComponent("notes/café.txt"))
    try Data((0..<16384).map { UInt8($0 % 256) }).write(to: source.appendingPathComponent("sample.bin"))
    log("fixture.paths", ["documents": documents.path, "root": root.path, "repo": repo.path, "source": source.path])
    try files.createSymbolicLink(atPath: source.appendingPathComponent("note-link").path, withDestinationPath: "notes/café.txt")
    try files.createSymbolicLink(atPath: source.appendingPathComponent("outside-link").path, withDestinationPath: "../outside-owner-fixture.txt")
    try Data("Synthetic outside source fixture\n".utf8).write(to: root.appendingPathComponent("outside-owner-fixture.txt"))
    let metadataPath = source.appendingPathComponent("sample.bin").path
    try require(metadataPath.withCString { backup_probe_set_fixture_metadata($0) } == 0, "fixture_metadata_set_failed")
    let original = try hashes(source)
    let initResult = try await call(1, "init")
    _ = try await call(2, "daily.open")
    let added = try await call(3, "recovery.add")
    let recoveryOnly = try await call(2, "recovery.independent-open", password: recovery)
    try require(recoveryOnly["key_id"] as? String == added["key_id"] as? String, "recovery_key_identity_mismatch")
    let wrong = try await call(2, "wrong-key", password: "synthetic-wrong-password", expectedStatus: 2)
    try require(wrong["diagnostic"] as? String == "AUTH_FAILED", "wrong_key_reason_mismatch")
    let first = try await call(4, "original.snapshot")
    try Data("Synthetic changed copper kite\n".utf8).write(to: source.appendingPathComponent("notes/café.txt"))
    try Data("Synthetic second file\n".utf8).write(to: source.appendingPathComponent("second.txt"))
    let changed = try hashes(source)
    let second = try await call(4, "changed.snapshot")
    guard let firstID = first["snapshot_id"] as? String, let secondID = second["snapshot_id"] as? String else {
      throw ProbeError(reason: "missing_snapshot_id")
    }
    try require(firstID.count == 64 && secondID.count == 64 && firstID != secondID, "snapshot_identity_invalid")
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
    let state: [String: Any] = ["first_pid": getpid(), "daily_key_id": initResult["key_id"] ?? "",
                                "recovery_key_id": added["key_id"] ?? "", "snapshot_id": secondID, "expected": changed]
    try JSONSerialization.data(withJSONObject: state, options: [.sortedKeys]).write(to: root.appendingPathComponent("fixture-state.json"), options: .atomic)
    log("phase.finished", ["phase": 1, "passed": true])
  }
  func resumed() async throws {
    _ = try metadataWorkspace()
    let state = try JSONSerialization.jsonObject(with: Data(contentsOf: root.appendingPathComponent("fixture-state.json"))) as! [String: Any]
    try require(state["first_pid"] as? Int != Int(getpid()), "restart_pid_reused")
    _ = try await call(2, "restart.daily-open")
    _ = try await call(2, "restart.recovery-open", password: recovery)
    _ = try await call(5, "restart.list", password: recovery)
    let destination = root.appendingPathComponent("restore-after-restart")
    _ = try await call(6, "restart.restore", password: recovery, restore: destination, snapshot: state["snapshot_id"] as! String)
    try verifyRestore(destination, expected: state["expected"] as! [String: String], label: "restart.bytes")
    _ = try await call(7, "restart.full-integrity")
    log("phase.finished", ["phase": 2, "passed": true, "first_pid": state["first_pid"]!])
  }
  func cancelled() async throws {
    let baseline = try await call(5, "cancel.before-list", password: recovery)
    let payload = source.appendingPathComponent("cancel-random.bin")
    try require(!files.fileExists(atPath: payload.path), "cancel_fixture_exists")
    try require(files.createFile(atPath: payload.path, contents: nil), "cancel_fixture_create_failed")
    let output = try FileHandle(forWritingTo: payload)
    do {
      var chunk = Data(count: 8 * 1024 * 1024)
      for _ in 0..<64 {
        let rc = chunk.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, $0.count, $0.baseAddress!) }
        try require(rc == errSecSuccess, "fixture_random_failed")
        try output.write(contentsOf: chunk)
      }
      try output.synchronize(); try output.close()
    } catch { try? output.close(); throw error }
    log("cancel.fixture-written", ["bytes": 512 * 1024 * 1024])
    let witness = CancelWitness()
    let queue = DispatchQueue(label: "backup-cancel-witness")
    var sources: [DispatchSourceFileSystemObject] = []
    defer { for item in sources { item.cancel() } }
    for shard in 0..<256 {
      let directory = repo.appendingPathComponent(String(format: "data/%02x", shard))
      let fd = open(directory.path, O_EVTONLY)
      try require(fd >= 0, "cancel_watcher_open_failed_\(errno)")
      let item = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: .write, queue: queue)
      item.setEventHandler { witness.event() }
      item.setCancelHandler { close(fd) }
      sources.append(item); item.resume()
    }
    let began = ProcessInfo.processInfo.systemUptime
    let id = try start(4)
    witness.publish(id)
    log("cancel.snapshot-started", ["handle": id])
    let result = try await wait(id)
    let observed = witness.result()
    log("cancel.snapshot-drained", ["result": result, "write_observed": observed.0,
        "cancel_code": observed.1, "free_while_running": observed.2,
        "seconds": ProcessInfo.processInfo.systemUptime - began])
    try require(observed.0 && observed.1 == 0 && observed.2 == 6, "cancel_did_not_observe_active_write")
    try require(result["status"] as? Int == 4 && result["snapshot_id"] as? String == "", "cancel_published_success")
    for item in sources { item.cancel() }
    sources.removeAll()
    let after = try await call(5, "cancel.after-list", password: recovery)
    try require(Set(after["snapshot_ids"] as? [String] ?? []) == Set(baseline["snapshot_ids"] as? [String] ?? []), "cancel_published_snapshot")
    _ = try await call(7, "cancel.full-integrity", password: recovery)
    let drained = try hashes(repo)
    try await Task.sleep(nanoseconds: 2_000_000_000)
    let later = try hashes(repo)
    try require(drained == later, "cancel_late_repository_write")
    log("phase.finished", ["phase": 3, "passed": true, "no_late_write": true])
  }
  func run() async {
    do {
      let args = ProcessInfo.processInfo.arguments
      if args.contains("--cancel") { try await cancelled() } else if args.contains("--resume") { try await resumed() } else { try await first() }
      _exit(0)
    } catch {
      log("host.failed", ["error": String(describing: error)])
      _exit(1)
    }
  }
}
@main struct BackupProbeApp: App {
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
