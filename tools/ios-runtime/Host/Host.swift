import ExtensionFoundation
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import SwiftUI
import UIKit
import XPC

extension AppExtensionPoint {
  @Definition static var journal: AppExtensionPoint { Name("journal") }
}
final class PendingReply: @unchecked Sendable {
  let lock = NSLock()
  var continuation: CheckedContinuation<Reply, Error>?
  var watchdog: DispatchWorkItem?
  init(_ c: CheckedContinuation<Reply, Error>) { continuation = c }
  @discardableResult func finish(_ result: Result<Reply, Error>) -> Bool {
    lock.lock()
    let c = continuation
    continuation = nil
    watchdog?.cancel()
    watchdog = nil
    lock.unlock()
    guard let c else { return false }
    c.resume(with: result)
    return true
  }
}

@MainActor final class Runner: ObservableObject {
  @Published var status = "journal closed"
  @Published var busy = false
  @Published var result = ""
  var process: AppExtensionProcess?
  var session: XPCSession?
  var started = false
  var finished = false
  let output = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
  let args = ProcessInfo.processInfo.arguments
  func argument(_ name: String, _ fallback: String) -> String {
    guard let i = args.firstIndex(of: name), i + 1 < args.count else { return fallback }
    return args[i + 1]
  }
  var journal: String { argument("--suite", "development") }
  func request(
    _ op: String, count: Int = 128, seconds: Int = 0, revision: Int = 0, mode: String = "reopen",
    root: String = ""
  ) -> Request {
    Request(
      op: op, journal: journal, root: root, count: count, seconds: seconds, revision: revision,
      mode: mode)
  }
  func log(_ event: String, _ value: Any) {
    do {
      let row: [String: Any] = [
        "event": event, "time": Date().timeIntervalSince1970,
        "uptime": ProcessInfo.processInfo.systemUptime, "host_pid": getpid(), "value": value,
      ]
      let data = try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys])
      let file = output.appendingPathComponent("evidence.jsonl")
      if !FileManager.default.fileExists(atPath: file.path) {
        FileManager.default.createFile(atPath: file.path, contents: nil)
      }
      let handle = try FileHandle(forWritingTo: file)
      try handle.seekToEnd()
      try handle.write(contentsOf: data + Data([10]))
      try handle.close()
      print(String(data: data, encoding: .utf8)!)
      status = event
    } catch {
      status = "evidence error: \(error)"
      print(status)
    }
  }
  func reopen() async throws {
    session?.cancel(reason: "explicit reconnect")
    session = nil
    process = nil
    let monitor = try await AppExtensionPoint.Monitor(appExtensionPoint: .journal)
    guard let identity = monitor.identities.first else {
      throw NSError(domain: "helper-not-discovered", code: 1)
    }
    let p = try await AppExtensionProcess(
      configuration: .init(
        appExtensionIdentity: identity,
        onInterruption: { [weak self] in
          Task { @MainActor in self?.log("helper.interruption", ["observed": true]) }
        }))
    let s = try p.makeXPCSession()
    try s.activate()
    process = p
    session = s
    _ = try await call(s, request("open"), "open")
  }
  func call(_ s: XPCSession, _ request: Request, _ label: String) async throws -> [String: Any] {
    let start = ProcessInfo.processInfo.systemUptime
    let reply: Reply = try await withCheckedThrowingContinuation { continuation in
      let pending = PendingReply(continuation)
      let timer = DispatchWorkItem {
        if pending.finish(.failure(NSError(domain: "reply-timeout", code: 150))) {
          s.cancel(reason: "reply timeout; outcome may be committed")
        }
      }
      pending.watchdog = timer
      DispatchQueue.global().asyncAfter(deadline: .now() + 150, execute: timer)
      do { try s.send(request) { (r: Result<Reply, Error>) in pending.finish(r) } } catch {
        pending.finish(.failure(error))
      }
    }
    let obj = try JSONSerialization.jsonObject(with: Data(reply.json.utf8)) as! [String: Any]
    log(
      label, ["roundtrip_ms": (ProcessInfo.processInfo.systemUptime - start) * 1000, "reply": obj])
    if let e = obj["error"] { throw NSError(domain: String(describing: e), code: 1) }
    if let j = obj["journal"] as? [String: Any], let r = j["result"] as? [String: Any],
      r["ok"] as? Bool == false
    {
      throw NSError(domain: String(describing: r["error"]), code: 1)
    }
    return obj
  }
  func invoke(_ request: Request, _ label: String) async throws -> [String: Any] {
    guard let s = session else { throw NSError(domain: "journal-closed", code: 1) }
    return try await call(s, request, label)
  }
  func command(_ op: String) {
    Task {
      busy = true
      defer { busy = false }
      do {
        if op == "open" {
          try await reopen()
          return
        }
        let reply = try await invoke(request(op), op)
        result = String(describing: reply)
      } catch {
        status = "operation failed: \(error)"
        result = status
      }
    }
  }
  func export() async throws {
    let exported = try await invoke(request("export"), "export")
    for file in [
      "indexer/journal.sqlite", "indexer/journal.sqlite-wal", "checkpoint.bin", "committed.bin",
    ] {
      if let encoded = exported[file] as? String, let data = Data(base64Encoded: encoded) {
        try data.write(
          to: output.appendingPathComponent(
            journal + "-" + file.replacingOccurrences(of: "/", with: "-")), options: .atomic)
      }
    }
  }
  func start() async {
    guard !started else { return }
    started = true
    UIApplication.shared.isIdleTimerDisabled = true
    UIDevice.current.isBatteryMonitoringEnabled = true
    guard args.contains("--qualify") else { return }
    let scenario = argument("--scenario", "acceptance")
    let count = Int(argument("--count", "128")) ?? 128
    let duration = Int(argument("--duration", "1200")) ?? 1200
    let watchdog = Task {
      try await Task.sleep(for: .seconds(1800))
      log("host.watchdog", [:])
      exit(78)
    }
    defer {
      watchdog.cancel()
      finished = true
    }
    for _ in 0..<50 {
      if UIApplication.shared.applicationState == .active { break }
      try? await Task.sleep(for: .milliseconds(100))
    }
    Task {
      while !finished {
        try? await Task.sleep(for: .seconds(1))
        if !finished {
          log(
            "host.heartbeat",
            [
              "state": UIApplication.shared.applicationState.rawValue, "resources": resources(),
              "thermal": ProcessInfo.processInfo.thermalState.rawValue,
              "battery": UIDevice.current.batteryState.rawValue,
            ])
        }
      }
    }
    log(
      "host.start",
      [
        "state": UIApplication.shared.applicationState.rawValue, "scenario": scenario,
        "journal": journal, "count": count, "duration": duration, "documents": output.path,
        "resources": resources(),
      ])
    do {
      try Data("host-owned authority control".utf8).write(
        to: output.appendingPathComponent("host.txt"))
      try await reopen()
      _ = try await invoke(
        request("probe", root: output.appendingPathComponent("host.txt").path), "authority")
      if scenario != "reopen" {
        _ = try await invoke(request("seed", count: count), "seed")
        _ = try await invoke(request("index"), "index")
      }
      if scenario == "acceptance" {
        _ = try await invoke(request("create"), "create.first")
        do {
          _ = try await invoke(request("create"), "create.unexpected")
          throw NSError(domain: "existing-destination-accepted", code: 2)
        } catch {
          guard (error as NSError).domain != "existing-destination-accepted" else { throw error }
          log("create.refused", String(describing: error))
        }
        _ = try await invoke(request("binding-control"), "binding.control")
        for fault in ["crash-write", "commit-disconnect"] {
          do {
            _ = try await invoke(request(fault), fault + ".unexpected")
            throw NSError(domain: "fault-did-not-fire", code: 2)
          } catch {
            guard (error as NSError).domain != "fault-did-not-fire" else { throw error }
            log(fault + ".lost-reply", String(describing: error))
          }
          try await Task.sleep(for: .seconds(1))
          try await reopen()
          _ = try await invoke(
            request(fault == "crash-write" ? "recover" : "inventory"), fault + ".reopened")
        }
        // A new process object may resolve to the same operating-system process.
        try await reopen()
        _ = try await invoke(request("probe"), "handle.reopened")
        let interruptedSession = session!
        let interrupted = Task {
          try await self.call(
            interruptedSession, self.request("active", seconds: 30), "connection.work")
        }
        try await Task.sleep(for: .seconds(1))
        interruptedSession.cancel(reason: "qualification connection interruption")
        do {
          _ = try await interrupted.value
          log("connection.reply-after-cancel", [:])
        } catch { log("connection.lost-reply", String(describing: error)) }
        try await reopen()
        _ = try await invoke(request("cancel"), "connection.cancel")
        _ = try await invoke(request("query"), "connection.recovered")
        let s = session!
        let active = Task {
          try await self.call(s, self.request("active", seconds: 30), "cancel.work")
        }
        try await Task.sleep(for: .seconds(2))
        _ = try await invoke(request("cancel"), "cancel.control")
        _ = try await active.value
        _ = try await invoke(request("probe"), "idle.before")
        try await Task.sleep(for: .seconds(30))
        _ = try await invoke(request("query"), "idle.after")
      } else if scenario == "hostloss" {
        let work = Task {
          try await self.invoke(self.request("active", seconds: 120), "hostloss.work")
        }
        try await Task.sleep(for: .seconds(1))
        let observed = try await invoke(request("activity"), "hostloss.activity")
        guard let native = observed["journal"] as? [String: Any],
          let result = native["result"] as? [String: Any],
          let value = result["value"] as? [String: Any], value["active"] as? Bool == true
        else { throw NSError(domain: "worker-not-active", code: 1) }
        log("host.kill-ready", [:])
        _ = try await work.value
        throw NSError(domain: "host-was-not-killed", code: 2)
      } else if scenario == "controls" {
        for mode in ["reopen", "retained", "reopen", "retained", "session"] {
          log("control.begin", ["mode": mode])
          _ = try await invoke(request("control", seconds: 10, mode: mode), "control." + mode)
        }
      } else if scenario == "mixed" {
        let start = ProcessInfo.processInfo.systemUptime
        var n = 0
        while ProcessInfo.processInfo.systemUptime - start < Double(max(1, min(duration, 1500))) {
          n += 1
          _ = try await invoke(request("mixed", count: n, revision: n), "mixed.\(n)")
          try await Task.sleep(for: .seconds(1))
        }
        log(
          "mixed.completed", ["units": n, "seconds": ProcessInfo.processInfo.systemUptime - start])
      }
      _ = try await invoke(request("query"), "final.query")
      _ = try await invoke(request("inventory"), "final.inventory")
      try await export()
      log(
        "host.finished",
        [
          "passed": true, "state": UIApplication.shared.applicationState.rawValue,
          "resources": resources(),
        ])
      if !args.contains("--hold") {
        try await Task.sleep(for: .seconds(1))
        exit(0)
      }
    } catch {
      log("host.failed", String(describing: error))
      if !args.contains("--hold") { exit(1) }
    }
  }
}
@main struct RuntimeDev: App {
  @StateObject var runner = Runner()
  var body: some Scene {
    WindowGroup {
      VStack(alignment: .leading, spacing: 16) {
        Text("journal development").font(.title)
        Text(runner.status).accessibilityIdentifier("runtime-status")
        HStack {
          Button("open / reconnect") { runner.command("open") }
          Button("cancel") { runner.command("cancel") }
        }
        HStack {
          Button("add sample memories") { runner.command("seed") }
          Button("index") { runner.command("index") }
          Button("search") { runner.command("query") }
        }
        ScrollView { Text(runner.result).font(.caption) }
      }.padding().task { await runner.start() }
    }
  }
}
