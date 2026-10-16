# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Opt-in filesystem/process checks of build receipts and failed commands."""

import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "scripts"))
import backup_bridge_build as bridge_build


class TestBackupBridgeBuild(unittest.TestCase):
    def test_linked_identity_binds_staged_producer_dependencies_and_new_bridge_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, args = self.staged_build(root)
            build = module.BridgeBuild(args)
            toolchain = build.inputs["go-android"]["sha256"]
            fields, original = module.linked_build_identity("linux", "amd64", toolchain)
            self.assertEqual(fields["acquisition_helper_sha256"], module.sha256(module.ROOT / "scripts/android_cross_build.py"))
            self.assertEqual(len(original), 64)
            self.assertEqual(module.linked_build_identity("linux", "amd64", toolchain)[1], original)
            for path in [
                module.ROOT / "scripts/android_cross_build.py",
                Path(module.__file__), module.INPUTS_PATH,
                module.BRIDGE_SRC_PATH / "restic-library-safety.patch",
                module.BRIDGE_SRC_PATH / "restic-rooted-restorer.patch",
                module.BRIDGE_SRC_PATH / "restic-existing-root.patch",
            ]:
                prior = path.read_bytes()
                try:
                    # JSON input gets a whitespace-only change, preserving parse.
                    path.write_bytes(prior + b"\n")
                    self.assertNotEqual(module.linked_build_identity("linux", "amd64", toolchain)[1], original, str(path))
                finally:
                    path.write_bytes(prior)
            added = module.BRIDGE_SRC_PATH / "unexpected-new-source.go"
            added.write_text("package main\n")
            self.assertNotEqual(module.linked_build_identity("linux", "amd64", toolchain)[1], original)
            added.unlink()
            self.assertEqual(module.linked_build_identity("linux", "amd64", toolchain)[1], original)
            for os_name, arch, pin in [("ios", "amd64", toolchain), ("linux", "arm64", toolchain), ("linux", "amd64", "0" * 64)]:
                self.assertNotEqual(module.linked_build_identity(os_name, arch, pin)[1], original)

    def test_native_http_closure_refuses_selected_or_linked_qualification_authority(self):
        selection = {"CgoFiles": ["native_http_apple.go", "s3_owner_policy_apple.go"], "MFiles": ["native_http_darwin.m", "s3_owner_policy_darwin.m"]}
        bridge_build.check_native_http_closure(selection, "ios", "public ABI", "fixed native symbols")
        for field, bad in (("CgoFiles", "native_http_testhooks_apple.go"), ("MFiles", "native_http_testhooks_darwin.m")):
            changed = {key: list(value) for key, value in selection.items()}
            changed[field].append(bad)
            with self.assertRaisesRegex(ValueError, "qualification hooks"):
                bridge_build.check_native_http_closure(changed, "ios", "public ABI", "fixed native symbols")
        with self.assertRaisesRegex(ValueError, "qualification symbols"):
            bridge_build.check_native_http_closure(selection, "ios", "public ABI", "_sb_http_test_install")
        with self.assertRaisesRegex(ValueError, "private HTTP authority"):
            bridge_build.check_native_http_closure(selection, "ios", "void sb_http_cancel();", "fixed native symbols")
        with self.assertRaisesRegex(ValueError, "private HTTP authority"):
            bridge_build.check_native_http_closure(selection, "ios", "int sb_s3_owner_file_policy();", "fixed native symbols")
        for missing in ("native_http_darwin.m", "s3_owner_policy_darwin.m"):
            changed = {key: list(value) for key, value in selection.items()}
            changed["MFiles"].remove(missing)
            with self.assertRaisesRegex(ValueError, "closure is missing or widened"):
                bridge_build.check_native_http_closure(changed, "ios", "public ABI", "fixed native symbols")

    def staged_build(self, root, target="linux"):
        work = root / "build"
        stage = work / "immutable-input"
        (stage / "scripts").mkdir(parents=True)
        (stage / "core/distribution/mobile").mkdir(parents=True)
        shutil.copy2(Path(bridge_build.__file__), stage / "scripts/backup_bridge_build.py")
        shutil.copy2(bridge_build.ROOT / "scripts/android_cross_build.py", stage / "scripts/android_cross_build.py")
        shutil.copy2(bridge_build.INPUTS_PATH, stage / "core/distribution/mobile/inputs.json")
        shutil.copytree(bridge_build.BRIDGE_SRC_PATH, stage / "core/distribution/mobile/backup-bridge")
        for p in stage.rglob("*"):
            p.chmod(0o755 if p.is_dir() else 0o644)
        stage.chmod(0o755)
        spec = importlib.util.spec_from_file_location(
            "staged_backup_bridge_build_" + str(id(root)), stage / "scripts/backup_bridge_build.py"
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        args = argparse.Namespace(
            target=target, work_dir=work, cache_dir=root / "cache", jobs=1, staged=True
        )
        return module, args

    def test_running_receipt_cannot_report_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, args = self.staged_build(root)
            build = module.BridgeBuild(args)
            build.run_step("control", [sys.executable, "-c", "pass"])
            receipt = json.loads(build.receipt_file.read_text())
            self.assertEqual(receipt["status"], "running")
            self.assertIsNone(receipt["exit"])
            self.assertFalse(receipt["complete"])
            with self.assertRaises(FileExistsError):
                module.BridgeBuild(args)

    def test_command_failure_leaves_terminal_failed_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, args = self.staged_build(root)
            build = module.BridgeBuild(args)
            with self.assertRaises(Exception):
                build.run_step("fail-step", [sys.executable, "-c", "raise SystemExit(7)"])
            build.save_receipt(7)
            receipt = json.loads(build.receipt_file.read_text())
            self.assertEqual(receipt["status"], "failed")
            self.assertEqual(receipt["exit"], 7)
            self.assertFalse(receipt["complete"])
            self.assertEqual(receipt["steps"][-1]["exit"], 7)

    def test_inputs_keys_dynamically_loaded_without_literal_digests(self):
        # Verify that backup_bridge_build.py source does not hardcode digest hex strings
        script_text = Path(bridge_build.__file__).read_text()
        inputs_json = json.loads(bridge_build.INPUTS_PATH.read_text())["inputs"]
        restic_digest = inputs_json["restic"]["sha256"]
        self.assertNotIn(restic_digest, script_text)

        # Verify dynamic load matches inputs.json
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, args = self.staged_build(root)
            build = module.BridgeBuild(args)
            self.assertEqual(build.inputs["restic"]["sha256"], restic_digest)

    def test_ios_target_on_non_darwin_host_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, args = self.staged_build(root, target="ios")
            with mock.patch("platform.system", return_value="Linux"), mock.patch("platform.machine", return_value="x86_64"):
                build = module.BridgeBuild(args)
                with self.assertRaisesRegex(ValueError, "ios target requires Darwin/arm64"):
                    build.execute()
                build.save_receipt(1)
                receipt = json.loads(build.receipt_file.read_text())
                self.assertEqual(receipt["status"], "failed")
                self.assertFalse(receipt["complete"])
                self.assertEqual(receipt["exit"], 1)

    def test_staged_flag_and_marker_cannot_bypass_exact_script_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / "build"
            (work / "immutable-input").mkdir(parents=True)
            args = argparse.Namespace(
                target="linux", work_dir=work, cache_dir=root / "cache", jobs=1, staged=True
            )
            with self.assertRaisesRegex(ValueError, "immutable staged inputs"):
                bridge_build.BridgeBuild(args)

    def test_missing_rooted_restorer_patch_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, args = self.staged_build(root)
            patch = root / "build/immutable-input/core/distribution/mobile/backup-bridge/restic-rooted-restorer.patch"
            if patch.exists():
                patch.unlink()
            build = module.BridgeBuild(args)
            with mock.patch.object(build, "check_host"):
                with self.assertRaisesRegex(ValueError, "rooted restorer patch is missing or empty"):
                    build.execute()

    def test_invalid_rooted_restorer_patch_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, args = self.staged_build(root)
            patch = root / "build/immutable-input/core/distribution/mobile/backup-bridge/restic-rooted-restorer.patch"
            patch.write_text("invalid diff without git header\n")
            build = module.BridgeBuild(args)
            with mock.patch.object(build, "check_host"):
                with self.assertRaisesRegex(ValueError, "not a git unified diff"):
                    build.execute()


if __name__ == "__main__":
    unittest.main()
