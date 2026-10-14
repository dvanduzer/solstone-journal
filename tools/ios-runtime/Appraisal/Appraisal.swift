// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import ExtensionFoundation
import Foundation
import XPC

@_silgen_name("ios_gpu_appraisal_execute")
func ios_gpu_appraisal_execute(_ input: UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>?

@_silgen_name("ios_gpu_appraisal_free")
func ios_gpu_appraisal_free(_ ptr: UnsafeMutablePointer<CChar>?)

struct Request: Codable, Sendable {
  var op: String = ""
  var payload: String = ""
}

struct Reply: Codable, Sendable {
  var json: String = ""
}

final class AppraisalState: @unchecked Sendable {
  static let shared = AppraisalState()
  private var handled = false
  private let lock = NSLock()

  func claimFirst() -> Bool {
    lock.lock()
    defer { lock.unlock() }
    if handled { return false }
    handled = true
    return true
  }
}

struct AppraisalHandler: XPCPeerHandler {
  func handleIncomingRequest(_ incoming: XPCReceivedMessage) -> (any Encodable)? {
    guard AppraisalState.shared.claimFirst() else {
      // Second request is SecondUse and exit
      DispatchQueue.global().asyncAfter(deadline: .now() + 0.05) { exit(1) }
      return Reply(json: "{\"error\":\"SecondUse\",\"second_use\":true}")
    }

    do {
      let message = try incoming.decode(as: Request.self)
      var replyJson = "{}"
      if Thread.isMainThread {
        replyJson = executeHelper(message.payload)
      } else {
        DispatchQueue.main.sync {
          replyJson = executeHelper(message.payload)
        }
      }
      DispatchQueue.global().asyncAfter(deadline: .now() + 0.1) {
        exit(0)
      }
      return Reply(json: replyJson)
    } catch {
      DispatchQueue.global().asyncAfter(deadline: .now() + 0.05) { exit(1) }
      return Reply(json: "{\"error\":\"invalid request\"}")
    }
  }

  private func executeHelper(_ payload: String) -> String {
    payload.withCString { cStr in
      guard let resPtr = ios_gpu_appraisal_execute(cStr) else {
        return "{\"pid\":\(getpid()),\"returncode\":1,\"result_code\":1,\"result_message\":\"helper returned null\",\"claims\":[],\"detached_eat\":[]}"
      }
      defer { ios_gpu_appraisal_free(resPtr) }
      let raw = String(cString: resPtr)
      if var json = (try? JSONSerialization.jsonObject(with: Data(raw.utf8))) as? [String: Any] {
        json["pid"] = Int(getpid())
        if let data = try? JSONSerialization.data(withJSONObject: json) {
          return String(data: data, encoding: .utf8) ?? raw
        }
      }
      return raw
    }
  }
}

@main struct Appraisal: AppExtension {
  @AppExtensionPoint.Bind var boundExtensionPoint: AppExtensionPoint {
    AppExtensionPoint.Identifier(host: "app.solstone.journal.runtime.dev", name: "gpu-appraisal")
  }
  var configuration: some AppExtensionConfiguration {
    ConnectionHandler(onSessionRequest: { request in request.accept { _ in AppraisalHandler() } })
  }
}
