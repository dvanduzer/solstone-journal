#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Unit tests for the iOS GPU appraisal receipt writer.

Ensures that failed steps, partial runs, and missing helper digests fail-closed
and can never yield a passed or complete receipt.
"""

import json
import pathlib
import sys
import tempfile
import unittest

SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

from build_gpu_appraisal import format_receipt, write_receipt


class TestGpuAppraisalReceipt(unittest.TestCase):
    def setUp(self):
        self.tmp_dir = tempfile.TemporaryDirectory(dir="/var/tmp")
        self.tmp_path = pathlib.Path(self.tmp_dir.name)
        self.dummy_helper = self.tmp_path / "Appraisal"
        self.dummy_helper.write_bytes(b"\xca\xfe\xba\xbe dummy executable bytes")

    def tearDown(self):
        self.tmp_dir.cleanup()

    def test_successful_run_is_passed_and_complete(self):
        steps = [
            {"name": "patch-apply", "exit": 0},
            {"name": "cmake-configure", "exit": 0},
            {"name": "link-probe", "exit": 0},
        ]
        dest = self.tmp_path / "receipt.json"
        receipt = write_receipt(
            dest,
            steps=steps,
            needed=["/usr/lib/libSystem.B.dylib"],
            helper_path=self.dummy_helper,
            is_partial=False,
        )
        self.assertEqual(receipt["status"], "passed")
        self.assertTrue(receipt["complete"])
        self.assertEqual(receipt["exit"], 0)
        self.assertTrue(len(receipt["helper_sha256"]) == 64)
        self.assertTrue(dest.exists())
        loaded = json.loads(dest.read_text())
        self.assertEqual(loaded["status"], "passed")

    def test_failed_step_cannot_be_passed_or_complete(self):
        steps = [
            {"name": "patch-apply", "exit": 0},
            {"name": "cmake-configure", "exit": 2},
            {"name": "link-probe", "exit": 0},
        ]
        dest = self.tmp_path / "receipt.json"
        receipt = write_receipt(
            dest,
            steps=steps,
            helper_path=self.dummy_helper,
            is_partial=False,
        )
        self.assertEqual(receipt["status"], "failed")
        self.assertFalse(receipt["complete"])
        self.assertNotEqual(receipt["exit"], 0)

    def test_partial_run_cannot_be_passed_or_complete(self):
        steps = [
            {"name": "patch-apply", "exit": 0},
        ]
        dest = self.tmp_path / "receipt.json"
        receipt = write_receipt(
            dest,
            steps=steps,
            helper_path=self.dummy_helper,
            is_partial=True,
        )
        self.assertEqual(receipt["status"], "failed")
        self.assertFalse(receipt["complete"])
        self.assertNotEqual(receipt["exit"], 0)

    def test_missing_helper_cannot_be_passed_or_complete(self):
        steps = [
            {"name": "patch-apply", "exit": 0},
        ]
        dest = self.tmp_path / "receipt.json"
        receipt = write_receipt(
            dest,
            steps=steps,
            helper_path=None,
            is_partial=False,
        )
        self.assertEqual(receipt["status"], "failed")
        self.assertFalse(receipt["complete"])
        self.assertNotEqual(receipt["exit"], 0)
        self.assertEqual(receipt["helper_sha256"], "")

    def test_empty_steps_cannot_be_passed_or_complete(self):
        receipt = format_receipt(
            steps=[],
            platform="ios",
            arch="arm64",
            driver_sha256="abc",
            inputs_sha256="abc",
            patch_sha256="abc",
            nvattest_revision="abc",
            regorus_revision="abc",
            regorus_original_sha256="abc",
            regorus_patched_sha256="abc",
            ca_bundle_sha256="abc",
            pcr_sha256_pins=[],
            rims=[],
            needed=[],
            helper_sha256="dummy",
            is_partial=False,
        )
        self.assertEqual(receipt["status"], "failed")
        self.assertFalse(receipt["complete"])
        self.assertNotEqual(receipt["exit"], 0)


if __name__ == "__main__":
    unittest.main()
