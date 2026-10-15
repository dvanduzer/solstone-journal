#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Qualify a fresh offline owner-policy app on an explicitly held phone."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import secrets
import signal
import stat
import subprocess
import sys

BUNDLE = "app.solstone.journal.backup.s3.ownerpolicy2.dev"
DEVICE = "1776B0A9-E149-52A1-9F6F-04CCDE223940"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require(value, reason):
    if not value:
        raise ValueError(reason)


def exact_code(value, expected):
    return type(value) is int and value == expected


def typed_equal(left, right):
    options = {"sort_keys": True, "separators": (",", ":"), "ensure_ascii": True, "allow_nan": False}
    return json.dumps(left, **options) == json.dumps(right, **options)


def valid_id(value):
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)


def reservation(text, expected, session):
    require(len(session) == 8 and session.isascii() and session.isalnum() and expected.startswith(session + " ") and
            "\n" not in expected and " since " not in expected, "full session and holder required")
    require(text.endswith("\n") and text.count("\n") == 1, "malformed phone reservation")
    actual, separator, since = text.rstrip("\n").rpartition(" since ")
    require(separator and actual == expected, "exact phone holder mismatch")
    clock = datetime.datetime.fromisoformat(since)
    require(clock.utcoffset() is not None, "reservation timestamp requires timezone")
    return {"device": DEVICE, "holder": actual, "since": since, "session": session}


def validate_build(work, source_record, source_sha, native_sha, build_sha, production_sha):
    require(all(valid_id(value) for value in (source_sha, native_sha, build_sha, production_sha)), "independent evidence hashes required")
    require(sha(source_record) == source_sha and sha(work / "build-receipt.json") == build_sha, "reviewed source/build receipt mismatch")
    source = json.loads(source_record.read_bytes())
    names = {"Host/Host.swift", "Native/Adapter.m", "Shared/Bridge.h", "Shared/Info.plist", "build.py", "project.yml", "run.py", "build_native.py", "owner_policy_probe.go.fixture"}
    require(set(source["files"]) == names and all(valid_id(value) for value in source["files"].values()), "reviewed source census mismatch")
    require(sha(Path(__file__)) == source["files"]["run.py"], "executing controller is not the reviewed source")
    build = json.loads((work / "build-receipt.json").read_bytes())
    required_steps = ["swift-typecheck", "objc-typecheck-adapter", "unlock-signing", "project", "app",
                      "signature", "signature-identity", "signature-entitlements"]
    require(build.get("complete") is True and exact_code(build.get("exit"), 0) and
            [step["name"] for step in build["steps"]] == required_steps and all(exact_code(step["exit"], 0) for step in build["steps"]),
            "complete exact signed build steps required")
    require(build["team"] == "7QCG8V4M6H" and build["builder_sha256"] == source["files"]["build.py"] and
            build["native_receipt_sha256"] == native_sha, "builder/team/native authority mismatch")
    require(build.get("signing_identity") == {"Identifier":BUNDLE, "TeamIdentifier":"7QCG8V4M6H"} and
            build.get("signing_entitlements", {}).get("application-identifier") == "7QCG8V4M6H." + BUNDLE and
            build["signing_entitlements"].get("com.apple.developer.team-identifier") == "7QCG8V4M6H" and
            not build["signing_entitlements"].get("com.apple.security.application-groups"), "measured signing identity/entitlements mismatch")
    inputs = work / "immutable-input"
    observed = {}
    for entry in [inputs, *inputs.rglob("*")]:
        info = entry.lstat()
        require(not info.st_mode & 0o222 and (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)), "sealed input object changed")
        if stat.S_ISREG(info.st_mode):
            observed[entry.relative_to(inputs).as_posix()] = sha(entry)
    native = json.loads((inputs / "native/receipt.json").read_bytes())
    require(native.get("status") == "passed" and native.get("complete") is True and exact_code(native.get("exit"), 0) and native.get("target") == "ios" and
            native["steps"] and all(exact_code(step["exit"], 0) for step in native["steps"]), "independently qualified native receipt required")
    require(build.get("qualification_profile") == "owner-policy-probe" and build.get("production_receipt_sha256") == production_sha and
            native.get("qualification_profile") == "owner-policy-probe" and native.get("production_receipt_sha256") == production_sha and
            native.get("observer_sha256") == source["files"]["owner_policy_probe.go.fixture"] and
            native.get("controller_sha256") == source["files"]["build_native.py"], "private qualification profile/provenance mismatch")
    private_files = dict(native["production_source_files"])
    require("mobilebackup/owner_policy_probe.go" not in private_files, "observer is already present in production source")
    private_files["mobilebackup/owner_policy_probe.go"] = native["observer_sha256"]
    require(native["private_source_files"] == private_files, "private source delta exceeds the one observer")
    expected = {"probe/" + name: digest for name, digest in source["files"].items()}
    expected.update({"native/receipt.json": native_sha, "native/librestic_bridge.a": native["archive_sha256"], "native/librestic_bridge.h": native["header_sha256"]})
    require(observed == expected and build["immutable_inputs"] == expected and
            build["native_archive_sha256"] == native["archive_sha256"] and build["native_header_sha256"] == native["header_sha256"], "sealed source/native closure mismatch")
    for step in build["steps"]:
        require(sha(work / (step["name"] + ".log")) == step["log_sha256"], "build step log changed")
    app = Path(build["app_path"])
    require(app.resolve() == work / "build/Build/Products/Debug-iphoneos/BackupS3OwnerPolicyProbe.app", "unexpected app path")
    app_files = {}
    for entry in app.rglob("*"):
        info = entry.lstat()
        require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode), "unexpected signed app object")
        if stat.S_ISREG(info.st_mode):
            app_files[entry.relative_to(app).as_posix()] = sha(entry)
    require(app_files == build["app_files"] and valid_id(app_files.get("BackupS3OwnerPolicyProbe")), "signed app closure changed")
    return build, app


def validate_result(row, *, nonce, pid, app_sha, native_sha):
    require(row.get("nonce") == nonce and exact_code(row.get("pid"), pid) and row.get("executable_sha256") == app_sha and
            row.get("native_archive_sha256") == native_sha and row.get("qualification_kind") == "private-owner-policy",
            "phone evidence identity mismatch")
    result = row["result"]
    require(result.get("passed") is True and result.get("reason") == "" and exact_code(result.get("actual_writer_status"), 0),
            "actual policy controls failed")
    primitives = result["primitives"]
    require([item["kind"] for item in primitives] == ["missing-exclusion", "wrong-protection", "wrong-exclusion"] and
            all(all(item.get(key) is True for key in ("bad_policy_refused", "corrected_policy_passed", "empty_before_and_after", "closed", "passed"))
                for item in primitives), "independent empty primitive controls missing")
    roots = result["root_refusals"]
    require([item["kind"] for item in roots] == ["root-exclusion", "root-protection"] and
            all(exact_code(item["go_admission_status"], 2) and all(item.get(key) is True for key in
                ("native_policy_refused", "go_admission_refused", "no_state_or_temp_written", "closed", "passed")) for item in roots),
            "actual owner root-policy refusal missing")
    writer = result["actual_writer"]
    require(writer.get("passed") is True and writer.get("reason") == "" and len(writer["commits"]) == 2,
            "actual initial/replacement persistence missing")
    identities = []
    for commit in writer["commits"]:
        before = (commit["before_device"], commit["before_inode"])
        after = (commit["after_device"], commit["after_inode"])
        require(all(type(value) is int and value > 0 for value in before + after) and before == after and
                exact_code(commit["before_size"], 0) and type(commit["after_size"]) is int and commit["after_size"] > 0 and
                commit["policy_read_only"] is True, "empty pre-secret policy/same-inode publication unproved")
        identities.append(before)
    require(identities[0] != identities[1], "replacement did not use a fresh inode")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-work", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--holder", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--source-record", type=Path, required=True)
    parser.add_argument("--source-record-sha256", required=True)
    parser.add_argument("--native-receipt-sha256", required=True)
    parser.add_argument("--production-receipt-sha256", required=True)
    parser.add_argument("--build-receipt-sha256", required=True)
    args = parser.parse_args()
    lock = Path("/var/tmp/android-bench-hilphone16.lock/holder")
    held = reservation(lock.read_text(), args.holder, args.session)
    build_work = args.build_work.resolve()
    anchors = (args.source_record, args.source_record_sha256, args.native_receipt_sha256,
               args.build_receipt_sha256, args.production_receipt_sha256)
    build, app = validate_build(build_work, *anchors)
    work = args.work_dir.resolve()
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    nonce = secrets.token_hex(16)
    env = {"PATH":"/usr/bin:/bin:/usr/sbin:/sbin", "HOME":str(Path.home()), "USER":os.environ["USER"],
           "LOGNAME":os.environ["USER"], "LANG":"en_US.UTF-8", "LC_ALL":"en_US.UTF-8", "TMPDIR":str(work)}
    env["DEVELOPER_DIR"] = subprocess.check_output(["/usr/bin/xcode-select", "-p"], env=env, text=True).strip()
    tool = Path(subprocess.check_output(["/usr/bin/xcrun", "--find", "devicectl"], env=env, text=True).strip()).resolve()
    record = {"complete":False, "exit":None, "nonce":nonce, "reservation":held,
              "source_record_sha256":args.source_record_sha256, "build_receipt_sha256":args.build_receipt_sha256,
              "native_receipt_sha256":args.native_receipt_sha256, "production_receipt_sha256":args.production_receipt_sha256,
              "native_archive_sha256":build["native_archive_sha256"], "app_files":build["app_files"],
              "controller_sha256":sha(Path(__file__)), "devicectl":{"path":str(tool), "sha256":sha(tool)}, "steps":[],
              "boundary":"Offline private policy observer only; synthetic local constants. No credentials, backend, production admission, normal app, cancellation or remote result."}

    def save():
        record["clock_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        (work / "receipt.json").write_text(json.dumps(record, indent=2) + "\n")

    def run(name, command, timeout=180):
        require(reservation(lock.read_text(), args.holder, args.session) == held, "phone reservation changed")
        output, report = work / (name + ".log"), work / (name + ".json")
        arguments = list(map(str, command))
        argv = [str(tool), *arguments[:3], "--json-output", str(report), *arguments[3:]]
        with output.open("xb") as stream:
            child = subprocess.Popen(argv, env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                code = child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                record["unknown_phone_process_after_timeout"] = True
                save()
                raise RuntimeError("controller timed out; phone custody remains unknown, no automatic retry")
        record["steps"].append({"name":name, "argv":argv, "exit":code, "log_sha256":sha(output),
                                "json_sha256":sha(report) if report.exists() else None})
        save()
        print(name + " EXIT=" + str(code), flush=True)
        require(code == 0 and report.exists(), "device command failed: " + name)
        value = json.loads(report.read_bytes())
        require(value["info"]["outcome"] == "success", "device command outcome failed: " + name)
        return value, output

    save()
    try:
        inventory, _ = run("apps-before", ["device", "info", "apps", "--device", DEVICE])
        require(all(item["bundleIdentifier"] != BUNDLE for item in inventory["result"]["apps"]), "policy app/container must be fresh")
        run("install", ["device", "install", "app", "--device", DEVICE, app])
        inventory, _ = run("apps-installed", ["device", "info", "apps", "--device", DEVICE])
        installed = [item for item in inventory["result"]["apps"] if item["bundleIdentifier"] == BUNDLE]
        require(len(installed) == 1 and installed[0]["builtByDeveloper"] is True, "installed identity missing")
        arguments = ["--nonce", nonce]
        record["unknown_phone_process_after_launch"] = True
        save()
        launch, log = run("policy", ["device", "process", "launch", "--device", DEVICE, "--console", "--terminate-existing",
                                     "--timeout", "120", BUNDLE, *arguments], timeout=150)
        actual = launch["result"]
        require(actual["deviceIdentifier"] == DEVICE and actual["launchOptions"]["arguments"] == arguments,
                "actual app launch mismatch")
        pid = actual["process"]["processIdentifier"]
        require(type(pid) is int and pid > 0 and actual["process"]["executable"] == installed[0]["url"] + "BackupS3OwnerPolicyProbe",
                "actual app PID/URL mismatch")
        termination = actual["terminationResult"]
        require(type(termination.get("exitCode")) is int and type(termination.get("wasCoreDumpCreated")) is bool,
                "matching actual terminal result missing")
        record["unknown_phone_process_after_launch"] = False
        record["terminal_process"] = {"pid":pid, "termination":termination}
        save()
        require(termination["exitCode"] == 0 and termination["wasCoreDumpCreated"] is False, "actual app exit failed")
        rows = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
        require(len(rows) == 1, "one current evidence row required")
        result = validate_result(rows[0], nonce=nonce, pid=pid, app_sha=build["app_files"]["BackupS3OwnerPolicyProbe"],
                                 native_sha=build["native_archive_sha256"])
        durable = work / "policy-evidence.json"
        run("copy-evidence", ["device", "copy", "from", "--device", DEVICE, "--domain-type", "appDataContainer", "--domain-identifier", BUNDLE,
                              "--source", "Documents/owner-policy-evidence.json", "--destination", durable])
        require(typed_equal(json.loads(durable.read_bytes()), rows[0]), "console/durable evidence mismatch")
        require(sha(tool) == record["devicectl"]["sha256"], "device tool changed")
        validate_build(build_work, *anchors)
        record.update(complete=True, exit=0, pid=pid, results=result, durable_evidence_sha256=sha(durable))
        save()
        return 0
    except Exception:
        record.update(complete=False, exit=1)
        save()
        raise


if __name__ == "__main__":
    sys.exit(main())
