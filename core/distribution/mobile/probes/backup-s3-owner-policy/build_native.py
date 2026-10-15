#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Add only the local owner-policy observer to an exact passed native module."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def census(root):
    result = {}
    for item in sorted(root.rglob("*")):
        relative = item.relative_to(root)
        if ".git" in relative.parts or "__pycache__" in relative.parts or item.suffix == ".pyc":
            continue
        info = item.lstat()
        if not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)):
            raise ValueError("non-regular source input")
        if stat.S_ISREG(info.st_mode):
            result[relative.as_posix()] = sha(item)
    return result


def tree_digest(files):
    digest = hashlib.sha256()
    # Match the qualified builder's Path-component ordering exactly.
    for name in sorted(files, key=Path):
        digest.update(name.encode() + b"\0")
        digest.update(bytes.fromhex(files[name]))
    return digest.hexdigest()


def toolchain_census(archive):
    files = {}
    with tarfile.open(archive) as bundle:
        for item in bundle.getmembers():
            parts = Path(item.name).parts
            if not parts or parts[0] != "go" or ".." in parts or item.name.startswith("/"):
                raise ValueError("unexpected admitted toolchain member")
            if item.isdir():
                continue
            if not item.isfile() or len(parts) < 2:
                raise ValueError("non-regular admitted toolchain member")
            name = Path(*parts[1:]).as_posix()
            if name in files:
                raise ValueError("duplicate admitted toolchain member")
            files[name] = hashlib.sha256(bundle.extractfile(item).read()).hexdigest()
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--production-work", type=Path, required=True)
    parser.add_argument("--production-receipt-sha256", required=True)
    parser.add_argument("--source-record", type=Path, required=True)
    parser.add_argument("--source-record-sha256", required=True)
    parser.add_argument("--toolchain-archive", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--staged", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    production = args.production_work.resolve()
    native_report = production / "receipts/backup-bridge-build.json"
    if sha(native_report) != args.production_receipt_sha256 or sha(args.source_record) != args.source_record_sha256:
        raise ValueError("independently expected production anchors mismatch")
    native = json.loads(native_report.read_bytes())
    if not (native.get("status") == "passed" and native.get("complete") is True and native.get("exit") == 0 and native.get("target") == "ios"):
        raise ValueError("complete passed production iOS archive required")
    for name, field in (("librestic_bridge.a", "archive_sha256"), ("librestic_bridge.h", "header_sha256")):
        if sha(production / "artifacts" / name) != native[field]:
            raise ValueError("production artifact changed")
    toolchain_archive = args.toolchain_archive.resolve()
    if sha(toolchain_archive) != native["toolchain_archive_sha256"]:
        raise ValueError("admitted Go archive changed")
    toolchain_files = toolchain_census(toolchain_archive)
    if census(production / "go-toolchain") != toolchain_files:
        raise ValueError("extracted Go toolchain differs from the admitted archive")
    source = production / "src/restic"
    original = census(source)
    if tree_digest(original) != native["staged_restic_source_sha256"]:
        raise ValueError("passed production module changed")
    source_record = json.loads(args.source_record.read_bytes())
    for name, digest in source_record["files"].items():
        part = Path(name)
        if part.parent.name == "backup-bridge" and part.suffix in (".go", ".h", ".m"):
            if original.get("mobilebackup/" + part.name) != digest:
                raise ValueError("production bridge source differs from reviewed source")
    work = args.work_dir.resolve()
    stage = work / "immutable-input"
    module = stage / "restic"
    if not args.staged:
        work.mkdir(mode=0o700, parents=True, exist_ok=False)
        stage.mkdir()
        shutil.copytree(source, module)
        fixture = Path(__file__).resolve().parent / "owner_policy_probe.go.fixture"
        shutil.copyfile(fixture, module / "mobilebackup/owner_policy_probe.go")
        shutil.copyfile(Path(__file__), stage / "build_native.py")
        shutil.copyfile(native_report, stage / "production-receipt.json")
        shutil.copyfile(args.source_record, stage / "source-record.json")
        for item in sorted(stage.rglob("*"), key=lambda item: len(item.parts), reverse=True):
            item.chmod(0o555 if item.is_dir() else 0o444)
        stage.chmod(0o555)
        argv = [sys.executable, str(stage / "build_native.py"), "--staged", "--production-work", str(production),
                "--production-receipt-sha256", args.production_receipt_sha256, "--source-record", str(stage / "source-record.json"),
                "--source-record-sha256", args.source_record_sha256, "--work-dir", str(work)]
        argv.extend(("--toolchain-archive", str(toolchain_archive)))
        os.execv(sys.executable, argv)
    if Path(__file__).resolve() != stage / "build_native.py" or sha(stage / "production-receipt.json") != args.production_receipt_sha256:
        raise ValueError("builder must execute from exact sealed staging")
    for item in [stage, *stage.rglob("*")]:
        if item.lstat().st_mode & 0o222:
            raise ValueError("staged input is writable")
    probe = module / "mobilebackup/owner_policy_probe.go"
    expected = dict(original)
    expected["mobilebackup/owner_policy_probe.go"] = sha(probe)
    if census(module) != expected:
        raise ValueError("private source delta is not exactly the observer")
    artifacts, receipts, temporary = work / "artifacts", work / "receipts", work / "tmp"
    for directory in (artifacts, receipts, temporary, work / "gocache"):
        directory.mkdir(mode=0o700)
    go = production / "go-toolchain/bin/go"
    target = native["target_build_environment"]
    env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(Path.home()), "USER": os.environ["USER"], "LOGNAME": os.environ["USER"],
           "GOROOT": str(production / "go-toolchain"), "GOTOOLCHAIN": "local", "GOENV": "off", "GOWORK": "off", "GOTELEMETRY": "off",
           "GOPROXY": "off", "GOSUMDB": "off", "GOMODCACHE": str(production / "gomodcache"), "GOPATH": str(production / "gopath"),
           "GOCACHE": str(work / "gocache"), "GOMAXPROCS": "4", "TMPDIR": str(temporary),
           "DEVELOPER_DIR": native["compiler_provenance"]["developer_dir"], **target}
    tools = {"go": {"path": str(go), "sha256": sha(go)}, "clang": {"path": target["CC"], "sha256": sha(Path(target["CC"]))}}
    result = {"status": "building", "complete": False, "exit": None, "target": "ios", "qualification_profile": "owner-policy-probe",
              "production_receipt_sha256": args.production_receipt_sha256, "production_source_record_sha256": args.source_record_sha256,
              "production_archive_sha256": native["archive_sha256"], "production_source_files": original, "private_source_files": expected,
              "observer_sha256": sha(probe), "controller_sha256": sha(Path(__file__)), "tools": tools, "environment": env, "steps": [],
              "toolchain_archive_sha256": sha(toolchain_archive), "toolchain_files_sha256": tree_digest(toolchain_files),
              "target_build_environment": target,
              "boundary": "Private qualification archive with one observer addition; never production admission. No backend or provider is constructed by the observer."}
    report = receipts / "backup-bridge-build.json"

    def save():
        result["clock_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        report.write_text(json.dumps(result, indent=2) + "\n")

    def run(name, argv):
        log = receipts / (name + ".log")
        argv = list(map(str, argv))
        with log.open("xb") as stream:
            child = subprocess.Popen(argv, cwd=module, env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                code = child.wait(timeout=360)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                code = 124
        result["steps"].append({"name": name, "argv": argv, "exit": code, "log_sha256": sha(log)})
        save()
        print(name + " EXIT=" + str(code), flush=True)
        if code:
            print(log.read_text(errors="replace")[-8000:], flush=True)
            raise subprocess.CalledProcessError(code, argv)
        return log

    save()
    try:
        def output(name, argv):
            return run(name, argv).read_text().strip()
        provenance = {
            "cc": str(Path(target["CC"]).resolve()),
            "cc_version": output("clang-version", [target["CC"], "--version"]).splitlines()[0],
            "developer_dir": output("developer-directory", ["/usr/bin/xcode-select", "-p"]),
            "xcode_version": output("xcode-version", ["/usr/bin/xcodebuild", "-version"]),
            "sdk_path": str(Path(output("sdk-path", ["/usr/bin/xcrun", "--sdk", "iphoneos", "--show-sdk-path"])).resolve()),
            "sdk_version": output("sdk-version", ["/usr/bin/xcrun", "--sdk", "iphoneos", "--show-sdk-version"]),
        }
        if provenance != native["compiler_provenance"] or output("go-version", [go, "version"]) != native["go_version"]:
            raise ValueError("measured compiler/SDK/Go provenance differs from production")
        result["compiler_provenance"] = provenance
        run("dependencies-verify-before", [go, "mod", "verify"])
        output("dependencies-selected", [go, "list", "-mod=readonly", "-buildvcs=false", "-tags=selfupdate,disable_grpc_modules", "-deps", "-json", "./mobilebackup"])
        production_symbols = run("production-symbol-exclusion", ["/usr/bin/nm", "-g", production / "artifacts/librestic_bridge.a"])
        if "restic_bridge_probe_s3_owner_policy" in production_symbols.read_text():
            raise ValueError("private observer already present in production archive")
        run("private-c-archive", [go, "build", "-p", "4", "-trimpath", "-mod=readonly", "-buildvcs=false", "-tags=selfupdate,disable_grpc_modules",
             "-buildmode=c-archive", "-o", artifacts / "librestic_bridge.a", "./mobilebackup"])
        selection_log = run("private-source-selection", [go, "list", "-mod=readonly", "-buildvcs=false", "-tags=selfupdate,disable_grpc_modules", "-json", "./mobilebackup"])
        selection = json.loads(selection_log.read_bytes())
        baseline = native["native_http_production_closure"]
        for key in ("GoFiles", "CgoFiles", "MFiles", "HFiles"):
            wanted = sorted(baseline[key] + (["owner_policy_probe.go"] if key == "CgoFiles" else []))
            if sorted(selection.get(key, [])) != wanted:
                raise ValueError("private selected closure differs beyond the observer")
        header = (artifacts / "librestic_bridge.h").read_text()
        old_header = (production / "artifacts/librestic_bridge.h").read_text()
        exports = lambda value: set(re.findall(r"extern [^;]+ (restic_bridge_\w+)\(", value))
        if exports(header) != exports(old_header) | {"restic_bridge_probe_s3_owner_policy"}:
            raise ValueError("public/private export delta mismatch")
        symbols = run("private-symbols", ["/usr/bin/nm", "-g", artifacts / "librestic_bridge.a"])
        if "restic_bridge_probe_s3_owner_policy" not in symbols.read_text():
            raise ValueError("private observer symbol absent")
        run("dependencies-verify-after", [go, "mod", "verify"])
        if census(module) != expected or census(source) != original:
            raise ValueError("private or original module changed")
        if sha(toolchain_archive) != native["toolchain_archive_sha256"] or census(production / "go-toolchain") != toolchain_files:
            raise ValueError("admitted toolchain changed during build")
        for tool in tools.values():
            if sha(Path(tool["path"])) != tool["sha256"]:
                raise ValueError("selected tool changed")
        result.update(status="passed", complete=True, exit=0, header_sha256=sha(artifacts / "librestic_bridge.h"), archive_sha256=sha(artifacts / "librestic_bridge.a"))
        save()
    except Exception:
        result.update(status="failed", complete=False, exit=1)
        save()
        raise


if __name__ == "__main__":
    main()
