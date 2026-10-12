import ExtensionFoundation
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import Foundation
import XPC

struct Handler: XPCPeerHandler {
  static let workQueue = DispatchQueue(label: "journal-runtime-work")
  func handleIncomingRequest(_ incoming: XPCReceivedMessage) -> (any Encodable)? {
    do {
      let message = try incoming.decode(as: Request.self)
      // All file/SQLite work is off the connection handler. Controls remain responsive.
      if message.op == "cancel" || message.op == "probe" || message.op == "activity" {
        return execute(message)
      }
      return incoming.handoffReply(to: Self.workQueue) { incoming.reply(self.execute(message)) }
    } catch { return Reply(json: "{\"error\":\"invalid request\"}") }
  }
  func execute(_ message: Request) -> Reply {
    do {
      guard !message.journal.isEmpty, message.journal.count <= 64,
        message.journal.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") })
      else { throw NSError(domain: "invalid-journal-name", code: 1) }
      // This container is the journal's authority. Host-supplied paths never select storage.
      guard message.op == "probe" || message.root.isEmpty else {
        throw NSError(domain: "host-path-is-not-authority", code: 1)
      }
      let root = NSHomeDirectory() + "/Documents/journal-" + message.journal
      var out: [String: Any] = [
        "op": message.op, "home": NSHomeDirectory(), "root": root, "before": resources(),
      ]
      if message.op == "probe" {
        out["host_file_readable"] = FileManager.default.isReadableFile(atPath: message.root)
        out["pid"] = getpid()
      } else if message.op == "export" {
        // Validate the retained authority before reading a closed-connection snapshot.
        let validation = String(
          data: try JSONEncoder().encode(
            Request(op: "prepare-export", journal: message.journal, root: root)), encoding: .utf8)!
        guard let pointer = validation.withCString({ journal_runtime($0) }) else {
          throw NSError(domain: "native-null", code: 1)
        }
        defer { journal_runtime_free(pointer) }
        let checked =
          try JSONSerialization.jsonObject(with: Data(String(cString: pointer).utf8))
          as! [String: Any]
        guard let result = checked["result"] as? [String: Any], result["ok"] as? Bool == true else {
          throw NSError(domain: "root-validation-failed", code: 1)
        }
        // Work queue serializes this snapshot with all writes.
        for file in [
          "indexer/journal.sqlite", "indexer/journal.sqlite-wal", "checkpoint.bin", "committed.bin",
        ] {
          if FileManager.default.fileExists(atPath: root + "/" + file) {
            out[file] = try Data(contentsOf: URL(fileURLWithPath: root + "/" + file))
              .base64EncodedString()
          }
        }
      } else {
        guard message.root.isEmpty else {
          throw NSError(domain: "host-path-is-not-authority", code: 1)
        }
        var request = message
        if request.op == "open" { request.root = root }
        let input = String(data: try JSONEncoder().encode(request), encoding: .utf8)!
        guard let p = input.withCString({ journal_runtime($0) }) else {
          throw NSError(domain: "native-null", code: 1)
        }
        defer { journal_runtime_free(p) }
        out["journal"] = try JSONSerialization.jsonObject(with: Data(String(cString: p).utf8))
      }
      out["after"] = resources()
      return Reply(
        json: String(
          data: try JSONSerialization.data(withJSONObject: out, options: [.sortedKeys]),
          encoding: .utf8)!)
    } catch {
      return Reply(
        json: String(
          data: try! JSONSerialization.data(withJSONObject: ["error": String(describing: error)]),
          encoding: .utf8)!)
    }
  }
}
@main struct Worker: AppExtension {
  @AppExtensionPoint.Bind var boundExtensionPoint: AppExtensionPoint {
    AppExtensionPoint.Identifier(host: "app.solstone.journal.runtime.dev", name: "journal")
  }
  var configuration: some AppExtensionConfiguration {
    ConnectionHandler(onSessionRequest: { request in request.accept { _ in Handler() } })
  }
}
