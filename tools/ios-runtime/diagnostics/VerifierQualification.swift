// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import Darwin
import Foundation

@_silgen_name("ios_observer_load_acquire")
private func ios_observer_load_acquire(_ pointer: UnsafePointer<UInt64>) -> UInt64

extension Runner {
  private func readNativeObservation(_ path: URL) -> [String: Any]? {
    let descriptor = open(path.path, O_RDONLY | O_NOFOLLOW)
    guard descriptor >= 0 else { return nil }
    defer { close(descriptor) }
    var info = stat()
    guard fstat(descriptor, &info) == 0, info.st_size == 32,
      (info.st_mode & S_IFMT) == S_IFREG,
      let memory = mmap(nil, 32, PROT_READ, MAP_SHARED, descriptor, 0), memory != MAP_FAILED
    else { return nil }
    defer { munmap(memory, 32) }
    let words = memory.assumingMemoryBound(to: UInt64.self)
    let before = ios_observer_load_acquire(words.advanced(by: 3))
    guard before == 1 || before == 2 else { return nil }
    let attempt = ios_observer_load_acquire(words)
    let pid = ios_observer_load_acquire(words.advanced(by: 1))
    let initialized = ios_observer_load_acquire(words.advanced(by: 2))
    let after = ios_observer_load_acquire(words.advanced(by: 3))
    guard before == after else { return nil }
    return ["attempt": attempt, "pid": pid, "main_thread_init": initialized == 1,
      "phase": after == 1 ? "entered" : "returned"]
  }

  func qualifyVerifierLifecycle() async throws {
    guard journal_ios_verifier_admit(),
      journal_ios_verifier_register_callback(appraisalTransportCallback)
    else { throw NSError(domain: "verifier-not-admitted", code: 1) }
    let fixtureURL = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
      .appendingPathComponent("verifier-fixture.json")
    let fixture = try JSONSerialization.jsonObject(with: Data(contentsOf: fixtureURL)) as! [String: Any]
    guard let group = FileManager.default.containerURL(
      forSecurityApplicationGroupIdentifier: "group.app.solstone.journal.runtime.dev")
    else { throw NSError(domain: "missing-group", code: 1) }
    let marker = group.appendingPathComponent("gpu-appraisal-native-observation.bin")
    let runAttempt = UInt64.random(in: 1...(UInt64(Int64.max) >> 8)) << 8
    var observedPIDs = Set<Int32>()
    for (index, mode) in ["positive", "positive", "proofs", "nonce", "evidence", "cancel", "positive", "deadline", "positive"].enumerated() {
      let attempt = runAttempt + UInt64(index)
      if FileManager.default.fileExists(atPath: marker.path) {
        try FileManager.default.removeItem(at: marker)
      }
      var request = fixture
      request["bridge_attempt"] = attempt
      request["budget_ms"] = mode == "deadline" ? 500 : 60_000
      if mode == "proofs" { request["proofs_hex"] = "010203" }
      if mode == "nonce" { request["nonce_hex"] = String(repeating: "00", count: 32) }
      if mode == "evidence" { request["evidence_json"] = [[:]] }
      if mode == "cancel" || mode == "deadline" {
        let evidence = fixture["evidence_json"] as! [[String: Any]]
        request["evidence_json"] = Array(repeating: evidence[0], count: 64)
      }
      let data = try JSONSerialization.data(withJSONObject: request, options: [.sortedKeys])
      guard data.count <= 1_114_112 else { throw NSError(domain: "request-cap", code: 1) }
      let encoded = String(decoding: data, as: UTF8.self)
      let started = ProcessInfo.processInfo.systemUptime
      let work = Task.detached {
        encoded.withCString { pointer -> [String: Any] in
          guard let result = AppraisalHostBridge.shared.transport(pointer) else { return ["null": true] }
          defer { free(result) }
          let returned = Data(String(cString: result).utf8)
          return (try? JSONSerialization.jsonObject(with: returned) as? [String: Any]) ?? [:]
        }
      }
      var activeWitness: [String: Any] = [:]
      if mode == "cancel" {
        let deadline = ProcessInfo.processInfo.systemUptime + 10
        while ProcessInfo.processInfo.systemUptime < deadline {
          if let value = readNativeObservation(marker),
            (value["attempt"] as? NSNumber)?.uint64Value == attempt,
            value["phase"] as? String == "entered" {
            try await Task.sleep(for: .milliseconds(50))
            if let still = readNativeObservation(marker),
              (still["attempt"] as? NSNumber)?.uint64Value == attempt,
              still["phase"] as? String == "entered" {
              activeWitness = still
              activeWitness["cancel_uptime"] = ProcessInfo.processInfo.systemUptime
              AppraisalHostBridge.shared.cancel()
              break
            }
          }
          try await Task.sleep(for: .milliseconds(5))
        }
        guard !activeWitness.isEmpty else { throw NSError(domain: "no-active-native-witness", code: 1) }
      }
      let result = await work.value
      guard let resultPID = result["pid"] as? NSNumber,
        resultPID.int64Value > 0, resultPID.int64Value <= Int64(Int32.max),
        (result["bridge_attempt"] as? NSNumber)?.uint64Value == attempt
      else { throw NSError(domain: "result-identity-missing", code: 1) }
      guard observedPIDs.insert(resultPID.int32Value).inserted else {
        throw NSError(domain: "helper-pid-reused", code: 1)
      }
      let elapsed = ProcessInfo.processInfo.systemUptime - started
      let reply = result["reply"] as? [String: Any] ?? [:]
      let code = reply["result_code"] as? Int
      let observation = readNativeObservation(marker) ?? [:]
      log("verifier.lifecycle", ["mode": mode, "attempt": attempt, "seconds": elapsed,
        "pid": result["pid"] ?? 0, "exited": result["exited"] ?? false,
        "cancelled": result["cancelled"] ?? false, "deadline": result["deadline"] ?? false,
        "result_code": code ?? -1, "main_thread_init": reply["main_thread_init"] ?? false,
        "native_observation": observation, "active_cancel_witness": activeWitness])
      fflush(stdout)
      guard result["exited"] as? Bool == true else { throw NSError(domain: "death-unproved", code: 1) }
      if mode == "cancel" {
        guard result["cancelled"] as? Bool == true, elapsed < 5,
          observation["phase"] as? String == "entered",
          (result["bridge_attempt"] as? NSNumber)?.uint64Value == attempt,
          (activeWitness["attempt"] as? NSNumber)?.uint64Value == attempt,
          (observation["attempt"] as? NSNumber)?.uint64Value == attempt,
          (activeWitness["pid"] as? NSNumber)?.int32Value == (result["pid"] as? NSNumber)?.int32Value,
          (observation["pid"] as? NSNumber)?.int32Value == (result["pid"] as? NSNumber)?.int32Value,
          observation["main_thread_init"] as? Bool == true
        else { throw NSError(domain: "cancel-not-prompt", code: 1) }
      } else if mode == "deadline" {
        guard result["deadline"] as? Bool == true, elapsed < 4,
          observation["phase"] as? String == "entered",
          (observation["attempt"] as? NSNumber)?.uint64Value == attempt,
          (observation["pid"] as? NSNumber)?.int32Value == (result["pid"] as? NSNumber)?.int32Value,
          observation["main_thread_init"] as? Bool == true
        else { throw NSError(domain: "native-deadline-unproved", code: 1) }
      } else {
        let expected = mode == "positive" ? 0 : mode == "proofs" ? 201 : mode == "nonce" ? 504 : 2
        guard code == expected, reply["main_thread_init"] as? Bool == true,
          observation["phase"] as? String == "returned",
          (observation["attempt"] as? NSNumber)?.uint64Value == attempt,
          (observation["pid"] as? NSNumber)?.int32Value == resultPID.int32Value,
          observation["main_thread_init"] as? Bool == true
        else { throw NSError(domain: "native-verdict-mismatch", code: code ?? -1) }
      }
      AppraisalHostBridge.shared.clearCancellationIfDrained()
    }
    log("verifier.lifecycle.finished", ["passed": true])
    fflush(stdout)
  }
}
