#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Prepare a separate iOS diagnostic clone with a synchronized native witness."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess


def replace_once(path, before, after):
    source = path.read_text()
    if source.count(before) != 1:
        raise ValueError(f"expected one source seam in {path}")
    path.write_text(source.replace(before, after))


def prepare(source, commit, work, fixture):
    source = source.resolve()
    work = work.resolve()
    if source == work or source in work.parents:
        raise ValueError("diagnostic destination must be outside the source tree")
    if work.exists():
        raise ValueError("diagnostic destination must be absent")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
    dirty = subprocess.check_output(["git", "status", "--porcelain"], cwd=source, text=True)
    if head != commit or dirty:
        raise ValueError("source must be clean at the explicitly selected commit")
    with fixture.open("rb") as stream:
        payload = stream.read(1_114_113)
    if len(payload) > 1_114_112 or not isinstance(json.loads(payload), dict):
        raise ValueError("fixture must be a bounded JSON request object")
    tool_root = Path(__file__).resolve().parents[2]
    if source != tool_root:
        raise ValueError("preparer must belong to the selected clean source commit")
    subprocess.run(["git", "clone", "--no-checkout", "--no-hardlinks", str(source), str(work)], check=True)
    subprocess.run(["git", "checkout", "--detach", commit], cwd=work, check=True)
    runtime = work / "tools/ios-runtime"
    assets = runtime / "diagnostics"
    shutil.copyfile(assets / "VerifierQualification.swift", runtime / "Host/VerifierQualification.swift")
    host = runtime / "Host/Host.swift"
    startup = "    UIApplication.shared.isIdleTimerDisabled = true\n    UIDevice.current.isBatteryMonitoringEnabled = true"
    replace_once(host, startup, startup + '''
    if args.contains("--verifier-lifecycle") {
      do {
        try await qualifyVerifierLifecycle()
        _exit(0)
      } catch {
        log("verifier.lifecycle.failed", ["error": String(describing: error)])
        _exit(1)
      }
    }''')
    helper = runtime / "Appraisal/Appraisal.swift"
    replace_once(helper, "private let appGroup", '''@_silgen_name("ios_gpu_appraisal_observation_path")
func ios_gpu_appraisal_observation_path(_ path: UnsafePointer<CChar>)

private let appGroup''')
    replace_once(helper, "    let result = executeHelper(message.payload, attempt: message.bridgeAttempt)", '''    guard let group = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup) else {
      return refusal("observation path missing", attempt: message.bridgeAttempt, exitCode: 126)
    }
    group.appendingPathComponent("gpu-appraisal-native-observation.bin").path.withCString {
      ios_gpu_appraisal_observation_path($0)
    }
    let result = executeHelper(message.payload, attempt: message.bridgeAttempt)''')
    native = runtime / "appraisal-helper/src/lib.rs"
    replace_once(native, "let request = match serde_json::from_str(input)", "let request: AppraisalRequestPayload = match serde_json::from_str(input)")
    replace_once(native, "    match run_appraisal(request) {", '''    let observation = NativeObservation::new(request.bridge_attempt);
    let output = match run_appraisal(request) {''')
    replace_once(native, "        )),\n    }\n}\n\n/// # Safety", "        )),\n    };\n    observation.returned();\n    output\n}\n\n/// # Safety")
    native.write_text(native.read_text() + "\n" + (assets / "lifecycle_observer.rs").read_text())
    objc = runtime / "Shared/Native.m"
    objc.write_text(objc.read_text() + "\n" + (assets / "lifecycle_observer.c").read_text())
    (work / "verifier-fixture.json").write_bytes(payload)
    names = ["Host/Host.swift", "Host/VerifierQualification.swift", "Appraisal/Appraisal.swift", "appraisal-helper/src/lib.rs", "Shared/Native.m"]
    record = {"source_commit": commit, "diagnostic_source": [
        {"path": name, "sha256": hashlib.sha256((runtime / name).read_bytes()).hexdigest()}
        for name in names
    ], "fixture_sha256": hashlib.sha256(payload).hexdigest(),
        "boundary": "Observation derivative; production verifier policy and native inputs remain unchanged."}
    (work / "lifecycle-source.json").write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(prepare(args.source, args.commit, args.work, args.fixture), sort_keys=True))


if __name__ == "__main__":
    main()
