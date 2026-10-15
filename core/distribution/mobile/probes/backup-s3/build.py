#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Build a separate S3 phone consumer from sealed source and native inputs."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
PATH = "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def census(root):
    result = {}
    for entry in sorted(root.rglob("*")):
        info = entry.lstat()
        if stat.S_ISLNK(info.st_mode) or not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode)):
            raise ValueError("unexpected input object")
        if stat.S_ISREG(info.st_mode):
            result[entry.relative_to(root).as_posix()] = sha(entry)
    return result


def passed_native(native, expected):
    report = native / "receipt.json"
    archive, header = native / "librestic_bridge.a", native / "librestic_bridge.h"
    if len(expected) != 64 or any(c not in "0123456789abcdef" for c in expected) or sha(report) != expected:
        raise ValueError("independent native receipt hash required")
    receipt = json.loads(report.read_bytes())
    if receipt.get("qualification_profile") or not receipt.get("steps") or any(step["exit"] != 0 for step in receipt["steps"]):
        raise ValueError("production native closure required")
    if not (receipt.get("status") == "passed" and receipt.get("complete") is True and receipt.get("exit") == 0 and receipt.get("target") == "ios"):
        raise ValueError("complete passed iOS archive receipt required")
    if sha(archive) != receipt["archive_sha256"] or sha(header) != receipt["header_sha256"]:
        raise ValueError("native archive/header identity mismatch")
    if "restic_bridge_s3_start" not in header.read_text():
        raise ValueError("additive callable S3 ABI missing")
    return receipt


def stage(args):
    work = args.work_dir.resolve()
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    inputs = work / "immutable-input"
    probe, native = inputs / "probe", inputs / "native"
    probe.mkdir(parents=True)
    native.mkdir()
    for name in ("Host", "Native", "Shared"):
        shutil.copytree(ROOT / name, probe / name, symlinks=True)
    for name in ("project.yml", "build.py", "run.py", "run_lifecycle.py"):
        shutil.copyfile(ROOT / name, probe / name)
    original = args.native_work.resolve()
    for source, target in ((original / "receipts/backup-bridge-build.json", native / "receipt.json"),
                           (original / "artifacts/librestic_bridge.a", native / "librestic_bridge.a"),
                           (original / "artifacts/librestic_bridge.h", native / "librestic_bridge.h")):
        if not stat.S_ISREG(source.lstat().st_mode):
            raise ValueError("native input must be a regular file")
        shutil.copyfile(source, target)
    passed_native(native, args.native_receipt_sha256)
    if sha(probe / "build.py") != sha(Path(__file__)):
        raise ValueError("executing builder changed while staging")
    census(inputs)
    for entry in sorted(inputs.rglob("*"), key=lambda item: len(item.parts), reverse=True):
        entry.chmod(0o555 if entry.is_dir() else 0o444)
    inputs.chmod(0o555)
    argv = [sys.executable, str(probe / "build.py"), "--staged", "--native-work", str(native), "--work-dir", str(work), "--native-receipt-sha256", args.native_receipt_sha256]
    if args.syntax_only:
        argv.append("--syntax-only")
    for flag, value in (("--team", args.team), ("--keychain", args.keychain), ("--signing-project", args.signing_project)):
        if value is not None:
            argv.extend((flag, str(value)))
    os.execv(sys.executable, argv)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-work", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--native-receipt-sha256", required=True)
    parser.add_argument("--syntax-only", action="store_true")
    parser.add_argument("--team")
    parser.add_argument("--keychain", type=Path)
    parser.add_argument("--signing-project", type=Path)
    parser.add_argument("--staged", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not args.syntax_only and not (args.team and args.keychain and args.signing_project):
        raise ValueError("explicit signing team, keychain and project required")
    if not args.staged:
        stage(args)
        raise AssertionError("re-exec did not occur")
    work = args.work_dir.resolve()
    inputs, native = work / "immutable-input", args.native_work.resolve()
    if ROOT != inputs / "probe" or native != inputs / "native" or Path(__file__).resolve() != inputs / "probe/build.py":
        raise ValueError("builder must execute from the sealed input tree")
    for entry in [inputs, *inputs.rglob("*")]:
        if entry.lstat().st_mode & 0o222:
            raise ValueError("input is writable")
    bound = census(inputs)
    receipt = passed_native(native, args.native_receipt_sha256)
    archive, header = native / "librestic_bridge.a", native / "librestic_bridge.h"
    project, temporary = work / "project", work / "tmp"
    project.mkdir(mode=0o700)
    temporary.mkdir(mode=0o700)
    env = {"PATH": PATH, "HOME": str(Path.home()), "USER": os.environ["USER"], "LOGNAME": os.environ["USER"],
           "TMPDIR": str(temporary), "LANG": "en_US.UTF-8", "LC_ALL": "en_US.UTF-8"}
    developer = subprocess.check_output(["/usr/bin/xcode-select", "-p"], env=env, text=True).strip()
    env["DEVELOPER_DIR"] = developer

    def capture(argv):
        return subprocess.check_output(list(map(str, argv)), env=env, text=True, stderr=subprocess.STDOUT).strip()

    tools = {}
    for name in ("swiftc", "clang", "xcodebuild", "codesign"):
        tools[name] = Path(capture(["/usr/bin/xcrun", "--find", name])).resolve()
    generator = shutil.which("xcodegen", path=PATH)
    if not generator:
        raise ValueError("xcodegen unavailable on explicit path")
    tools["xcodegen"] = Path(generator).resolve()
    tool_records = {name: {"path": str(binary), "sha256": sha(binary)} for name, binary in tools.items()}
    for name, argv in (("swiftc", [tools["swiftc"], "--version"]), ("clang", [tools["clang"], "--version"]),
                       ("xcodebuild", [tools["xcodebuild"], "-version"]), ("xcodegen", [tools["xcodegen"], "--version"])):
        tool_records[name]["version"] = capture(argv)
    sdk = capture(["/usr/bin/xcrun", "--sdk", "iphoneos", "--show-sdk-path"])
    sdk_version = capture(["/usr/bin/xcrun", "--sdk", "iphoneos", "--show-sdk-version"])
    template = (ROOT / "project.yml").read_text()
    template = template.replace("SWIFT_OBJC_BRIDGING_HEADER: Shared/Bridge.h", "SWIFT_OBJC_BRIDGING_HEADER: " + json.dumps(str(ROOT / "Shared/Bridge.h")))
    template = template.replace("INFOPLIST_FILE: Shared/Info.plist", "INFOPLIST_FILE: " + json.dumps(str(ROOT / "Shared/Info.plist")))
    template = template.replace("sources: [Host, Native]", "sources: [" + json.dumps(str(ROOT / "Host")) + ", " + json.dumps(str(ROOT / "Native")) + "]")
    (project / "project.yml").write_text(template)
    project_hash = sha(project / "project.yml")
    result = {"native_receipt_sha256": sha(native / "receipt.json"), "native_archive_sha256": receipt["archive_sha256"],
              "native_header_sha256": receipt["header_sha256"], "immutable_inputs": bound, "builder_sha256": sha(Path(__file__)),
              "environment": env, "tools": tool_records, "sdk": {"path": sdk, "version": sdk_version},
              "rendered_project_sha256": project_hash, "team": args.team, "steps": [], "complete": False, "exit": None,
              "boundary": "Separate synthetic app; production archive is unchanged. Read-only staged inputs and explicit tools bind this cooperative-host build. No hostile same-UID rename exclusion, normal-app authority, operated qualification or physical result is inferred."}
    report = work / "build-receipt.json"

    def save():
        result["recorded_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        report.write_text(json.dumps(result, indent=2) + "\n")

    def run(name, argv):
        argv = list(map(str, argv))
        log = work / (name + ".log")
        with log.open("wb") as stream:
            child = subprocess.run(argv, cwd=project, env=env, stdout=stream, stderr=subprocess.STDOUT)
        result["steps"].append({"name": name, "argv": argv, "exit": child.returncode, "log_sha256": sha(log)})
        save()
        print(name + " EXIT=" + str(child.returncode), flush=True)
        if child.returncode:
            print(log.read_text(errors="replace")[-12000:], flush=True)
            raise subprocess.CalledProcessError(child.returncode, argv)
        return log

    save()
    try:
        run("swift-typecheck", [tools["swiftc"], "-typecheck", "-parse-as-library", "-target", "arm64-apple-ios26.0", "-sdk", sdk,
            "-import-objc-header", ROOT / "Shared/Bridge.h", *sorted((ROOT / "Host").glob("*.swift"))])
        for source in sorted((ROOT / "Native").glob("*.m")):
            run("objc-typecheck-" + source.stem.lower(), [tools["clang"], "-fsyntax-only", "-fobjc-arc", "-Wall", "-Wextra", "-Werror",
                "-target", "arm64-apple-ios26.0", "-isysroot", sdk, "-I", header.parent, source])
        if not args.syntax_only:
            run("unlock-signing", ["/usr/bin/make", "-C", args.signing_project.resolve(), "unlock-signing"])
            run("project", [tools["xcodegen"], "generate"])
            run("app", [tools["xcodebuild"], "-project", project / "BackupS3Probe.xcodeproj", "-scheme", "BackupS3Probe", "-configuration", "Debug",
                "-destination", "generic/platform=iOS", "-derivedDataPath", work / "build", "-allowProvisioningUpdates",
                "BACKUP_ARCHIVE=" + str(archive), "BACKUP_HEADERS=" + str(header.parent), "BACKUP_NATIVE_SHA256=" + receipt["archive_sha256"], "DEVELOPMENT_TEAM=" + args.team,
                "OTHER_CODE_SIGN_FLAGS=--keychain " + str(args.keychain.resolve()), "build"])
            app = work / "build/Build/Products/Debug-iphoneos/BackupS3Probe.app"
            app_info = plistlib.loads((app / "Info.plist").read_bytes())
            if app_info.get("BackupS3NativeSHA256") != receipt["archive_sha256"] or app_info.get("CFBundleIdentifier") != "app.solstone.journal.backup.s3.dev":
                raise ValueError("built app native/bundle binding is missing or incorrect")
            run("signature", [tools["codesign"], "--verify", "--deep", "--strict", app])
            identity_log = run("signature-identity", [tools["codesign"], "--display", "--verbose=4", app])
            entitlements_log = run("signature-entitlements", [tools["codesign"], "--display", "--entitlements", ":-", app])
            identity = {}
            for line in identity_log.read_text().splitlines():
                key, separator, value = line.partition("=")
                if separator and key in ("Identifier", "TeamIdentifier"):
                    if key in identity:
                        raise ValueError("duplicate signing identity field")
                    identity[key] = value
            raw_entitlements = entitlements_log.read_bytes()
            start, end = raw_entitlements.find(b"<?xml"), raw_entitlements.find(b"</plist>")
            if start < 0 or end < start:
                raise ValueError("actual signing entitlements missing")
            entitlements = plistlib.loads(raw_entitlements[start:end + len(b"</plist>")])
            bundle = "app.solstone.journal.backup.s3.dev"
            if (identity != {"Identifier":bundle, "TeamIdentifier":args.team} or
                entitlements.get("com.apple.developer.team-identifier") != args.team or
                entitlements.get("application-identifier") != args.team + "." + bundle or
                entitlements.get("com.apple.security.application-groups")):
                raise ValueError("actual signer/bundle/entitlements mismatch")
            result["signing_identity"] = identity
            result["signing_entitlements"] = entitlements
            result["app_files"] = census(app)
            result["app_path"] = str(app)
        if census(inputs) != bound or sha(project / "project.yml") != project_hash:
            raise ValueError("sealed input or rendered project changed during build")
        for name, binary in tools.items():
            if sha(binary) != tool_records[name]["sha256"]:
                raise ValueError("selected tool changed during build")
        result.update(complete=True, exit=0)
        save()
        return 0
    except Exception:
        result.update(complete=False, exit=1)
        save()
        raise


if __name__ == "__main__":
    sys.exit(main())
