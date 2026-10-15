#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Build an isolated HTTP canary app from an explicit passed native build."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess

ROOT = Path(__file__).resolve().parent


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree_digest(root):
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if ".git" in relative.parts or "__pycache__" in relative.parts or path.suffix == ".pyc" or not path.is_file():
            continue
        digest.update(relative.as_posix().encode() + b"\0")
        digest.update(bytes.fromhex(sha(path)))
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-work", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--device", required=True)
    parser.add_argument("--team", required=True)
    parser.add_argument("--keychain", type=Path, required=True)
    parser.add_argument("--signing-project", type=Path, required=True)
    args = parser.parse_args()
    source_census = [{"path": str(path.relative_to(ROOT)), "sha256": sha(path)} for path in sorted(ROOT.rglob("*"))
        if path.is_file() and "__pycache__" not in path.parts]
    native = args.native_work.resolve()
    receipt_path = native / "receipts/backup-bridge-build.json"
    receipt = json.loads(receipt_path.read_text())
    if not (receipt.get("status") == "passed" and receipt.get("complete") is True and receipt.get("exit") == 0 and receipt.get("target") == "ios"):
        raise ValueError("passed immutable iOS native receipt required")
    for name in ("archive", "header"):
        path = native / ("artifacts/librestic_bridge." + ("a" if name == "archive" else "h"))
        if sha(path) != receipt[name + "_sha256"]:
            raise ValueError("native archive/header identity mismatch")
    fixture = args.fixture.resolve()
    if stat.S_IMODE(fixture.stat().st_mode) != 0o600 or fixture.stat().st_size > 32768:
        raise ValueError("bounded private fixture required")
    work = args.work_dir.resolve()
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    source = work / "restic"
    shutil.copytree(native / "src/restic", source)
    bound_source_digest = tree_digest(source)
    if bound_source_digest != receipt["staged_restic_source_sha256"]:
        raise ValueError("copied native source does not match passed immutable receipt")
    for name in ("http_probe.go", "http_probe_test.go"):
        shutil.copyfile(ROOT / (name + ".fixture"), source / "mobilebackup" / name)
    for name in ("wire_hooks_darwin.m", "wire_hooks_apple.go", "wire_test.go"):
        shutil.copyfile(ROOT / (name + ".fixture"), source / "mobilebackup" / name)
    env = {
        "PATH": os.environ["PATH"], "HOME": str(Path.home()),
        "GOROOT": str(native / "go-toolchain"), "GOTOOLCHAIN": "local", "GOENV": "off",
        "GOWORK": "off", "GOTELEMETRY": "off", "GOPROXY": "off", "GOSUMDB": "off",
        "GOMODCACHE": str(native / "gomodcache"), "GOPATH": str(work / "gopath"),
        "GOCACHE": str(work / "gocache"), "TMPDIR": str(work), "CGO_ENABLED": "1",
        "GOOS": "darwin", "GOARCH": "arm64", "CC": receipt["sanitized_build_environment"]["CC"],
    }
    go = native / "go-toolchain/bin/go"
    steps = []

    def run(name, argv, cwd=source, build_env=None):
        argv = list(map(str, argv))
        log = work / (name + ".log")
        with log.open("wb") as output:
            result = subprocess.run(argv, cwd=cwd, env=build_env, stdout=output, stderr=subprocess.STDOUT)
        steps.append({"name": name, "argv": argv, "exit": result.returncode, "log_sha256": sha(log)})
        print(name + " EXIT=" + str(result.returncode), flush=True)
        if result.returncode:
            print(log.read_text(errors="replace")[-12000:], flush=True)
            raise subprocess.CalledProcessError(result.returncode, argv)
        return log

    run("host-wire", [go, "test", "-race", "-mod=readonly", "-buildvcs=false", "-tags=backup_http_wire",
        "-run", "^TestNativeHTTPWire", "-count=1", "-timeout=90s", "-v", "./mobilebackup"], build_env=env)
    run("host-canary", [go, "test", "-mod=readonly", "-buildvcs=false", "-tags=backup_http_probe",
        "-run", "^TestHTTPQualificationEntry$", "-count=1", "-v", "./mobilebackup", "-http-probe-fixture", fixture], build_env=env)
    env.update(receipt["target_build_environment"])
    tags = "selfupdate,disable_grpc_modules,backup_http_probe"
    selected = json.loads(run("harness-selection", [go, "list", "-mod=readonly", "-buildvcs=false", "-tags=" + tags,
        "-json", "./mobilebackup"], build_env=env).read_text())
    production = receipt["native_http_production_closure"]
    for field, original in production.items():
        expected = set(original) | ({"http_probe.go"} if field == "CgoFiles" else set())
        if set(selected.get(field, [])) != expected:
            raise ValueError("harness widened production closure: " + field)
        for name in original:
            if sha(source / "mobilebackup" / name) != sha(native / "src/restic/mobilebackup" / name):
                raise ValueError("production source changed in harness: " + name)
    artifacts = work / "artifacts"
    artifacts.mkdir()
    archive = artifacts / "librestic_bridge.a"
    run("harness-archive", [go, "build", "-trimpath", "-mod=readonly", "-buildvcs=false", "-tags=" + tags,
        "-buildmode=c-archive", "-o", archive, "./mobilebackup"], build_env=env)
    app_source = work / "app-source"
    app_source.mkdir()
    for name in ("Host", "Native", "Shared"):
        shutil.copytree(ROOT / name, app_source / name)
    shutil.copyfile(ROOT / "project.yml", app_source / "project.yml")
    run("unlock-signing", ["make", "-C", args.signing_project.resolve(), "unlock-signing"], cwd=app_source)
    run("project", ["xcodegen", "generate"], cwd=app_source)
    run("app", ["xcodebuild", "-project", "BackupHTTPProbe.xcodeproj", "-scheme", "BackupHTTPProbe", "-configuration", "Debug",
        "-destination", "generic/platform=iOS", "-derivedDataPath", work / "build", "-allowProvisioningUpdates",
        "BACKUP_ARCHIVE=" + str(archive), "BACKUP_HEADERS=" + str(artifacts), "DEVELOPMENT_TEAM=" + args.team,
        "OTHER_CODE_SIGN_FLAGS=--keychain " + str(args.keychain.resolve()), "build"], cwd=app_source)
    app = work / "build/Build/Products/Debug-iphoneos/BackupHTTPProbe.app"
    run("signature", ["codesign", "--verify", "--deep", "--strict", app], cwd=app_source)
    run("install", ["xcrun", "devicectl", "device", "install", "app", "--device", args.device, app], cwd=app_source)
    record = {
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "exit": 0,
        "native_receipt_sha256": sha(receipt_path), "steps": steps,
        "production_source_sha256_before_harness": bound_source_digest,
        "harness_archive_sha256": sha(archive), "harness_header_sha256": sha(artifacts / "librestic_bridge.h"),
        "selected": {field: selected.get(field, []) for field in production},
        "harness_source": source_census,
        "app_files": [{"path": str(path.relative_to(app)), "sha256": sha(path)} for path in sorted(app.rglob("*")) if path.is_file()],
        "boundary": "separate qualification archive/app; exact production native files plus explicit canary entry; no backup admission",
    }
    if any(sha(ROOT / item["path"]) != item["sha256"] for item in source_census):
        raise ValueError("qualification source changed during build")
    (work / "build-receipt.json").write_text(json.dumps(record, indent=2) + "\n")
    print("HTTP_PROBE_BUILD_INSTALL_EXIT=0", flush=True)


if __name__ == "__main__":
    main()
