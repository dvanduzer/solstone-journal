#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Run the reviewed existing-root C client in a fresh, isolated phone app."""
import sys
if not sys.flags.isolated:
    raise SystemExit("invoke this controller with python3 -I")

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

BUNDLE = "app.solstone.journal.backup.rooted.dev"
DEVICE = "1776B0A9-E149-52A1-9F6F-04CCDE223940"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require(value, reason):
    if not value:
        raise ValueError(reason)


def typed_equal(left, right):
    # Python container equality conflates JSON booleans with numbers.
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


def validate_build(work, source_record, source_sha, native_sha, build_sha):
    require(all(valid_id(value) for value in (source_sha, native_sha, build_sha)), "independent evidence hashes required")
    require(sha(source_record) == source_sha and sha(work / "build-receipt.json") == build_sha, "reviewed source/build receipt mismatch")
    source = json.loads(source_record.read_bytes())
    names = {"Host/Host.swift", "Native/Adapter.m", "Native/Local.m", "Shared/Bridge.h", "Shared/Info.plist", "build.py", "project.yml", "run.py"}
    require(set(source["files"]) == names and all(valid_id(value) for value in source["files"].values()), "reviewed source census mismatch")
    require(sha(Path(__file__)) == source["files"]["run.py"], "executing controller is not the reviewed source")
    build = json.loads((work / "build-receipt.json").read_bytes())
    required_steps = ["swift-typecheck", "objc-typecheck-adapter", "objc-typecheck-local", "unlock-signing", "project", "app",
                      "signature", "signature-identity", "signature-entitlements"]
    require(build.get("complete") is True and type(build.get("exit")) is int and build["exit"] == 0 and
            [step["name"] for step in build["steps"]] == required_steps and
            all(type(step.get("exit")) is int and step["exit"] == 0 for step in build["steps"]),
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
    require(native.get("status") == "passed" and native.get("complete") is True and
            type(native.get("exit")) is int and native["exit"] == 0 and native.get("target") == "ios" and
            not native.get("qualification_profile") and native["steps"] and
            all(type(step.get("exit")) is int and step["exit"] == 0 for step in native["steps"]), "independently qualified native receipt required")
    expected = {"probe/" + name: digest for name, digest in source["files"].items()}
    expected.update({"native/receipt.json": native_sha, "native/librestic_bridge.a": native["archive_sha256"], "native/librestic_bridge.h": native["header_sha256"]})
    require(observed == expected and build["immutable_inputs"] == expected and
            build["native_archive_sha256"] == native["archive_sha256"] and build["native_header_sha256"] == native["header_sha256"], "sealed source/native closure mismatch")
    for step in build["steps"]:
        require(sha(work / (step["name"] + ".log")) == step["log_sha256"], "build step log changed")
    app = Path(build["app_path"])
    require(app.resolve() == work / "build/Build/Products/Debug-iphoneos/BackupRootedProbe.app", "unexpected app path")
    app_files = {}
    for entry in app.rglob("*"):
        info = entry.lstat()
        require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode), "unexpected signed app object")
        if stat.S_ISREG(info.st_mode):
            app_files[entry.relative_to(app).as_posix()] = sha(entry)
    require(app_files == build["app_files"] and valid_id(app_files.get("BackupRootedProbe")), "signed app closure changed")
    return build, app


def rows(log):
    values = []
    for line in log.read_text(errors="strict").splitlines():
        if not line.startswith("{"):
            continue
        value = json.loads(line)
        if isinstance(value, dict) and "event" in value:
            values.append(value)
    return values


def validate_phase(launch, events, *, arguments, nonce, build, installed):
    require(launch["info"]["outcome"] == "success", "launch failed")
    result = launch["result"]
    require(result["deviceIdentifier"] == DEVICE and result["launchOptions"]["arguments"] == arguments,
            "launch device/arguments mismatch")
    termination = result["terminationResult"]
    require(type(termination.get("exitCode")) is int and termination["exitCode"] == 0 and
            termination.get("wasCoreDumpCreated") is False, "app did not exit successfully")
    pid = result["process"]["processIdentifier"]
    require(type(pid) is int and pid > 0 and result["process"]["executable"] == installed["url"] + "BackupRootedProbe",
            "launched installed process identity mismatch")
    require([row["event"] for row in events] == ["phase.started", "phase.finished"], "incomplete or failed evidence")
    require(all(row["nonce"] == nonce and type(row["pid"]) is int and row["pid"] == pid and
                row["executable_sha256"] == build["app_files"]["BackupRootedProbe"] and
                row["native_archive_sha256"] == build["native_archive_sha256"] for row in events),
            "foreign/stale/build-mismatched evidence")
    started, finished = (row["value"] for row in events)
    require(started["fixture_root"].endswith("/Documents/rooted-local-" + nonce) and
            started["excluded_from_backup"] is True and started["protection"] == "NSFileProtectionComplete",
            "fresh fixture policy unproved")
    require(type(finished["caller_exit"]) is int and finished["caller_exit"] == 0 and finished["passed"] is True,
            "actual exported C client failed")
    return pid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-work", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--holder", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--source-record", type=Path, required=True)
    parser.add_argument("--source-record-sha256", required=True)
    parser.add_argument("--native-receipt-sha256", required=True)
    parser.add_argument("--build-receipt-sha256", required=True)
    args = parser.parse_args()
    lock = Path("/var/tmp/android-bench-hilphone16.lock/holder")
    held = reservation(lock.read_text(), args.holder, args.session)
    build_work = args.build_work.resolve()
    build, app = validate_build(build_work, args.source_record, args.source_record_sha256,
                                args.native_receipt_sha256, args.build_receipt_sha256)
    nonce = secrets.token_hex(16)
    work = args.work_dir.resolve()
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(Path.home()), "USER": os.environ["USER"],
           "LOGNAME": os.environ["USER"], "LANG": "en_US.UTF-8", "LC_ALL": "en_US.UTF-8", "TMPDIR": str(work)}
    env["DEVELOPER_DIR"] = subprocess.check_output(["/usr/bin/xcode-select", "-p"], env=env, text=True).strip()
    tool = Path(subprocess.check_output(["/usr/bin/xcrun", "--find", "devicectl"], env=env, text=True).strip()).resolve()
    record = {"complete": False, "exit": None, "nonce": nonce, "reservation": held,
              "reviewed_source_record_sha256": args.source_record_sha256,
              "build_receipt_sha256": sha(build_work / "build-receipt.json"), "native_receipt_sha256": build["native_receipt_sha256"],
              "native_archive_sha256": build["native_archive_sha256"], "app_files": build["app_files"], "steps": [],
              "controller_sha256": sha(Path(__file__)), "devicectl": {"path": str(tool), "sha256": sha(tool)},
              "boundary": "Separate synthetic local existing-root C client. No provider, normal app, operated, interruption, pending, rotation, teardown or retained namespace qualification is inferred."}

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
                raise RuntimeError("controller timeout; phone custody unknown, no automatic retry")
        record["steps"].append({"name": name, "argv": argv, "exit": code, "log_sha256": sha(output),
                                "json_sha256": sha(report) if report.exists() else None})
        save()
        print(name + " EXIT=" + str(code), flush=True)
        require(code == 0 and report.exists(), "device command failed: " + name)
        result = json.loads(report.read_bytes())
        require(result["info"]["outcome"] == "success", "device command outcome failed: " + name)
        return result, output

    save()
    try:
        inventory, _ = run("apps-before", ["device", "info", "apps", "--device", DEVICE])
        require(all(item["bundleIdentifier"] != BUNDLE for item in inventory["result"]["apps"]), "fresh separate app/container required")
        run("install", ["device", "install", "app", "--device", DEVICE, app])
        inventory, _ = run("apps-installed", ["device", "info", "apps", "--device", DEVICE])
        installed = [item for item in inventory["result"]["apps"] if item["bundleIdentifier"] == BUNDLE]
        require(len(installed) == 1 and installed[0]["builtByDeveloper"] is True, "installed app identity missing")
        arguments = ["--run", "--nonce", nonce]
        launch, log = run("local-restore", ["device", "process", "launch", "--device", DEVICE, "--console", "--timeout", "300",
                                           BUNDLE, *arguments], timeout=330)
        current = rows(log)
        pid = validate_phase(launch, current, arguments=arguments, nonce=nonce, build=build, installed=installed[0])
        durable = work / "local-restore-evidence.jsonl"
        run("copy-evidence", ["device", "copy", "from", "--device", DEVICE, "--domain-type", "appDataContainer",
             "--domain-identifier", BUNDLE, "--source", "Documents/backup-rooted-evidence.jsonl", "--destination", durable])
        require(typed_equal(rows(durable), current), "console/durable evidence mismatch")
        require(sha(tool) == record["devicectl"]["sha256"], "execution tool changed")
        validate_build(build_work, args.source_record, args.source_record_sha256,
                       args.native_receipt_sha256, args.build_receipt_sha256)
        require(reservation(lock.read_text(), args.holder, args.session) == held, "phone reservation changed")
        record.update(complete=True, exit=0, pid=pid)
        save()
        return 0
    except Exception:
        record.update(complete=False, exit=1)
        save()
        raise


if __name__ == "__main__":
    sys.exit(main())
