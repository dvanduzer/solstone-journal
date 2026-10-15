#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Qualify a fresh, separately signed S3 fixture app on an explicitly held phone."""
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

BUNDLE = "app.solstone.journal.backup.s3.dev"
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


def fixture(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    raw = bytearray()
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid() and before.st_nlink == 1 and
                stat.S_IMODE(before.st_mode) == 0o600 and 0 < before.st_size <= 32768, "private bounded fixture required")
        while len(raw) <= 32768:
            block = os.read(fd, 32769 - len(raw))
            if not block:
                break
            raw.extend(block)
        after, named = os.fstat(fd), path.lstat()
        require(len(raw) == before.st_size and len(raw) <= 32768 and
                (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
                (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns) and
                (before.st_mode, before.st_uid, before.st_nlink) == (after.st_mode, after.st_uid, after.st_nlink) and
                (after.st_dev, after.st_ino) == (named.st_dev, named.st_ino), "fixture changed during bounded read")
        digest = hashlib.sha256(raw).hexdigest()
        value = json.loads(raw)
    finally:
        for index in range(len(raw)):
            raw[index] = 0
        os.close(fd)
    fields = {"endpoint", "region", "bucket", "prefix", "access_key", "secret_key", "daily_password", "recovery_password"}
    require(isinstance(value, dict) and fields <= set(value) <= fields | {"session_token"} and
            all(isinstance(v, str) for v in value.values()) and all(value[key] for key in fields), "fixture shape refused")
    return digest, value


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
    names = {"Host/Host.swift", "Host/Lifecycle.swift", "Native/Adapter.m", "Native/Fixture.m", "Native/Lifecycle.m", "Shared/Bridge.h", "Shared/Info.plist", "build.py", "project.yml", "run.py", "run_lifecycle.py"}
    require(set(source["files"]) == names and all(valid_id(value) for value in source["files"].values()), "reviewed source census mismatch")
    require(sha(Path(__file__)) == source["files"]["run.py"], "executing controller is not the reviewed source")
    build = json.loads((work / "build-receipt.json").read_bytes())
    required_steps = ["swift-typecheck", "objc-typecheck-adapter", "objc-typecheck-fixture", "objc-typecheck-lifecycle", "unlock-signing", "project", "app",
                      "signature", "signature-identity", "signature-entitlements"]
    require(build.get("complete") is True and build.get("exit") == 0 and
            [step["name"] for step in build["steps"]] == required_steps and all(step["exit"] == 0 for step in build["steps"]),
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
    require(native.get("status") == "passed" and native.get("complete") is True and native.get("exit") == 0 and native.get("target") == "ios" and
            not native.get("qualification_profile") and native["steps"] and all(step["exit"] == 0 for step in native["steps"]), "independently qualified native receipt required")
    expected = {"probe/" + name: digest for name, digest in source["files"].items()}
    expected.update({"native/receipt.json": native_sha, "native/librestic_bridge.a": native["archive_sha256"], "native/librestic_bridge.h": native["header_sha256"]})
    require(observed == expected and build["immutable_inputs"] == expected and
            build["native_archive_sha256"] == native["archive_sha256"] and build["native_header_sha256"] == native["header_sha256"], "sealed source/native closure mismatch")
    for step in build["steps"]:
        require(sha(work / (step["name"] + ".log")) == step["log_sha256"], "build step log changed")
    app = Path(build["app_path"])
    require(app.resolve() == work / "build/Build/Products/Debug-iphoneos/BackupS3Probe.app", "unexpected app path")
    app_files = {}
    for entry in app.rglob("*"):
        info = entry.lstat()
        require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode), "unexpected signed app object")
        if stat.S_ISREG(info.st_mode):
            app_files[entry.relative_to(app).as_posix()] = sha(entry)
    require(app_files == build["app_files"] and valid_id(app_files.get("BackupS3Probe")), "signed app closure changed")
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


def validate_phase(launch, console, *, phase, nonce, digest, app_sha, native_sha, destination, expected_args):
    require(launch["info"]["outcome"] == "success", "launch failed")
    result = launch["result"]
    require(result["deviceIdentifier"] == DEVICE and result["launchOptions"]["arguments"] == expected_args,
            "launch device/arguments mismatch")
    require(type(result["terminationResult"].get("exitCode")) is int and result["terminationResult"]["exitCode"] == 0 and result["terminationResult"].get("wasCoreDumpCreated") is False,
            "app did not exit successfully")
    pid = result["process"]["processIdentifier"]
    require(type(pid) is int and pid > 0 and console, "missing actual process/evidence")
    require(all(row["nonce"] == nonce and type(row["phase"]) is int and row["phase"] == phase and
                type(row["pid"]) is int and row["pid"] == pid and
                row["fixture_sha256"] in ("", digest) for row in console), "foreign/stale evidence")
    require(console[0]["event"] == "phase.started" and console[-1]["event"] == "phase.finished", "incomplete event sequence")
    require(sum(row["event"] == "phase.started" for row in console) == 1 and
            sum(row["event"] == "phase.finished" for row in console) == 1 and
            all(row["event"] != "host.failed" for row in console), "duplicate/failed phase")
    require(typed_equal(console[0]["value"], {"phase": phase, "executable_sha256": app_sha, "native_archive_sha256": native_sha}), "running build mismatch")
    finish = console[-1]
    require(type(finish["value"]["phase"]) is int and finish["value"]["phase"] == phase and finish["value"]["passed"] is True, "phase did not pass")
    if phase:
        bound = [row for row in console if row["event"] == "fixture.bound"]
        require(len(bound) == 1 and bound[0]["fixture_sha256"] == digest and
                all(bound[0]["value"][key] == value for key, value in destination.items()) and
                bound[0]["value"]["sha256"] == digest and bound[0]["value"]["held_nofollow_bounded_read"] is True,
                "fixture/destination binding mismatch")
        start = console.index(bound[0])
        require(all(row["fixture_sha256"] == digest for row in console[start:]), "unbound operation result")
    else:
        require(all(row["fixture_sha256"] == "" for row in console), "prepare had credentials")
        policy = [row["value"] for row in console if row["event"] == "owner.root-policy"]
        require(len(policy) == 1 and policy[0]["fresh_empty_census"] is True and policy[0]["held_nofollow_roots"] is True and
                policy[0]["excluded_from_backup"] is True and policy[0]["protection"] == "NSFileProtectionComplete", "fresh ingress custody unproved")
    return pid


def validate_operations(first, resumed):
    def result(events, name, status=0, diagnostic=None):
        hits = [row["value"]["result"] for row in events if row["event"] == name]
        require(len(hits) == 1 and type(hits[0]["status"]) is int and hits[0]["status"] == status and
                type(hits[0].get("wait_code")) is int and hits[0]["wait_code"] == 0 and
                type(hits[0].get("free_code")) is int and hits[0]["free_code"] == 0,
                "missing/wrong operation or drain result: " + name)
        if diagnostic is not None:
            require(hits[0]["diagnostic"] == diagnostic, "wrong operation diagnostic: " + name)
        return hits[0]
    daily = result(first, "init")["key_id"]
    recovery = result(first, "recovery.add")["key_id"]
    require(valid_id(daily) and valid_id(recovery) and daily != recovery, "key IDs invalid")
    for events, name, expected in ((first, "daily.open", daily), (first, "recovery.independent-open", recovery),
                                   (resumed, "restart.daily-open", daily), (resumed, "restart.recovery-open", recovery)):
        require(result(events, name)["key_id"] == expected, "key identity mismatch")
    result(first, "owner.inspect-new", diagnostic="S3_STATE_NEW")
    result(resumed, "restart.owner-inspect", diagnostic="S3_STATE_READY")
    result(first, "wrong-key", 2, "AUTH_FAILED")
    result(first, "existing-init.refused", 2, "REPO_EXISTS")
    snapshots = [result(first, name)["snapshot_id"] for name in ("original.snapshot", "changed.snapshot")]
    require(all(valid_id(value) for value in snapshots) and len(set(snapshots)) == 2, "snapshot IDs invalid")
    for events, name in ((first, "snapshots.list"), (resumed, "restart.list")):
        ids = result(events, name)["snapshot_ids"]
        require(len(ids) == 2 and set(ids) == set(snapshots), "snapshot list mismatch")
    for events, names in ((first, ("original.restore", "changed.restore", "full-integrity")),
                           (resumed, ("restart.restore", "restart.full-integrity"))):
        for name in names:
            result(events, name)
    for events, name in ((first, "original.bytes"), (first, "changed.bytes"), (resumed, "restart.bytes")):
        require(sum(row["event"] == name for row in events) == 1 and
                sum(row["event"] == name + ".metadata" for row in events) == 1, "missing restore byte/metadata proof")
    require(sum(row["event"] == "nonempty.restore.refused" and row["value"]["observed"] is True for row in first) == 1,
            "nonempty restore refusal missing")
    return {"daily_key_id": daily, "recovery_key_id": recovery, "snapshot_ids": snapshots}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-work", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--holder", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--source-record", type=Path, required=True)
    parser.add_argument("--source-record-sha256", required=True)
    parser.add_argument("--native-receipt-sha256", required=True)
    parser.add_argument("--build-receipt-sha256", required=True)
    args = parser.parse_args()
    holder = Path("/var/tmp/android-bench-hilphone16.lock/holder").read_text()
    held = reservation(holder, args.holder, args.session)
    build_work, fixture_path = args.build_work.resolve(), args.fixture.absolute()
    build, app = validate_build(build_work, args.source_record, args.source_record_sha256, args.native_receipt_sha256, args.build_receipt_sha256)
    digest, value = fixture(fixture_path)
    destination = {name: value[name] for name in ("endpoint", "region", "bucket", "prefix")}
    destination["access_key_id"] = value["access_key"]
    nonce = secrets.token_hex(16)
    work = args.work_dir.resolve()
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(Path.home()), "USER": os.environ["USER"],
           "LOGNAME": os.environ["USER"], "LANG": "en_US.UTF-8", "LC_ALL": "en_US.UTF-8", "TMPDIR": str(work)}
    env["DEVELOPER_DIR"] = subprocess.check_output(["/usr/bin/xcode-select", "-p"], env=env, text=True).strip()
    tool = Path(subprocess.check_output(["/usr/bin/xcrun", "--find", "devicectl"], env=env, text=True).strip()).resolve()
    record = {"complete": False, "exit": None, "nonce": nonce, "fixture_sha256": digest, "destination": destination, "reservation": held,
              "reviewed_source_record_sha256": args.source_record_sha256,
              "build_receipt_sha256": sha(build_work / "build-receipt.json"), "native_receipt_sha256": build["native_receipt_sha256"],
              "native_archive_sha256": build["native_archive_sha256"], "app_files": build["app_files"], "steps": [],
              "controller_sha256": sha(Path(__file__)), "devicectl": {"path": str(tool), "sha256": sha(tool)},
              "boundary": "Fresh separate synthetic fixture app. No normal app, operated create-only, interruption, pending, rotation, teardown or pre-secret temporary-file result is inferred."}

    def save():
        record["clock_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        (work / "receipt.json").write_text(json.dumps(record, indent=2) + "\n")

    def run(name, command, timeout=180):
        require(reservation(Path("/var/tmp/android-bench-hilphone16.lock/holder").read_text(), args.holder, args.session) == held,
                "phone reservation changed during qualification")
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
                raise RuntimeError("local controller timed out; phone operation custody remains unknown, no automatic retry")
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
        require(all(item["bundleIdentifier"] != BUNDLE for item in inventory["result"]["apps"]), "qualification app/container must be fresh")
        run("install", ["device", "install", "app", "--device", DEVICE, app])
        inventory, _ = run("apps-installed", ["device", "info", "apps", "--device", DEVICE])
        installed = [item for item in inventory["result"]["apps"] if item["bundleIdentifier"] == BUNDLE]
        require(len(installed) == 1 and installed[0]["builtByDeveloper"] is True, "installed app identity missing")
        cumulative, phases, pids = [], {}, []
        for phase in (0, 1, 2):
            if phase == 1:
                rechecked_digest, rechecked = fixture(fixture_path)
                require(rechecked_digest == digest and rechecked == value, "fixture changed before transfer")
                run("copy-fixture", ["device", "copy", "to", "--device", DEVICE, "--domain-type", "appDataContainer", "--domain-identifier", BUNDLE,
                                     "--source", fixture_path, "--destination", "Documents/remote-input/fixture.json"])
            arguments = ["--nonce", nonce] + (["--prepare"] if phase == 0 else ["--fixture-sha256", digest] + (["--resume"] if phase == 2 else []))
            name = "phase" + str(phase)
            launch, log = run(name, ["device", "process", "launch", "--device", DEVICE, "--console", "--terminate-existing", "--timeout", "600",
                                     BUNDLE, *arguments], timeout=630)
            current = rows(log)
            pid = validate_phase(launch, current, phase=phase, nonce=nonce, digest=digest, app_sha=build["app_files"]["BackupS3Probe"],
                                 native_sha=build["native_archive_sha256"], destination=destination, expected_args=arguments)
            require(pid not in pids, "fresh process PID required for each phase")
            pids.append(pid)
            executable = launch["result"]["process"]["executable"]
            require(executable == installed[0]["url"] + "BackupS3Probe", "launched installed app URL mismatch")
            phases[phase] = current
            cumulative.extend(current)
            durable = work / (name + "-evidence.jsonl")
            run(name + "-copy-evidence", ["device", "copy", "from", "--device", DEVICE, "--domain-type", "appDataContainer", "--domain-identifier", BUNDLE,
                "--source", "Documents/backup-s3-evidence.jsonl", "--destination", durable])
            require(typed_equal(rows(durable), cumulative), "console/durable evidence mismatch or stale rows")
            for sensitive in (value["secret_key"], value["daily_password"], value["recovery_password"], value.get("session_token", "")):
                if sensitive:
                    require(sensitive.encode() not in log.read_bytes() and sensitive.encode() not in durable.read_bytes(), "secret in evidence")
        record["results"] = validate_operations(phases[1], phases[2])
        require(type(phases[2][-1]["value"]["first_pid"]) is int and
                phases[2][-1]["value"]["first_pid"] == pids[1], "restart did not bind previous process")
        require(sha(tool) == record["devicectl"]["sha256"] and sha(build_work / "build-receipt.json") == record["build_receipt_sha256"], "execution inputs changed")
        validate_build(build_work, args.source_record, args.source_record_sha256, args.native_receipt_sha256, args.build_receipt_sha256)
        record.update(complete=True, exit=0, pids=pids)
        save()
        return 0
    except Exception:
        record.update(complete=False, exit=1)
        save()
        raise


if __name__ == "__main__":
    sys.exit(main())
