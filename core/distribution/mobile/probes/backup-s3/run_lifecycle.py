#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Continue a retained S3 fixture through explicit cancellation and process death."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import uuid

import run as base

require, sha = base.require, base.sha
BUNDLE, DEVICE = base.BUNDLE, base.DEVICE


def one(events, name):
    found = [row["value"] for row in events if row.get("event") == name]
    require(len(found) == 1, "one current event required: " + name)
    return found[0]


def exact_code(value, expected):
    return type(value) is int and value == expected


def ordered(events, names):
    positions = []
    for name in names:
        matches = [index for index, row in enumerate(events) if row.get("event") == name]
        require(len(matches) == 1, "one ordered event required: " + name)
        positions.append(matches[0])
    require(positions == sorted(positions), "actual operation evidence out of order")


def same_handle(value, expected):
    return type(value) is int and type(expected) is int and value > 0 and expected > 0 and value == expected


def intent_id(value):
    return isinstance(value, str) and len(value) == 32 and all(c in "0123456789abcdef" for c in value)


def operation(events, name, status=0, diagnostic=None):
    outer = one(events, name)
    result = outer["result"]
    require(type(result.get("status")) is int and result["status"] == status and
            exact_code(result.get("wait_code"), 0) and exact_code(result.get("free_code"), 0), "operation/drain mismatch: " + name)
    if diagnostic is not None:
        require(result.get("diagnostic") == diagnostic, "operation reason mismatch: " + name)
    return result


def observation(value, *, write, previous=None):
    active = value.get("active")
    require(isinstance(active, dict) and intent_id(active.get("id")) and
            type(active.get("operation")) is int and active["operation"] == (4 if write else 2) and
            value.get("current_credential_present") is True and value.get("current_password_present") is True and
            base.valid_id(value.get("record_sha256")) and base.valid_id(value.get("recovery_material_sha256")),
            "actual new intent/material observation missing")
    if previous is None:
        require(intent_id(value.get("armed_previous_id")) and active["id"] != value["armed_previous_id"], "watcher did not bind prior completed operation")
    if previous is not None:
        require(active["id"] == previous["active"]["id"] and
                value["recovery_material_sha256"] == previous["recovery_material_sha256"], "retained intent/material changed")
    if write:
        require(active.get("may_have_written") is True and isinstance(active.get("locks"), list) and
                active["locks"] and all(base.valid_id(v) for v in active["locks"]),
                "write admission/lock intent unexercised")
    return active


def retry(events, label, baseline, allow_new=False):
    operation(events, label + ".inspect", diagnostic="S3_STATE_READY")
    require(operation(events, label + ".good-open")["key_id"] == baseline["daily_key_id"], "daily identity changed")
    ids = operation(events, label + ".list")["snapshot_ids"]
    require(isinstance(ids, list) and all(base.valid_id(v) for v in ids) and len(set(ids)) == len(ids) and
            set(baseline["snapshot_ids"]) <= set(ids) and (allow_new or set(ids) == set(baseline["snapshot_ids"])),
            "retry snapshot identity mismatch")
    operation(events, label + ".integrity")
    return ids


def pending(events, label, prior):
    ordered(events, [label + ".inspect", label + ".open-refused", label + ".retained", "phase.finished"])
    operation(events, label + ".inspect", 11, "REMOTE_PENDING")
    operation(events, label + ".open-refused", 11, "REMOTE_PENDING")
    retained = one(events, label + ".retained")["observation"]
    observation(retained, write=True, previous=prior)
    return retained


def validate_controls(events, phase, baseline, killed=None):
    require(not any(row["event"] in ("host.failed", "handle.wait-failed") or
                    "unexercised" in row["event"] or "failed-control" in row["event"] for row in events),
            "failed or unexercised control")
    if phase == 3:
        ordered(events, ["lifecycle.initial-inspect", "credentials.wrong-secret", "credentials.good-secret", "cancel.read.started",
                         "cancel.read.intent", "cancel.read.live", "cancel.read.finished", "cancel.read.retry.inspect",
                         "cancel.read.retry.good-open", "cancel.read.retry.list", "cancel.read.retry.integrity", "phase.finished"])
        operation(events, "lifecycle.initial-inspect", diagnostic="S3_STATE_READY")
        operation(events, "credentials.wrong-secret", 2, "AUTH_FAILED")
        require(operation(events, "credentials.good-secret")["key_id"] == baseline["daily_key_id"], "credential recovery changed identity")
        live, finished = one(events, "cancel.read.live"), one(events, "cancel.read.finished")
        observed = one(events, "cancel.read.intent")
        observation(observed["observation"], write=False)
        handle = observed["handle"]
        require(type(handle) is int and handle > 0 and observed.get("observer_closed") is True and
                same_handle(one(events, "cancel.read.started")["handle"], handle) and
                same_handle(live["handle"], handle) and exact_code(live.get("free_code"), 6) and same_handle(finished["handle"], handle) and
                exact_code(finished.get("cancel_code"), 0) and finished.get("actual_wait_and_free") is True,
                "read cancellation authority/drain mismatch")
        operation(events, "cancel.read.finished", 3, "CANCELLED")
        retry(events, "cancel.read.retry", baseline)
        return {"read_cancelled": True, "same_process_retry": True}
    if phase in (4, 5):
        label = "cancel.write" if phase == 4 else "kill.write"
        ordered(events, [label + ".initial-inspect", ("cancel" if phase == 4 else "kill") + ".source", label + ".started", label + ".intent"] +
                ([label + ".first-live", label + ".replacement-refused", label + ".live", label + ".finished", "phase.finished"] if phase == 4 else [label + ".live"]))
        operation(events, label + ".initial-inspect", diagnostic="S3_STATE_READY")
        source = one(events, ("cancel" if phase == 4 else "kill") + ".source")
        require(source.get("bytes") == 256 * 1024 * 1024 and base.valid_id(source.get("sha256")) and
                source.get("streamed") is True and source.get("excluded_from_backup") is True, "large fixture not bound")
        observed = one(events, label + ".intent")
        observation(observed["observation"], write=True)
        handle = observed["handle"]
        require(type(handle) is int and handle > 0 and observed.get("observer_closed") is True and
                same_handle(one(events, label + ".started")["handle"], handle), "actual observed handle mismatch")
        live = one(events, label + ".live")
        require(same_handle(live["handle"], handle) and exact_code(live.get("free_code"), 6), "live operation not proved")
        if phase == 5:
            require(live.get("sampled_active_only") is True and base.typed_equal(live.get("observation"), observed["observation"]) and
                    type(live.get("self_pid")) is int and events[-1]["event"] == "kill.write.live" and
                    not any(row["event"] == "phase.finished" for row in events), "pre-kill sampled evidence incomplete")
            return {"sampled_active_only": True, "observation": observed["observation"], "handle": handle, "pid": live["self_pid"]}
        ordered(events, ["cancel.write.initial-inspect", "cancel.write.alternate.prepared", "cancel.source",
                         "cancel.write.started", "cancel.write.first-live", "cancel.write.replacement-refused",
                         "cancel.write.finished", "cancel.write.alternate.closed", "phase.finished"])
        prepared, closed = one(events, "cancel.write.alternate.prepared"), one(events, "cancel.write.alternate.closed")
        require(prepared.get("held_owned_empty_root") is True and prepared.get("production_policy_apply_and_check") is True and
                exact_code(closed.get("code"), 0) and closed.get("failed_path") is False, "alternate fixture ownership/close missing")
        first_live, refusal = one(events, "cancel.write.first-live"), one(events, "cancel.write.replacement-refused")
        require(same_handle(first_live["handle"], handle) and exact_code(first_live.get("free_code"), 6) and same_handle(refusal.get("original_handle"), handle) and
                exact_code(refusal.get("start_code"), 6) and exact_code(refusal.get("out_handle"), 0) and
                refusal.get("alternate_root_policy_verified") is True and refusal.get("explicit_replacement_credentials") is True,
                "replacement refusal unproved")
        finished = one(events, "cancel.write.finished")
        require(same_handle(finished.get("handle"), handle) and exact_code(finished.get("cancel_code"), 0) and finished.get("actual_wait_and_free") is True,
                "write cancel/drain authority mismatch")
        status, reason = finished["result"].get("status"), finished["result"].get("diagnostic")
        category = one(events, "phase.finished").get("owner_category")
        if (status, reason, category) == (4, "CANCELLED_UNCERTAIN", "READY"):
            operation(events, "cancel.write.finished", 4, "CANCELLED_UNCERTAIN")
            ordered(events, ["cancel.write.finished", "cancel.write.retry.inspect", "cancel.write.retry.good-open", "cancel.write.retry.list", "cancel.write.retry.integrity", "phase.finished"])
            ids = retry(events, "cancel.write.retry", baseline, allow_new=True)
            known = finished["result"].get("snapshot_id")
            require(known == "" or (base.valid_id(known) and known in ids), "known snapshot missing after cancellation")
            require(one(events, "phase.finished").get("rollback_claimed") is False, "rollback claim refused")
            return {"owner_category": "READY", "snapshot_ids": ids}
        require((status, reason, category) == (11, "REMOTE_PENDING", "REMOTE_PENDING"), "write cancel classification unexercised")
        operation(events, "cancel.write.finished", 11, "REMOTE_PENDING")
        ordered(events, ["cancel.write.finished", "cancel.write.pending.inspect", "cancel.write.pending.open-refused", "cancel.write.pending.retained", "phase.finished"])
        pending(events, "cancel.write.pending", observed["observation"])
        finish = one(events, "phase.finished")
        require(finish.get("accepted_live_cancel_claimed") is False and finish.get("new_namespace_required_for_kill") is True,
                "pending category overclaimed")
        return {"owner_category": "REMOTE_PENDING", "kill_not_exercised": True}
    if phase == 7:
        label = "failed-write.pending"
        require([row["event"] for row in events] == ["phase.started", "owner.root-policy", "fixture.bound", "lifecycle.paths",
                label+".before", label+".inspect.started", label+".inspect", label+".open-refused.started", label+".open-refused",
                label+".retained", "phase.finished"], "closed read-only pending diagnostic sequence required")
        before = one(events, label+".before")
        prior = before["observation"]
        observation(prior, write=True, previous=prior)
        retained = pending(events, label, prior)
        require(base.typed_equal(retained, prior), "pending observation changed by refusal")
        for operation_name in [label+".inspect", label+".open-refused"]:
            require(same_handle(one(events, operation_name)["handle"], one(events, operation_name+".started")["handle"]),
                    "pending diagnostic operation handle mismatch")
        require(one(events, label+".open-refused.started").get("explicit_credentials") is True and
                one(events, label+".open-refused").get("actual_wait_and_free") is True, "explicit pending Open/drain required")
        ended = before.get("ended_pid")
        finish = one(events, "phase.finished")
        require(type(ended) is int and ended > 0 and ended != events[0]["pid"] and
                type(finish.get("ended_pid")) is int and finish["ended_pid"] == ended and
                finish.get("pending_cleared") is False and finish.get("remote_mutation_requested") is False and
                finish.get("origin") == "normally_exited_failed_control", "pending diagnostic origin/classification changed")
        return {"pending_retained":True, "normal_failed_process":ended, "mutation_requested":False}
    require(phase == 6 and killed is not None, "restart requires actual killed evidence")
    retained = pending(events, "kill.restart.pending", killed["observation"])
    for name in ["kill.restart.pending.inspect", "kill.restart.pending.open-refused"]:
        require(same_handle(one(events, name)["handle"], one(events, name + ".started")["handle"]),
                "restart pending operation handle mismatch")
    require(one(events, "kill.restart.pending.open-refused.started").get("explicit_credentials") is True and
            one(events, "kill.restart.pending.open-refused").get("actual_wait_and_free") is True,
            "explicit restart pending Open/drain required")
    require(retained["record_sha256"] == killed["observation"]["record_sha256"], "pending record changed after death")
    finish = one(events, "phase.finished")
    require(same_handle(finish.get("killed_pid"), killed["pid"]) and finish.get("sampled_active_only") is True and
            finish.get("crash_caused_pending_claimed") is False, "restart death binding mismatch")
    return {"pending_retained": True, "sampled_active_only": True, "crash_caused_pending_claimed": False}


def phase_identity(launch, events, *, phase, arguments, nonce, digest, build, installed_url, paths):
    actual = launch["result"]
    require(actual.get("deviceIdentifier") == DEVICE and actual.get("launchOptions", {}).get("arguments") == arguments,
            "actual launch device/arguments mismatch")
    process = actual["process"]
    pid = process.get("processIdentifier")
    require(type(pid) is int and pid > 0 and process.get("executable") == installed_url + "BackupS3Probe", "actual PID/app URL mismatch")
    termination = actual.get("terminationResult", {})
    # No inferred signal from a shell status such as 137. A differently shaped
    # actual terminal report is retained as a failed control for review.
    if phase == 5:
        require(type(termination.get("terminatingSignal")) is int and termination["terminatingSignal"] == signal.SIGKILL and
                termination.get("wasCoreDumpCreated") is False, "actual SIGKILL terminal witness required")
    else:
        require(type(termination.get("exitCode")) is int and termination["exitCode"] == 0 and
                termination.get("wasCoreDumpCreated") is False, "actual successful terminal witness required")
    require(events and all(type(row.get("phase")) is int and row["phase"] == phase and
                row.get("nonce") == nonce and type(row.get("pid")) is int and row["pid"] == pid and
                row.get("fixture_sha256") in ("", digest) for row in events), "foreign/stale phase evidence")
    require(events[0]["event"] == "phase.started" and sum(row["event"] == "phase.started" for row in events) == 1 and
            base.typed_equal(events[0]["value"], {"phase":phase, "executable_sha256":build["app_files"]["BackupS3Probe"],
                                    "native_archive_sha256":build["native_archive_sha256"]}), "running source/native identity mismatch")
    if phase != 5:
        require(events[-1]["event"] == "phase.finished" and exact_code(one(events, "phase.finished").get("phase"), phase) and
                one(events, "phase.finished").get("passed") is True, "phase terminal evidence missing")
    bound = one(events, "fixture.bound")
    require(bound.get("sha256") == digest and bound.get("held_nofollow_bounded_read") is True, "fixture custody mismatch")
    bound_index = next(i for i, row in enumerate(events) if row["event"] == "fixture.bound")
    require(all(row["fixture_sha256"] == digest for row in events[bound_index:]) and one(events, "lifecycle.paths") == paths,
            "retained owner/container path changed")
    return pid, termination


def relocated_paths(prior, current):
    """Permit only an iOS data-container UUID relocation, retaining all leaves."""
    suffixes = {"root":"Documents/remote-backup-foundation-v1",
                "owner_state":"Library/Application Support/callable-s3-owner-v1",
                "source":"Documents/remote-backup-foundation-v1/source"}
    def container(paths):
        require(isinstance(paths, dict) and set(paths) == set(suffixes), "closed retained path fields required")
        roots = []
        for key, suffix in suffixes.items():
            value = paths[key]
            prefix = "/private/var/mobile/Containers/Data/Application/"
            require(isinstance(value, str) and value.startswith(prefix), "iOS app data-container root required")
            tail = value[len(prefix):]
            identifier, separator, remainder = tail.partition("/")
            require(separator and str(uuid.UUID(identifier)).upper() == identifier and remainder == suffix,
                    "retained relative owner/source leaf changed")
            roots.append(prefix + identifier)
        require(len(set(roots)) == 1, "owner/source paths split across containers")
        return roots[0]
    container(prior); container(current)
    return dict(current)


def continued_read(directory, receipt_sha, *, baseline_sha, source_sha, build_sha, native_sha,
                   digest, build, installed_url, paths, held, baseline_results, durable, durable_sha, destination):
    require(base.valid_id(receipt_sha) and sha(directory / "receipt.json") == receipt_sha, "explicit failed-run anchor required")
    prior = json.loads((directory / "receipt.json").read_bytes())
    names = ["apps-before", "before-update-copy-evidence", "same-signer-update", "after-update-copy-evidence", "apps-updated", "phase3"]
    require(prior.get("complete") is False and exact_code(prior.get("exit"), 1) and prior.get("launching_phase") == 3 and
            prior.get("unknown_phone_process_after_launch") is True and [step["name"] for step in prior["steps"]] == names and
            all(exact_code(step.get("exit"), 0) for step in prior["steps"]), "only retained post-read controller failure may continue")
    require(prior.get("source_record_sha256") == source_sha and prior.get("build_receipt_sha256") == build_sha and
            prior.get("native_receipt_sha256") == native_sha and prior.get("fixture_sha256") == digest and
            base.typed_equal(prior.get("reservation"), held) and base.typed_equal(prior.get("destination"), destination),
            "failed-run app/fixture/reservation binding changed")
    for step in prior["steps"]:
        require(sha(directory / (step["name"] + ".log")) == step["log_sha256"] and
                sha(directory / (step["name"] + ".json")) == step["json_sha256"], "failed-run raw step changed")
        require(json.loads((directory / (step["name"] + ".json")).read_bytes())["info"]["outcome"] == "success", "failed-run step outcome changed")
    before = directory / "before-update-evidence.jsonl"
    after = directory / "after-update-evidence.jsonl"
    require(sha(before) == baseline_sha and sha(after) == baseline_sha, "original update did not preserve exact raw baseline")
    inventory = json.loads((directory / "apps-updated.json").read_bytes())
    installed = [item for item in inventory["result"]["apps"] if item["bundleIdentifier"] == BUNDLE]
    require(len(installed) == 1 and installed[0].get("builtByDeveloper") is True and installed[0].get("url") == installed_url,
            "retained updated app URL changed; no reinstall permitted")
    nonce = prior.get("nonce"); require(intent_id(nonce), "retained lifecycle nonce required")
    events = base.rows(directory / "phase3.log")
    actual_paths = relocated_paths(paths, one(events, "lifecycle.paths"))
    launch = json.loads((directory / "phase3.json").read_bytes())
    arguments = ["--nonce", nonce, "--fixture-sha256", digest, "--lifecycle", "3"]
    pid, terminal = phase_identity(launch, events, phase=3, arguments=arguments, nonce=nonce, digest=digest,
                                   build=build, installed_url=installed_url, paths=actual_paths)
    result = validate_controls(events, 3, baseline_results)
    bound = one(events, "fixture.bound")
    require(all(bound.get(key) == value for key, value in destination.items()), "retained read destination changed")
    require(base.valid_id(durable_sha) and sha(durable) == durable_sha and durable.read_bytes().startswith(before.read_bytes()),
            "explicit post-read forensic evidence/prefix binding required")
    combined = [json.loads(line) for line in before.read_bytes().splitlines()] + events
    require(base.typed_equal([json.loads(line) for line in durable.read_bytes().splitlines()], combined),
            "retained read console/durable evidence disagreement")
    return {"nonce":nonce, "pid":pid, "terminal":terminal, "paths":actual_paths, "events":events, "result":result,
            "boundary":"Posthoc read component reconciliation only. Original failed receipt stays failed; no phase3 replay, app reinstall or state reset."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("build-work", "work-dir", "fixture", "source-record", "baseline-receipt", "baseline-evidence"):
        parser.add_argument("--" + flag, type=Path, required=True)
    for flag in ("holder", "session", "source-record-sha256", "native-receipt-sha256", "build-receipt-sha256",
                 "baseline-receipt-sha256", "baseline-evidence-sha256"):
        parser.add_argument("--" + flag, required=True)
    parser.add_argument("--continue-from", type=Path)
    parser.add_argument("--continue-receipt-sha256")
    parser.add_argument("--continue-evidence-sha256")
    parser.add_argument("--controller-source-record", type=Path)
    parser.add_argument("--controller-source-record-sha256")
    args = parser.parse_args()
    continuation_fields = (args.continue_from, args.continue_receipt_sha256, args.continue_evidence_sha256,
                           args.controller_source_record, args.controller_source_record_sha256)
    require(all(item is None for item in continuation_fields) or all(item is not None for item in continuation_fields),
            "complete explicit continuation anchors required")
    lock = Path("/var/tmp/android-bench-hilphone16.lock/holder")
    held = base.reservation(lock.read_text(), args.holder, args.session)
    build_work = args.build_work.resolve()
    anchors = (args.source_record, args.source_record_sha256, args.native_receipt_sha256, args.build_receipt_sha256)
    build, app = base.validate_build(build_work, *anchors)
    source = json.loads(args.source_record.read_bytes())
    if args.continue_from is None:
        require(sha(Path(__file__)) == source["files"]["run_lifecycle.py"], "unreviewed lifecycle controller")
    else:
        require(base.valid_id(args.controller_source_record_sha256) and sha(args.controller_source_record) == args.controller_source_record_sha256,
                "independent continuation controller record required")
        controller_source = json.loads(args.controller_source_record.read_bytes())
        expected_files = dict(source["files"]); expected_files["run_lifecycle.py"] = sha(Path(__file__))
        require(controller_source["files"] == expected_files and sha(Path(base.__file__)) == source["files"]["run.py"],
                "continuation may change only the Python lifecycle driver; app/runtime inputs must stay exact")
    require(base.valid_id(args.baseline_receipt_sha256) and sha(args.baseline_receipt) == args.baseline_receipt_sha256 and
            base.valid_id(args.baseline_evidence_sha256) and sha(args.baseline_evidence) == args.baseline_evidence_sha256,
            "independent retained baseline anchors required")
    baseline = json.loads(args.baseline_receipt.read_bytes())
    previous = base.rows(args.baseline_evidence)
    require(baseline.get("complete") is True and baseline.get("exit") == 0 and baseline.get("native_archive_sha256") == build["native_archive_sha256"] and
            previous and previous[-1]["phase"] == 2 and previous[-1]["event"] == "phase.finished" and
            previous[-1]["value"].get("passed") is True and len({row["nonce"] for row in previous}) == 1,
            "passed retained production-archive baseline required")
    baseline_results = base.validate_operations([r for r in previous if r["phase"] == 1], [r for r in previous if r["phase"] == 2])
    require(baseline_results == baseline["results"], "baseline result/evidence disagreement")
    old_paths = one(previous, "fixture.paths")
    paths = {key:old_paths[key] for key in ("root", "owner_state", "source")}
    digest, value = base.fixture(args.fixture.absolute())
    require(digest == baseline["fixture_sha256"], "retained credential fixture changed")
    destination = {key:value[key] for key in ("endpoint", "region", "bucket", "prefix")}
    destination["access_key_id"] = value["access_key"]
    require(destination == baseline["destination"], "retained destination changed")
    work = args.work_dir.resolve(); work.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = {"PATH":"/usr/bin:/bin:/usr/sbin:/sbin", "HOME":str(Path.home()), "USER":os.environ["USER"],
           "LOGNAME":os.environ["USER"], "LANG":"en_US.UTF-8", "LC_ALL":"en_US.UTF-8", "TMPDIR":str(work)}
    env["DEVELOPER_DIR"] = subprocess.check_output(["/usr/bin/xcode-select", "-p"], env=env, text=True).strip()
    tool = Path(subprocess.check_output(["/usr/bin/xcrun", "--find", "devicectl"], env=env, text=True).strip()).resolve()
    nonce = secrets.token_hex(16)
    record = {"complete":False, "exit":None, "nonce":nonce, "fixture_sha256":digest, "destination":destination, "reservation":held,
              "source_record_sha256":args.source_record_sha256, "build_receipt_sha256":args.build_receipt_sha256,
              "native_receipt_sha256":args.native_receipt_sha256, "native_archive_sha256":build["native_archive_sha256"],
              "baseline_receipt_sha256":args.baseline_receipt_sha256, "baseline_evidence_sha256":args.baseline_evidence_sha256,
              "controller_sha256":sha(Path(__file__)), "devicectl":{"path":str(tool), "sha256":sha(tool)}, "steps":[], "results":{},
              "boundary":"Retained separate synthetic app and production archive. BYO owner-managed storage. Sampled active intent plus actual process death/retained pending only; no crash-caused pending, wire-inflight, operated, normal app, rotation, teardown or rollback claim."}

    def save():
        record["clock_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        (work / "receipt.json").write_text(json.dumps(record, indent=2) + "\n")

    def run(name, command, timeout=180, allowed=(0,)):
        require(base.reservation(lock.read_text(), args.holder, args.session) == held, "phone reservation changed")
        output, report = work / (name + ".log"), work / (name + ".json")
        arguments = list(map(str, command)); argv = [str(tool), *arguments[:3], "--json-output", str(report), *arguments[3:]]
        with output.open("xb") as stream:
            child = subprocess.Popen(argv, env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                code = child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                finally:
                    child.wait()
                record["unknown_phone_process_after_timeout"] = True; save()
                raise RuntimeError("local controller timed out; preserve phone owner/operation custody, no automatic retry")
        record["steps"].append({"name":name, "argv":argv, "exit":code, "log_sha256":sha(output),
                                "json_sha256":sha(report) if report.exists() else None}); save()
        print(name + " EXIT=" + str(code), flush=True)
        require(type(code) is int and code in allowed and report.exists(), "device command failed: " + name)
        result = json.loads(report.read_bytes())
        require(result["info"]["outcome"] == "success", "device command outcome failed: " + name)
        return result, output

    def copy_evidence(name):
        durable = work / (name + "-evidence.jsonl")
        run(name + "-copy-evidence", ["device", "copy", "from", "--device", DEVICE, "--domain-type", "appDataContainer", "--domain-identifier", BUNDLE,
                                     "--source", "Documents/backup-s3-evidence.jsonl", "--destination", durable])
        return durable

    save()
    try:
        inventory, _ = run("apps-before", ["device", "info", "apps", "--device", DEVICE])
        installed = [item for item in inventory["result"]["apps"] if item["bundleIdentifier"] == BUNDLE]
        require(len(installed) == 1 and installed[0].get("builtByDeveloper") is True, "retained separate app missing")
        cumulative, pids, killed = list(previous), [], None
        phases = (3, 4, 5, 6)
        if args.continue_from is not None:
            before = copy_evidence("continued-read")
            reconciled = continued_read(args.continue_from.resolve(), args.continue_receipt_sha256,
                baseline_sha=args.baseline_evidence_sha256, source_sha=args.source_record_sha256,
                build_sha=args.build_receipt_sha256, native_sha=args.native_receipt_sha256, digest=digest,
                build=build, installed_url=installed[0]["url"], paths=paths, held=held, baseline_results=baseline_results,
                durable=before, durable_sha=args.continue_evidence_sha256, destination=destination)
            nonce, paths = reconciled["nonce"], reconciled["paths"]
            pids.append(reconciled["pid"]); cumulative.extend(reconciled["events"]); phases = (4, 5, 6)
            record.update(nonce=nonce, continuation_failed_receipt_sha256=args.continue_receipt_sha256,
                controller_source_record_sha256=args.controller_source_record_sha256, original_run_reclassified=False,
                reconciled_read={key:value for key, value in reconciled.items() if key != "events"},
                unknown_phone_process_after_launch=False)
            record["results"]["3"] = reconciled["result"]; save()
        else:
            before = copy_evidence("before-update")
            require(sha(before) == args.baseline_evidence_sha256, "retained app evidence changed before update; do not reset")
            run("same-signer-update", ["device", "install", "app", "--device", DEVICE, app])
            after = copy_evidence("after-update")
            require(sha(after) == sha(before), "same-signer update changed retained evidence")
            inventory, _ = run("apps-updated", ["device", "info", "apps", "--device", DEVICE])
            installed = [item for item in inventory["result"]["apps"] if item["bundleIdentifier"] == BUNDLE]
            require(len(installed) == 1 and installed[0].get("builtByDeveloper") is True, "updated separate app missing")
        for phase in phases:
            check_digest, check_value = base.fixture(args.fixture.absolute())
            require(check_digest == digest and check_value == value, "fixture changed before phase")
            arguments = ["--nonce", nonce, "--fixture-sha256", digest, "--lifecycle", str(phase)]
            if phase == 6:
                arguments.extend(["--pending-intent", killed["observation"]["active"]["id"],
                                  "--pending-record-sha256", killed["observation"]["record_sha256"],
                                  "--pending-material-sha256", killed["observation"]["recovery_material_sha256"], "--killed-pid", str(killed["pid"])])
            name = "phase" + str(phase)
            record["unknown_phone_process_after_launch"] = True; record["launching_phase"] = phase; save()
            # CoreDevice reports CLI1 for an actual signal9 terminal. Only
            # phase5 admits that instrument status; phase_identity still
            # requires the exact typed signal/PID/app/device witness below.
            launch, log = run(name, ["device", "process", "launch", "--device", DEVICE, "--console", "--timeout", "600", BUNDLE, *arguments],
                              timeout=630, allowed=(0, 1) if phase == 5 else (0,))
            current = base.rows(log)
            if phase == 3:
                paths = relocated_paths(paths, one(current, "lifecycle.paths"))
            pid, terminal = phase_identity(launch, current, phase=phase, arguments=arguments, nonce=nonce, digest=digest,
                                           build=build, installed_url=installed[0]["url"], paths=paths)
            require(pid not in pids and pid not in baseline["pids"], "new process required")
            record["unknown_phone_process_after_launch"] = False
            record["terminal_process"] = {"phase":phase, "pid":pid, "termination":terminal}; save()
            pids.append(pid); cumulative.extend(current)
            durable = copy_evidence(name)
            raw_durable = durable.read_bytes()
            durable_rows = [json.loads(line) for line in raw_durable.splitlines()]
            require(raw_durable.startswith(before.read_bytes()) and base.typed_equal(durable_rows, cumulative) and
                    all(isinstance(row, dict) and "event" in row for row in durable_rows),
                    "console/durable cumulative evidence mismatch or changed raw baseline")
            for secret in (value["secret_key"], value["daily_password"], value["recovery_password"], value.get("session_token", "")):
                if secret:
                    require(secret.encode() not in log.read_bytes() and secret.encode() not in durable.read_bytes(), "secret in evidence")
            result = validate_controls(current, phase, baseline_results, killed=killed)
            bound = one(current, "fixture.bound")
            require(all(bound.get(key) == item for key, item in destination.items()), "phase destination changed")
            record["results"][str(phase)] = result; save()
            if phase == 4 and result["owner_category"] == "REMOTE_PENDING":
                record["retained_pending_requires_new_namespace"] = True; save()
                raise RuntimeError("write cancellation retained pending; preserve namespace and provision a distinct owned fixture for kill proof")
            if phase == 5:
                require(result["pid"] == pid, "self-killed PID mismatch"); killed = result
        base.validate_build(build_work, *anchors)
        require(sha(tool) == record["devicectl"]["sha256"], "device tool changed")
        record.update(complete=True, exit=0, pids=pids); save()
        return 0
    except Exception:
        record.update(complete=False, exit=1); save()
        raise


if __name__ == "__main__":
    sys.exit(main())
