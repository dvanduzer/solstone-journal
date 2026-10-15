#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Build driver for mobile backup bridge static archive and verification."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from android_cross_build import acquire, sha256

INPUTS_PATH = ROOT / "core/distribution/mobile/inputs.json"
BRIDGE_SRC_PATH = ROOT / "core/distribution/mobile/backup-bridge"


def stage_and_reexec(args):
    work = Path(args.work_dir).resolve()
    work.mkdir(parents=True, exist_ok=False)
    stage = work / "immutable-input"
    (stage / "scripts").mkdir(parents=True)
    (stage / "core/distribution/mobile").mkdir(parents=True)
    shutil.copy2(Path(__file__), stage / "scripts/backup_bridge_build.py")
    shutil.copy2(ROOT / "scripts/android_cross_build.py", stage / "scripts/android_cross_build.py")
    shutil.copy2(INPUTS_PATH, stage / "core/distribution/mobile/inputs.json")
    shutil.copytree(BRIDGE_SRC_PATH, stage / "core/distribution/mobile/backup-bridge")
    for path in sorted(stage.rglob("*"), reverse=True):
        path.chmod(0o555 if path.is_dir() else 0o444)
    stage.chmod(0o555)
    env = os.environ.copy()
    env["SOLSTONE_BACKUP_SOURCE_COMMIT"] = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()
    argv = [
        sys.executable, str(stage / "scripts/backup_bridge_build.py"),
        "--target", args.target,
        "--work-dir", str(work),
        "--cache-dir", str(Path(args.cache_dir).resolve()),
        "--jobs", str(args.jobs),
        "--staged",
    ]
    os.execve(sys.executable, argv, env)


def tree_digest(root):
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if (
            ".git" in relative.parts
            or "__pycache__" in relative.parts
            or path.suffix == ".pyc"
            or not path.is_file()
        ):
            continue
        digest.update(relative.as_posix().encode() + b"\0")
        digest.update(bytes.fromhex(sha256(path)))
    return digest.hexdigest()


def check_native_http_closure(selection, target, header, symbols):
    selected = {
        key: selection.get(key, []) for key in ("GoFiles", "CgoFiles", "MFiles", "HFiles")
    }
    names = [name for values in selected.values() for name in values]
    if any("testhooks" in name or "_probe" in name for name in names):
        raise ValueError("qualification hooks selected in production archive")
    if target == "ios":
        if (
            not {"native_http_apple.go", "s3_owner_policy_apple.go"}.issubset(selected["CgoFiles"])
            or sorted(selected["MFiles"]) != ["native_http_darwin.m", "s3_owner_policy_darwin.m"]
        ):
            raise ValueError("fixed Apple HTTP/state-policy closure is missing or widened")
    elif selected["MFiles"] or "native_http_other.go" not in selected["GoFiles"]:
        raise ValueError("non-Apple HTTP platform boundary changed")
    if any(token in symbols for token in ("sb_http_test_", "SBHTTPFixtureProtocol", "restic_bridge_http_probe")):
        raise ValueError("qualification symbols present in production archive")
    if any(token in header for token in ("sb_http_", "sb_s3_owner_", "native_http_internal", "restic_bridge_http_probe")):
        raise ValueError("private HTTP authority escaped into public header")
    return selected


class BridgeBuild:
    def __init__(self, args):
        self.args = args
        self.work = Path(args.work_dir).resolve()
        stage = (self.work / "immutable-input").resolve()
        expected_script = stage / "scripts/backup_bridge_build.py"
        if (
            not args.staged
            or not stage.is_dir()
            or Path(__file__).resolve() != expected_script
            or ROOT.resolve() != stage
            or INPUTS_PATH.resolve() != stage / "core/distribution/mobile/inputs.json"
            or BRIDGE_SRC_PATH.resolve() != stage / "core/distribution/mobile/backup-bridge"
        ):
            raise ValueError("build must execute from immutable staged inputs")
        self.cache = Path(args.cache_dir).resolve()
        self.cache.mkdir(parents=True, exist_ok=True)

        self.receipts = self.work / "receipts"
        self.receipts.mkdir()
        self.receipt_file = self.receipts / "backup-bridge-build.json"

        self.jobs = getattr(args, "jobs", None) or os.cpu_count() or 4
        self.inputs = json.loads(INPUTS_PATH.read_text())["inputs"]

        self.receipt = {
            "status": "running",
            "complete": False,
            "exit": None,
            "target": args.target,
            "source_commit": self.git_head(),
            "inputs_sha256": sha256(INPUTS_PATH),
            "bridge_source_sha256": tree_digest(BRIDGE_SRC_PATH),
            "builder_sha256": sha256(Path(__file__)),
            "steps": [],
            "boundary": "compile-only; this manifest does not admit the bytes into the mobile native build, Android service tools, or an iOS product",
        }
        self.save_receipt()

    def git_head(self):
        if commit := os.environ.get("SOLSTONE_BACKUP_SOURCE_COMMIT"):
            return commit
        try:
            return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        except Exception:
            return "unknown"

    def save_receipt(self, code=None):
        if code is not None:
            self.receipt["exit"] = code
            self.receipt["status"] = "passed" if code == 0 else "failed"
            self.receipt["complete"] = (code == 0)
        self.receipt_file.write_text(json.dumps(self.receipt, indent=2) + "\n")

    def run_step(self, name, argv, cwd=ROOT, env=None):
        log_file = self.receipts / f"{name}.log"
        started = time.time()
        with log_file.open("wb") as stream:
            result = subprocess.run(list(map(str, argv)), cwd=cwd, env=env, stdout=stream, stderr=subprocess.STDOUT)
            code = result.returncode

        step = {
            "name": name,
            "argv": list(map(str, argv)),
            "started_epoch": started,
            "seconds": round(time.time() - started, 3),
            "exit": code,
            "log": str(log_file),
            "log_sha256": sha256(log_file),
        }
        self.receipt["steps"].append(step)
        self.save_receipt()

        if code != 0:
            print(log_file.read_text(errors="replace")[-12000:], file=sys.stderr)
            raise subprocess.CalledProcessError(code, argv)
        return log_file

    def extract_tar(self, archive, destination):
        destination.mkdir(parents=True, exist_ok=True)
        with tarfile.open(archive) as bundle:
            for member in bundle.getmembers():
                if "/" in member.name:
                    member.name = member.name.split("/", 1)[1]
                if member.name:
                    bundle.extract(member, destination, filter="data")

    def check_host(self):
        sys_name = platform.system()
        machine = platform.machine()
        if self.args.target == "linux":
            if (sys_name, machine) != ("Linux", "x86_64"):
                raise ValueError(f"linux target requires Linux/x86_64, host is {sys_name}/{machine}")
            cc = shutil.which("gcc") or shutil.which("clang") or "cc"
            self.receipt["compiler_provenance"] = {
                "cc": str(Path(cc).resolve()),
                "cc_version": subprocess.check_output([cc, "--version"], text=True).splitlines()[0],
            }
            self.host_cc = str(Path(cc).resolve())
        elif self.args.target == "ios":
            if (sys_name, machine) != ("Darwin", "arm64"):
                raise ValueError(f"ios target requires Darwin/arm64, host is {sys_name}/{machine}")
            sdk = subprocess.check_output(["xcrun", "--sdk", "iphoneos", "--show-sdk-path"], text=True).strip()
            if not sdk or not Path(sdk).exists():
                raise ValueError("missing iPhoneOS SDK")
            self.sdk = sdk
            clang = subprocess.check_output(["xcrun", "--sdk", "iphoneos", "--find", "clang"], text=True).strip()
            self.receipt["compiler_provenance"] = {
                "cc": str(Path(clang).resolve()),
                "cc_version": subprocess.check_output([clang, "--version"], text=True).splitlines()[0],
                "developer_dir": subprocess.check_output(["xcode-select", "-p"], text=True).strip(),
                "xcode_version": subprocess.check_output(["xcodebuild", "-version"], text=True).strip(),
                "sdk_path": str(Path(sdk).resolve()),
                "sdk_version": subprocess.check_output(["xcrun", "--sdk", "iphoneos", "--show-sdk-version"], text=True).strip(),
            }
            self.host_cc = str(Path(shutil.which("clang") or "cc").resolve())
        self.save_receipt()

    def execute(self):
        self.check_host()

        rooted_patch = BRIDGE_SRC_PATH / "restic-rooted-restorer.patch"
        if not rooted_patch.is_file() or rooted_patch.stat().st_size == 0:
            raise ValueError(f"rooted restorer patch is missing or empty: {rooted_patch}")
        patch_text = rooted_patch.read_text(errors="replace")
        if not patch_text.startswith("diff --git "):
            raise ValueError(f"rooted restorer patch is not a git unified diff: {rooted_patch}")

        # 1. Acquire and extract restic
        restic_pin = dict(self.inputs["restic"])
        restic_pin["filename"] = f"restic-{restic_pin['sha256']}.tar.gz"
        restic_archive = acquire(restic_pin, self.cache)
        if sha256(restic_archive) != restic_pin["sha256"]:
            raise ValueError(f"restic archive digest mismatch: {restic_archive}")

        restic_dir = self.work / "src/restic"
        self.extract_tar(restic_archive, restic_dir)

        # Verify module
        go_mod = (restic_dir / "go.mod").read_text()
        if "module github.com/restic/restic" not in go_mod:
            raise ValueError("restic source lacks module github.com/restic/restic")

        # The mobile-only overlay turns malformed authenticated metadata into
        # errors before upstream child goroutines can panic, and bounds decoding.
        safety_patch = BRIDGE_SRC_PATH / "restic-library-safety.patch"
        self.run_step("library-safety-overlay", ["patch", "--batch", "--forward", "--fuzz=0", "-p1", "-i", safety_patch], cwd=restic_dir)
        self.receipt["library_safety_patch_sha256"] = sha256(safety_patch)

        self.run_step("rooted-restorer-overlay", ["patch", "--batch", "--forward", "--fuzz=0", "-p1", "-i", rooted_patch], cwd=restic_dir)
        self.receipt["rooted_restorer_patch_sha256"] = sha256(rooted_patch)

        # Opt-in existing-root semantics; the prior restorer overlay and all
        # external/common producer inputs remain independently pinned.
        existing_patch = BRIDGE_SRC_PATH / "restic-existing-root.patch"
        self.run_step("existing-root-overlay", ["patch", "--batch", "--forward", "--fuzz=0", "-p1", "-i", existing_patch], cwd=restic_dir)
        self.receipt["existing_root_patch_sha256"] = sha256(existing_patch)

        # Copy bridge files into restic/mobilebackup/
        pkg_dir = restic_dir / "mobilebackup"
        pkg_dir.mkdir(parents=True, exist_ok=True)
        for src_file in sorted(BRIDGE_SRC_PATH.iterdir()):
            if not src_file.is_file() or src_file.suffix not in {".go", ".h", ".m"}:
                continue
            shutil.copy2(src_file, pkg_dir / src_file.name)

        # 2. Acquire and extract toolchain
        toolchain_key = "go-android" if self.args.target == "linux" else "go-ios"
        toolchain_pin = dict(self.inputs[toolchain_key])
        toolchain_pin["filename"] = f"{toolchain_key}-{toolchain_pin['sha256']}.tar.gz"
        toolchain_archive = acquire(toolchain_pin, self.cache)
        if sha256(toolchain_archive) != toolchain_pin["sha256"]:
            raise ValueError(f"toolchain archive digest mismatch: {toolchain_archive}")

        toolchain_dir = self.work / "go-toolchain"
        self.extract_tar(toolchain_archive, toolchain_dir)
        go_bin = toolchain_dir / "bin/go"

        # Build environment
        goos = "linux" if self.args.target == "linux" else "ios"
        goarch = "amd64" if self.args.target == "linux" else "arm64"

        env = os.environ.copy()
        for key in (
            "GOFLAGS", "GOEXPERIMENT", "GOCACHEPROG", "GO111MODULE", "GODEBUG",
            "GOAMD64", "GOARM", "GOARM64", "GO386", "GOMIPS", "GOMIPS64",
            "GOPPC64", "GORISCV64", "GOWASM", "CGO_CFLAGS", "CGO_CPPFLAGS",
            "CGO_CXXFLAGS", "CGO_LDFLAGS", "CC", "CXX", "AR", "PKG_CONFIG",
            "CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "LIBRARY_PATH",
            "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "IPHONEOS_DEPLOYMENT_TARGET",
        ):
            env.pop(key, None)
        env.update(
            GOROOT=str(toolchain_dir),
            GOTOOLCHAIN="local",
            GOENV="off",
            GOTELEMETRY="off",
            GOWORK="off",
            GOPATH=str(self.work / "gopath"),
            GOMODCACHE=str(self.work / "gomodcache"),
            GOCACHE=str(self.work / "gocache"),
            TMPDIR=str(self.work),
            GOOS="linux" if platform.system() == "Linux" else "darwin",
            GOARCH="amd64" if platform.machine() == "x86_64" else "arm64",
            CGO_ENABLED="1",
            GOPROXY="https://proxy.golang.org",
            GOSUMDB="sum.golang.org",
            CC=self.host_cc,
        )

        go_version_out = subprocess.check_output([str(go_bin), "version"], env=env, text=True).strip()
        self.receipt["sanitized_build_environment"] = {
            key: env.get(key, "") for key in (
                "GOROOT", "GOTOOLCHAIN", "GOENV", "GOWORK", "GOOS", "GOARCH",
                "CGO_ENABLED", "CC", "CGO_CFLAGS", "CGO_LDFLAGS",
            )
        }
        self.save_receipt()

        # Step: Module download & verify
        self.run_step("go-download", [go_bin, "mod", "download", "-modcacherw"], cwd=restic_dir, env=env)
        self.run_step("go-verify", [go_bin, "mod", "verify"], cwd=restic_dir, env=env)

        env.update(GOPROXY="off", GOSUMDB="off")

        # Step: Dependency check
        deps_log = self.run_step("go-deps", [go_bin, "list", "-buildvcs=false", "-deps", "./mobilebackup"], cwd=restic_dir, env=env)
        deps_text = deps_log.read_text(errors="replace")
        if "github.com/rclone/rclone" in deps_text:
            raise ValueError("rclone found in mobilebackup dependencies")

        # Step: Run unit tests (no tags)
        self.run_step("go-test", [go_bin, "test", "-buildvcs=false", "./mobilebackup"], cwd=restic_dir, env=env)
        self.run_step("rooted-metadata-tests", [go_bin, "test", "-buildvcs=false", "-run", "^TestRootRestoreMetadata", "./internal/restorer"], cwd=restic_dir, env=env)
        self.run_step("existing-root-tests", [go_bin, "test", "-buildvcs=false", "-run", "^TestExistingRoot", "./internal/restorer"], cwd=restic_dir, env=env)
        self.run_step(
            "receipt-control-tests",
            [sys.executable, BRIDGE_SRC_PATH / "check_build_receipts.py"],
            cwd=ROOT,
            env=env,
        )

        # Compile and execute the behavioral suite on the build host.
        integration_bin = self.work / "mobilebackup-integration.test"
        self.run_step("go-compile-integration", [go_bin, "test", "-buildvcs=false", "-c", "-tags", "integration", "-o", integration_bin, "./mobilebackup"], cwd=restic_dir, env=env)
        self.run_step("go-run-integration", [integration_bin, "-test.count=1"], cwd=restic_dir, env=env)

        # Exercise the exported ABI through a real host C caller. The target
        # archive receives its own link/platform checks below; an iOS executable
        # cannot be executed by the Darwin build host.
        host_artifacts = self.work / "host-artifacts"
        host_artifacts.mkdir()
        host_archive = host_artifacts / "librestic_bridge.a"
        self.run_step(
            "host-c-archive-build",
            [go_bin, "build", "-trimpath", "-mod=readonly", "-buildvcs=false",
             "-tags=selfupdate,disable_grpc_modules", "-buildmode=c-archive",
             "-o", host_archive, "./mobilebackup"],
            cwd=restic_dir, env=env,
        )
        host_caller = host_artifacts / "caller.c"
        abi_repo = host_artifacts / "abi-repo"
        abi_source = host_artifacts / "abi-source"
        shutil.copy2(BRIDGE_SRC_PATH / "abi_legacy.h", host_artifacts / "abi_legacy.h")
        host_caller.write_text(f'''#include "abi_legacy.h"
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>

static int run(restic_request *req, restic_result *result) {{
    uint64_t id = 0;
    if (restic_bridge_start(req, &id) != 0 || id == 0) return 10;
    if (restic_bridge_wait(id, result) != 0) return 11;
    if (restic_bridge_free(id) != 0) return 12;
    return 0;
}}

int main(void) {{
    const char *password = "host-abi-fixture-password";
    const char *repo = {json.dumps(str(abi_repo))};
    const char *source = {json.dumps(str(abi_source))};
    if (mkdir(repo, 0700) != 0 || mkdir(source, 0700) != 0) return 1;
    char file_path[4096];
    if (snprintf(file_path, sizeof(file_path), "%s/owner.txt", source) <= 0) return 2;
    FILE *file = fopen(file_path, "wb");
    if (file == NULL || fwrite("owner", 1, 5, file) != 5 || fclose(file) != 0) return 3;

    restic_request req = {{0}};
    restic_result result = {{0}};
    req.operation = 1;
    req.password = (char *)password; req.password_len = (uint32_t)strlen(password);
    req.repo_path = (char *)repo; req.repo_path_len = (uint32_t)strlen(repo);
    if (run(&req, &result) != 0 || result.status != 0 || strlen(result.key_id) != 64) return 20;

    memset(&req, 0, sizeof(req)); memset(&result, 0, sizeof(result));
    req.operation = 4;
    req.password = (char *)password; req.password_len = (uint32_t)strlen(password);
    req.repo_path = (char *)repo; req.repo_path_len = (uint32_t)strlen(repo);
    req.source_path = (char *)source; req.source_path_len = (uint32_t)strlen(source);
    if (run(&req, &result) != 0 || result.status != 0 || strlen(result.snapshot_id) != 64) return 21;
    char snapshot_id[65]; memcpy(snapshot_id, result.snapshot_id, sizeof(snapshot_id));

    memset(&req, 0, sizeof(req)); memset(&result, 0, sizeof(result));
    req.operation = 5;
    req.password = (char *)password; req.password_len = (uint32_t)strlen(password);
    req.repo_path = (char *)repo; req.repo_path_len = (uint32_t)strlen(repo);
    if (run(&req, &result) != 0 || result.status != 0 || result.snapshot_count != 1) return 22;
    if (strcmp(snapshot_id, result.snapshot_ids[0]) != 0) return 23;
    if (restic_bridge_cancel(0) != 7 || restic_bridge_free(0) != 7) return 24;
    return 0;
}}
''')
        host_caller_bin = host_artifacts / "caller"
        if platform.system() == "Darwin":
            host_cc = shutil.which("clang") or "cc"
            host_link = [host_cc, "-I", host_artifacts, host_caller, host_archive,
                         "-framework", "CoreFoundation", "-framework", "Foundation", "-framework", "Security", "-lresolv", "-o", host_caller_bin]
        else:
            host_cc = shutil.which("gcc") or shutil.which("clang") or "cc"
            host_link = [host_cc, "-I", host_artifacts, host_caller, host_archive,
                         "-lpthread", "-ldl", "-lm", "-o", host_caller_bin]
        self.run_step("host-c-link", host_link, cwd=host_artifacts, env=env)
        self.run_step("host-c-run", [host_caller_bin], cwd=host_artifacts, env=env)

        metadata_caller = host_artifacts / "metadata-caller.c"
        shutil.copy2(BRIDGE_SRC_PATH / "metadata_caller.c.fixture", metadata_caller)
        metadata_bin = host_artifacts / "metadata-caller"
        metadata_link = [str(item) if item not in (host_caller, host_caller_bin) else str(metadata_caller if item == host_caller else metadata_bin) for item in host_link]
        self.run_step("metadata-c-link", metadata_link, cwd=host_artifacts, env=env)
        self.run_step("metadata-c-run", [metadata_bin, abi_repo], cwd=host_artifacts, env=env)

        selection_caller = host_artifacts / "selection-caller.c"
        shutil.copy2(BRIDGE_SRC_PATH / "snapshot_selection_caller.c.fixture", selection_caller)
        selection_bin = host_artifacts / "selection-caller"
        selection_link = [str(item) if item not in (host_caller, host_caller_bin) else str(selection_caller if item == host_caller else selection_bin) for item in host_link]
        self.run_step("selection-c-link", selection_link, cwd=host_artifacts, env=env)
        self.run_step("selection-c-run", [selection_bin, host_artifacts], cwd=host_artifacts, env=env)

        rooted_caller = host_artifacts / "rooted-caller.c"
        shutil.copy2(BRIDGE_SRC_PATH / "rooted_restore_caller.c.fixture", rooted_caller)
        rooted_bin = host_artifacts / "rooted-caller"
        rooted_link = [str(item) if item not in (host_caller, host_caller_bin) else str(rooted_caller if item == host_caller else rooted_bin) for item in host_link]
        self.run_step("existing-root-c-link", rooted_link, cwd=host_artifacts, env=env)
        self.run_step("existing-root-c-run", [rooted_bin, host_artifacts], cwd=host_artifacts, env=env)

        workspace_caller = host_artifacts / "workspace-caller.c"
        shutil.copy2(BRIDGE_SRC_PATH / "workspace_caller.c.fixture", workspace_caller)
        workspace_bin = host_artifacts / "workspace-caller"
        workspace_link = [str(item) if item not in (host_caller, host_caller_bin) else str(workspace_caller if item == host_caller else workspace_bin) for item in host_link]
        self.run_step("workspace-c-link", workspace_link, cwd=host_artifacts, env=env)
        self.run_step("workspace-c-run", [workspace_bin, host_artifacts], cwd=host_artifacts, env=env)

        s3_caller = host_artifacts / "s3-caller.c"
        shutil.copy2(BRIDGE_SRC_PATH / "s3_caller.c.fixture", s3_caller)
        s3_bin = host_artifacts / "s3-caller"
        s3_link = [str(item) if item not in (host_caller, host_caller_bin) else str(s3_caller if item == host_caller else s3_bin) for item in host_link]
        self.run_step("s3-c-link", s3_link, cwd=host_artifacts, env=env)
        self.run_step("s3-c-run", [s3_bin, host_artifacts], cwd=host_artifacts, env=env)

        # Host behavioral execution above is independent of the device target.
        env.update(GOOS=goos, GOARCH=goarch)

        # Step: Build static c-archive
        artifacts_dir = self.work / "artifacts"
        artifacts_dir.mkdir(parents=True, exist_ok=True)
        archive_path = artifacts_dir / "librestic_bridge.a"
        header_path = artifacts_dir / "librestic_bridge.h"

        cgo_cflags = []
        cgo_ldflags = []
        if self.args.target == "ios":
            cgo_cflags = ["-arch", "arm64", "-isysroot", self.sdk, "-miphoneos-version-min=26.0"]
            cgo_ldflags = ["-arch", "arm64", "-isysroot", self.sdk, "-miphoneos-version-min=26.0"]
            env["CC"] = subprocess.check_output(["xcrun", "--sdk", "iphoneos", "--find", "clang"], text=True).strip()
            env["CGO_CFLAGS"] = " ".join(cgo_cflags)
            env["CGO_LDFLAGS"] = " ".join(cgo_ldflags)

        self.receipt["target_build_environment"] = {
            key: env.get(key, "") for key in (
                "GOOS", "GOARCH", "CGO_ENABLED", "CC", "CGO_CFLAGS", "CGO_LDFLAGS",
            )
        }
        self.save_receipt()

        build_cmd = [
            go_bin, "build",
            "-p", str(self.jobs),
            "-trimpath",
            "-mod=readonly",
            "-buildvcs=false",
            "-tags=selfupdate,disable_grpc_modules",
            "-buildmode=c-archive",
            "-o", archive_path,
            "./mobilebackup",
        ]
        self.run_step("c-archive-build", build_cmd, cwd=restic_dir, env=env)

        # Copy generated header to standard name if needed
        gen_header = artifacts_dir / "librestic_bridge.h"
        if not gen_header.exists():
            raise ValueError(f"expected C header: {gen_header}")

        selection_log = self.run_step(
            "target-source-selection", [go_bin, "list", "-mod=readonly", "-buildvcs=false",
             "-tags=selfupdate,disable_grpc_modules", "-json", "./mobilebackup"], cwd=restic_dir, env=env,
        )
        symbols_log = self.run_step("target-archive-symbols", ["nm", "-g", archive_path], cwd=restic_dir, env=env)
        self.receipt["native_http_production_closure"] = check_native_http_closure(
            json.loads(selection_log.read_text()), self.args.target,
            gen_header.read_text(), symbols_log.read_text(errors="replace"),
        )

        # Step: Link check with standalone C caller
        caller_c = artifacts_dir / "caller.c"
        caller_c.write_text(
            '#include "librestic_bridge.h"\n'
            '#include <stdint.h>\n'
            'int main(void) {\n'
            '    uint64_t id = 0;\n'
            '    restic_request req = {0};\n'
            '    int32_t rc = restic_bridge_start(&req, &id);\n'
            '    int32_t cancelled = restic_bridge_cancel(0);\n'
            '    int32_t waited = restic_bridge_wait(0, 0);\n'
            '    int32_t freed = restic_bridge_free(0);\n'
            '    uint32_t required = 0;\n'
            '    int32_t metadata = restic_bridge_result_metadata(0, 0, 0, &required);\n'
            '    int32_t selected = restic_bridge_snapshot_v1_start(0, 0, 0, 0, 0, &id);\n'
            '    int32_t remote_selected = restic_bridge_s3_snapshot_v1_start(0, 0, 0, &id);\n'
            '    return rc == 0 || cancelled == 0 || waited == 0 || freed == 0 || metadata == 0 || selected == 0 || remote_selected == 0 ? 1 : 0;\n'
            '}\n'
        )
        caller_bin = artifacts_dir / "caller-link"

        if self.args.target == "linux":
            cc = shutil.which("gcc") or shutil.which("clang") or "cc"
            link_cmd = [cc, "-O2", "-I", str(artifacts_dir), str(caller_c), str(archive_path), "-lpthread", "-ldl", "-lm", "-o", str(caller_bin)]
        else:
            cc = subprocess.check_output(["xcrun", "--sdk", "iphoneos", "--find", "clang"], text=True).strip()
            link_cmd = [
                cc, *cgo_cflags,
                "-I", str(artifacts_dir),
                str(caller_c),
                str(archive_path),
                "-framework", "CoreFoundation",
                "-framework", "Foundation",
                "-framework", "Security",
                "-lresolv",
                "-o", str(caller_bin),
            ]
        self.run_step("c-link-check", link_cmd, cwd=artifacts_dir, env=env)

        if self.args.target == "ios":
            self.run_step("caller-platform", ["xcrun", "vtool", "-show-build", caller_bin], cwd=artifacts_dir, env=env)
        else:
            self.run_step("caller-platform", ["readelf", "-h", caller_bin], cwd=artifacts_dir, env=env)

        # Update receipt with final evidence
        self.receipt.update({
            "goos": goos,
            "goarch": goarch,
            "go_version": go_version_out,
            "restic_archive_sha256": sha256(restic_archive),
            "toolchain_archive_sha256": sha256(toolchain_archive),
            "staged_bridge_sha256": tree_digest(pkg_dir),
            "staged_restic_source_sha256": tree_digest(restic_dir),
            "header_sha256": sha256(header_path),
            "archive_sha256": sha256(archive_path),
            "compile_exit": 0,
        })
        self.save_receipt(0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=["linux", "ios"], required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--cache-dir", type=Path, required=True)
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    parser.add_argument("--staged", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()

    if not args.staged:
        stage_and_reexec(args)

    build = None
    try:
        build = BridgeBuild(args)
        build.execute()
        return 0
    except Exception as e:
        print(f"build failed: {e}", file=sys.stderr)
        if build is not None:
            build.save_receipt(1)
        return 1


if __name__ == "__main__":
    sys.exit(main())
