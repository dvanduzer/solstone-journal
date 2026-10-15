// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import CryptoKit
import Darwin
import Foundation
import Security

extension Probe {
  func validIntentID(_ value: String) -> Bool {
    value.utf8.count == 32 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
  }
  func explicitStart(_ operation: Int, secret: String? = nil, state: URL? = nil, sourcePath: URL? = nil) throws -> (UInt64, Int32) {
    var request: [String: Any] = [:]
    for key in ["endpoint", "region", "bucket", "prefix", "access_key", "secret_key", "session_token"] { request[key] = configuration[key] ?? "" }
    if let secret { request["secret_key"] = secret }
    request["operation"] = operation
    request["password"] = operation == 8 ? "" : daily
    request["recovery_password"] = ""
    request["state_path"] = (state ?? ownerState).path
    request["source_path"] = operation == 4 ? (sourcePath ?? source).path : ""
    request["restore_path"] = ""; request["snapshot_id"] = ""
    request["workspace_path"] = operation == 4 ? try metadataWorkspace().path : ""
    var bytes = try JSONSerialization.data(withJSONObject: request)
    defer { bytes.resetBytes(in: bytes.indices) }
    bytes.append(0)
    var code: Int32 = -1
    let handle = bytes.withUnsafeBytes { backup_probe_start($0.baseAddress!.assumingMemoryBound(to:CChar.self), &code) }
    return (handle, code)
  }
  func explicitCall(_ operation: Int, _ label: String, secret: String? = nil, expectedStatus: Int = 0) async throws -> [String: Any] {
    let (handle, code) = try explicitStart(operation, secret:secret)
    try require(code == 0 && handle != 0, "explicit_start_refused_\(code)")
    log(label + ".started", ["handle":handle, "explicit_credentials":true])
    let result = try await wait(handle)
    log(label, ["handle":handle, "result":result, "actual_wait_and_free":true])
    try require(result["status"] as? Int == expectedStatus, label + "_unexpected_status")
    return result
  }
  func destinationBytes() throws -> Data {
    guard let endpoint = URLComponents(string:configuration["endpoint"]!), let host = endpoint.host else {
      throw ProbeError(reason:"destination_shape_refused")
    }
    let canonical = "https://" + host.lowercased() + ":" + String(endpoint.port ?? 443)
    var credentials = [Data(configuration["access_key"]!.utf8), Data(configuration["secret_key"]!.utf8), Data((configuration["session_token"] ?? "").utf8)]
    var password = Data(daily.utf8)
    var encodedPassword = [password.base64EncodedData()]
    defer {
      for index in credentials.indices { credentials[index].resetBytes(in:credentials[index].indices) }
      password.resetBytes(in:password.indices)
      encodedPassword[0].resetBytes(in:encodedPassword[0].indices)
    }
    let credentialCommitment = fixtureCommitment("ios-s3-credential-match-v1", values:credentials)
    let passwordCommitment = fixtureCommitment("ios-s3-password-match-v1", values:encodedPassword)
    return try JSONSerialization.data(withJSONObject:["endpoint":canonical, "region":configuration["region"]!,
      "bucket":configuration["bucket"]!, "prefix":configuration["prefix"]!, "nonce":runNonce,
      "credential_commitment":credentialCommitment, "password_commitment":passwordCommitment]) + Data([0])
  }
  func fixtureCommitment(_ domain: String, values: [Data]) -> String {
    var digest = SHA256()
    digest.update(data:Data(domain.utf8) + Data([0]))
    func length(_ value: Int) {
      var encoded = UInt64(value).bigEndian
      withUnsafeBytes(of:&encoded) { digest.update(data:Data($0)) }
    }
    let salt = Data(runNonce.utf8)
    length(salt.count); digest.update(data:salt); length(values.count)
    for value in values { length(value.count); digest.update(data:value) }
    return digest.finalize().map { String(format:"%02x", $0) }.joined()
  }
  func observe(_ observer: UnsafeMutableRawPointer) async throws -> [String: Any] {
    // This bounded native watcher sleeps on actual directory notifications.
    let address = UInt(bitPattern:observer)
    return try await withCheckedThrowingContinuation { continuation in
      DispatchQueue.global(qos:.userInitiated).async {
        guard let held = UnsafeMutableRawPointer(bitPattern:address), let pointer = backup_s3_observer_wait(held, 30000) else {
          continuation.resume(throwing:ProbeError(reason:"active_intent_unexercised")); return
        }
        defer { backup_probe_json_free(pointer) }
        do {
          guard let value = try JSONSerialization.jsonObject(with:Data(String(cString:pointer).utf8)) as? [String: Any] else {
            throw ProbeError(reason:"intent_evidence_shape_refused")
          }
          continuation.resume(returning:value)
        } catch { continuation.resume(throwing:error) }
      }
    }
  }
  func lifecycleState() throws -> [String: Any] {
    let state = try JSONSerialization.jsonObject(with:Data(contentsOf:root.appendingPathComponent("fixture-state.json"))) as! [String: Any]
    try require(state["fixture_sha256"] as? String == fixtureSHA && validID(state["daily_key_id"] as? String ?? ""), "retained_fixture_identity_refused")
    return state
  }
  func observedStart(_ operation: Int, requireWrite: Bool, sourcePath: URL? = nil, label: String) async throws -> (UInt64, [String: Any]) {
    let destination = try destinationBytes()
    guard let observer = destination.withUnsafeBytes({ backup_s3_observer_create($0.baseAddress!.assumingMemoryBound(to:CChar.self), Int32(operation), requireWrite ? 1 : 0) }) else {
      throw ProbeError(reason:"observer_admission_failed")
    }
    var observerClosed = false
    defer { if !observerClosed { _ = backup_s3_observer_free(observer) } }
    let (handle, code) = try explicitStart(operation, sourcePath:sourcePath)
    try require(code == 0 && handle != 0, "observed_start_refused")
    log(label + ".started", ["handle":handle])
    do {
      let evidence = try await observe(observer)
      let closeCode = backup_s3_observer_free(observer); observerClosed = true
      try require(closeCode == 0, "observer_close_failed")
      log(label + ".intent", ["handle":handle, "observation":evidence, "observer_closed":true])
      return (handle, evidence)
    } catch {
      let cancelled = backup_probe_cancel(handle)
      let drained = try await wait(handle)
      log(label + ".unexercised-drain", ["handle":handle, "cancel_code":cancelled, "result":drained])
      throw error
    }
  }
  @discardableResult
  func readyRetry(_ label: String, state: [String: Any], allowNew: Bool = false) async throws -> [String] {
    let inspected = try await call(8, label + ".inspect")
    try require(inspected["diagnostic"] as? String == "S3_STATE_READY", "owner_not_ready")
    let opened = try await explicitCall(2, label + ".good-open")
    try require(opened["key_id"] as? String == state["daily_key_id"] as? String, "retained_daily_key_mismatch")
    let listed = try await call(5, label + ".list", password:recovery)
    let ids = listed["snapshot_ids"] as? [String] ?? []
    let originals = [state["first_snapshot_id"] as? String ?? "", state["snapshot_id"] as? String ?? ""]
    try require(originals.allSatisfy(ids.contains) && (allowNew || Set(ids) == Set(originals)), "retained_snapshots_missing_or_changed")
    _ = try await call(7, label + ".integrity", password:recovery)
    return ids
  }
  func readCancellation() async throws {
    let retained = try lifecycleState()
    let inspected = try await call(8, "lifecycle.initial-inspect")
    try require(inspected["diagnostic"] as? String == "S3_STATE_READY", "lifecycle_requires_ready_owner")
    let wrong = "synthetic-invalid-s3-secret-vdajrvrs"
    try require(wrong != configuration["secret_key"], "wrong_secret_fixture_invalid")
    let refusal = try await explicitCall(2, "credentials.wrong-secret", secret:wrong, expectedStatus:2)
    try require(refusal["diagnostic"] as? String == "AUTH_FAILED", "wrong_storage_credential_reason_changed")
    let opened = try await explicitCall(2, "credentials.good-secret")
    try require(opened["key_id"] as? String == retained["daily_key_id"] as? String, "explicit_good_credential_identity_mismatch")
    let (handle, _) = try await observedStart(2, requireWrite:false, label:"cancel.read")
    let busy = backup_probe_free(handle)
    log("cancel.read.live", ["handle":handle, "free_code":busy])
    if busy != 6 {
      if busy != 0 { _ = backup_probe_cancel(handle); _ = try await wait(handle) }
      throw ProbeError(reason:"read_cancel_unexercised")
    }
    let cancelCode = backup_probe_cancel(handle)
    let result = try await wait(handle)
    log("cancel.read.finished", ["handle":handle, "cancel_code":cancelCode, "actual_wait_and_free":true, "result":result])
    try require(cancelCode == 0 && result["status"] as? Int == 3 && result["diagnostic"] as? String == "CANCELLED", "read_cancel_classification_unexercised")
    try await readyRetry("cancel.read.retry", state:retained)
    log("phase.finished", ["phase":3, "passed":true])
  }
  func ownerObservation() throws -> [String: Any] {
    let destination = try destinationBytes()
    guard let pointer = destination.withUnsafeBytes({ backup_s3_read_owner($0.baseAddress!.assumingMemoryBound(to:CChar.self)) }) else {
      throw ProbeError(reason:"owner_observation_refused")
    }
    defer { backup_probe_json_free(pointer) }
    guard let value = try JSONSerialization.jsonObject(with:Data(String(cString:pointer).utf8)) as? [String: Any] else {
      throw ProbeError(reason:"owner_observation_shape_refused")
    }
    return value
  }
  func largeSource(_ label: String) throws -> URL {
    var directory = root.appendingPathComponent("lifecycle-" + label + "-" + runNonce)
    try require(!files.fileExists(atPath:directory.path), "large_fixture_already_exists")
    try files.createDirectory(at:directory, withIntermediateDirectories:false, attributes:[.posixPermissions:0o700, .protectionKey:FileProtectionType.complete.rawValue])
    var policy = URLResourceValues(); policy.isExcludedFromBackup = true
    try directory.setResourceValues(policy)
    let observed = try directory.resourceValues(forKeys:[.isExcludedFromBackupKey])
    try require(observed.isExcludedFromBackup == true, "large_fixture_backup_exclusion_failed")
    let file = directory.appendingPathComponent("incompressible.bin")
    try require(files.createFile(atPath:file.path, contents:nil, attributes:[.posixPermissions:0o600, .protectionKey:FileProtectionType.complete.rawValue]), "large_fixture_create_failed")
    let stream = try FileHandle(forWritingTo:file)
    var closed = false
    defer { if !closed { try? stream.close() } }
    var chunk = Data(count:1024 * 1024), digest = SHA256()
    for _ in 0..<256 {
      let code = chunk.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, $0.count, $0.baseAddress!) }
      try require(code == errSecSuccess, "large_fixture_random_failed")
      digest.update(data:chunk); try stream.write(contentsOf:chunk)
    }
    try stream.synchronize()
    try stream.close(); closed = true
    log(label + ".source", ["bytes":256 * 1024 * 1024, "sha256":digest.finalize().map { String(format:"%02x", $0) }.joined(),
      "streamed":true, "excluded_from_backup":true])
    return directory
  }
  func pendingRefusal(_ label: String, observed: [String: Any]) async throws {
    let before = try ownerObservation()
    let intent = before["active"] as? [String: Any] ?? [:]
    let prior = observed["active"] as? [String: Any] ?? [:]
    try require(intent["id"] as? String == prior["id"] as? String && intent["operation"] as? Int == 4 &&
      intent["may_have_written"] as? Bool == true && before["recovery_material_sha256"] as? String == observed["recovery_material_sha256"] as? String,
      "pending_intent_or_material_changed")
    let inspected = try await call(8, label + ".inspect", expectedStatus:11)
    try require(inspected["diagnostic"] as? String == "REMOTE_PENDING", "pending_inspection_classification_changed")
    let refused = try await explicitCall(2, label + ".open-refused", expectedStatus:11)
    try require(refused["diagnostic"] as? String == "REMOTE_PENDING", "pending_open_not_refused")
    let after = try ownerObservation()
    try require(after["record_sha256"] as? String == before["record_sha256"] as? String, "pending_record_changed_by_refusal")
    log(label + ".retained", ["observation":after])
  }
  func writeCancellation() async throws {
    let retained = try lifecycleState()
    let inspected = try await call(8, "cancel.write.initial-inspect")
    try require(inspected["diagnostic"] as? String == "S3_STATE_READY", "write_cancel_requires_ready_owner")
    let alternateName = "callable-s3-alternate-" + runNonce
    guard let alternateAuthority = alternateName.withCString({ backup_s3_alternate_root_create($0) }) else {
      throw ProbeError(reason:"alternate_root_construction_failed")
    }
    var alternateCloseAttempted = false
    defer {
      if !alternateCloseAttempted {
        alternateCloseAttempted = true
        let code = backup_s3_alternate_root_free(alternateAuthority)
        log("cancel.write.alternate.closed", ["code":code, "failed_path":true])
      }
    }
    let alternate = ownerState.deletingLastPathComponent().appendingPathComponent(alternateName)
    try require(alternate.path.withCString { backup_s3_alternate_root_check(alternateAuthority, $0) } == 0,
      "alternate_root_preflight_failed")
    log("cancel.write.alternate.prepared", ["held_owned_empty_root":true, "production_policy_apply_and_check":true])
    let large = try largeSource("cancel")
    let (handle, observed) = try await observedStart(4, requireWrite:true, sourcePath:large, label:"cancel.write")
    var retired = false
    do {
    let busy = backup_probe_free(handle)
    log("cancel.write.first-live", ["handle":handle, "free_code":busy])
    if busy != 6 {
      if busy == 0 { retired = true }
      throw ProbeError(reason:"write_cancel_unexercised")
    }
    try require(alternate.path.withCString { backup_s3_alternate_root_check(alternateAuthority, $0) } == 0,
      "alternate_root_policy_failed")
    let (replacement, admission) = try explicitStart(8, secret:"synthetic-explicit-replacement-secret", state:alternate)
    log("cancel.write.replacement-refused", ["original_handle":handle, "start_code":admission, "out_handle":replacement,
      "alternate_root_policy_verified":true, "explicit_replacement_credentials":true])
    if replacement != 0 {
      _ = backup_probe_cancel(replacement)
      let unexpected = try await wait(replacement)
      log("cancel.write.unexpected-inspect-drained", ["handle":replacement, "result":unexpected])
    }
    if admission != 6 || replacement != 0 {
      throw ProbeError(reason:"live_replacement_not_refused")
    }
    try require(alternate.path.withCString { backup_s3_alternate_root_check(alternateAuthority, $0) } == 0,
      "alternate_root_changed_by_refused_replacement")
    let secondBusy = backup_probe_free(handle)
    log("cancel.write.live", ["handle":handle, "free_code":secondBusy])
    if secondBusy != 6 {
      if secondBusy == 0 { retired = true }
      throw ProbeError(reason:"write_cancel_no_longer_live")
    }
    let cancelled = backup_probe_cancel(handle)
    let result = try await wait(handle)
    retired = true
    log("cancel.write.finished", ["handle":handle, "cancel_code":cancelled, "result":result, "actual_wait_and_free":true])
    try require(cancelled == 0, "write_cancel_signal_refused")
    alternateCloseAttempted = true
    let alternateCloseCode = backup_s3_alternate_root_free(alternateAuthority)
    log("cancel.write.alternate.closed", ["code":alternateCloseCode, "failed_path":false])
    try require(alternateCloseCode == 0, "alternate_root_close_failed")
    if result["status"] as? Int == 4 && result["diagnostic"] as? String == "CANCELLED_UNCERTAIN" {
      let ids = try await readyRetry("cancel.write.retry", state:retained, allowNew:true)
      let known = result["snapshot_id"] as? String ?? ""
      try require(known.isEmpty || (validID(known) && ids.contains(known)), "cancelled_known_snapshot_not_in_list")
      log("phase.finished", ["phase":4, "passed":true, "owner_category":"READY", "rollback_claimed":false])
    } else if result["status"] as? Int == 11 && result["diagnostic"] as? String == "REMOTE_PENDING" {
      try await pendingRefusal("cancel.write.pending", observed:observed)
      log("phase.finished", ["phase":4, "passed":true, "owner_category":"REMOTE_PENDING", "accepted_live_cancel_claimed":false,
        "new_namespace_required_for_kill":true])
    } else { throw ProbeError(reason:"write_cancel_classification_unexercised") }
    } catch {
      if (error as? ProbeError)?.retiredHandle == handle { retired = true }
      if !retired {
        let code = backup_probe_cancel(handle)
        let drained = try await wait(handle)
        log("cancel.write.failed-control-drain", ["handle":handle, "cancel_code":code, "result":drained])
      }
      throw error
    }
  }
  func killedWrite() async throws {
    _ = try lifecycleState()
    let inspected = try await call(8, "kill.write.initial-inspect")
    try require(inspected["diagnostic"] as? String == "S3_STATE_READY", "kill_requires_ready_owner")
    let large = try largeSource("kill")
    let (handle, observed) = try await observedStart(4, requireWrite:true, sourcePath:large, label:"kill.write")
    let busy = backup_probe_free(handle)
    if busy != 6 {
      if busy != 0 { _ = backup_probe_cancel(handle); _ = try await wait(handle) }
      throw ProbeError(reason:"kill_write_unexercised")
    }
    log("kill.write.live", ["handle":handle, "free_code":busy, "observation":observed, "self_pid":getpid(),
      "sampled_active_only":true])
    let killCode = kill(getpid(), SIGKILL)
    let cancelCode = backup_probe_cancel(handle)
    let drained = try await wait(handle)
    log("kill.write.failed-kill-drained", ["handle":handle, "kill_code":killCode, "cancel_code":cancelCode, "result":drained])
    throw ProbeError(reason:"self_kill_returned")
  }
  func killedRestart() async throws {
    func argument(_ name: String) throws -> String {
      let arguments = ProcessInfo.processInfo.arguments
      let indices = arguments.indices.filter { arguments[$0] == name }
      try require(indices.count == 1 && indices[0] + 1 < arguments.count, "pending_argument_refused")
      return arguments[indices[0] + 1]
    }
    let id = try argument("--pending-intent"), record = try argument("--pending-record-sha256"), material = try argument("--pending-material-sha256")
    let priorPID = Int(try argument("--killed-pid")) ?? 0
    try require(validIntentID(id) && validID(record) && validID(material) && priorPID > 0 && priorPID != Int(getpid()), "pending_bindings_refused")
    let observed = try ownerObservation(), active = observed["active"] as? [String: Any] ?? [:]
    try require(active["id"] as? String == id && observed["record_sha256"] as? String == record &&
      observed["recovery_material_sha256"] as? String == material, "killed_owner_record_changed")
    try await pendingRefusal("kill.restart.pending", observed:observed)
    log("phase.finished", ["phase":6, "passed":true, "killed_pid":priorPID, "sampled_active_only":true,
      "crash_caused_pending_claimed":false])
  }
  func retainedPendingDiagnostic() async throws {
    func argument(_ name: String) throws -> String {
      let arguments = ProcessInfo.processInfo.arguments
      let indices = arguments.indices.filter { arguments[$0] == name }
      try require(indices.count == 1 && indices[0] + 1 < arguments.count, "pending_diagnostic_argument_refused")
      return arguments[indices[0] + 1]
    }
    let id = try argument("--pending-intent"), record = try argument("--pending-record-sha256")
    let material = try argument("--pending-material-sha256"), lock = try argument("--pending-lock")
    let endedPID = Int(try argument("--ended-pid")) ?? 0
    try require(validIntentID(id) && validID(record) && validID(material) && validID(lock) &&
      endedPID > 0 && endedPID != Int(getpid()), "pending_diagnostic_bindings_refused")
    let observed = try ownerObservation(), active = observed["active"] as? [String: Any] ?? [:]
    try require(active["id"] as? String == id && active["operation"] as? Int == 4 &&
      active["may_have_written"] as? Bool == true && (active["locks"] as? [String] ?? []).contains(lock) &&
      observed["record_sha256"] as? String == record && observed["recovery_material_sha256"] as? String == material,
      "pending_diagnostic_record_changed")
    log("failed-write.pending.before", ["ended_pid":endedPID, "observation":observed])
    try await pendingRefusal("failed-write.pending", observed:observed)
    log("phase.finished", ["phase":7, "passed":true, "ended_pid":endedPID, "pending_cleared":false,
      "remote_mutation_requested":false, "origin":"normally_exited_failed_control"])
  }
  func lifecycle() async throws {
    log("lifecycle.paths", ["root":root.path, "owner_state":ownerState.path, "source":source.path])
    if phase == 3 { try await readCancellation(); return }
    if phase == 4 { try await writeCancellation(); return }
    if phase == 5 { try await killedWrite(); return }
    if phase == 6 { try await killedRestart(); return }
    if phase == 7 { try await retainedPendingDiagnostic(); return }
    throw ProbeError(reason:"lifecycle_phase_not_implemented")
  }
}
