# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Offline failure and publication tests for the area-tier refresh tool."""

import copy
import http.client
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import refresh_location_area_tier as refresh


class RefreshTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.cache = self.root / "cache"
        self.old = b"last good area tier"
        self.previous = {
            "sha256": refresh.digest(self.old),
            "version": "previous-inputs",
            "inputs": {},
            "input_bytes": {},
            "stats": {
                "towns": 225397,
                "admin1": 3777,
                "countries": 252,
                "bytes": 7776080,
            },
        }
        self.files = {
            refresh.GEO / "area-tier.bin": self.old,
            refresh.GEO / "sources.json": json.dumps(self.previous).encode(),
            refresh.ASSET: f'const DIGEST = "{self.previous["sha256"]}";'.encode(),
            refresh.NOTICES: self.previous["sha256"].encode(),
            refresh.COMPONENT: b'let ver = "previous-inputs".to_string();\n' * 2,
        }
        for path, data in self.files.items():
            refresh.atomic_write(self.root / path, data)
        self.inputs = {name: f"download: {name}".encode() for name in refresh.INPUTS}
        self.ne = {
            name: refresh.digest(self.inputs[name]) for name in refresh.NE_DIGESTS
        }
        self.stack = self.enterContext(mock.patch.dict(refresh.NE_DIGESTS, self.ne))
        self.downloader = self.enterContext(
            mock.patch.object(
                refresh,
                "download",
                side_effect=lambda url: self.inputs[url.rsplit("/", 1)[1]],
            )
        )

    def tearDown(self):
        self.temporary.cleanup()

    def assert_untouched(self):
        for path, data in self.files.items():
            self.assertEqual((self.root / path).read_bytes(), data, str(path))

    def run_refresh(self, output, stats=None, apply=True):
        with (
            mock.patch.object(refresh, "validate_geonames"),
            mock.patch.object(
                refresh, "build", return_value=(output, stats or self.previous["stats"])
            ),
        ):
            return refresh.refresh(self.root, self.cache, apply)

    def test_download_failure_retains_every_file(self):
        self.downloader.side_effect = OSError("upstream unavailable")
        result = refresh.refresh(self.root, self.cache, True)
        self.assertEqual(result["status"], "retained")
        self.assertIn("upstream unavailable", result["reason"])
        self.assert_untouched()

    def test_malformed_http_response_retains_every_file(self):
        self.downloader.side_effect = http.client.BadStatusLine("broken HTTP")
        result = refresh.refresh(self.root, self.cache, True)
        self.assertEqual(result["status"], "retained")
        self.assertIn("BadStatusLine", result["reason"])
        self.assert_untouched()

    def test_schema_failure_and_shrunken_output_retain_every_file(self):
        with mock.patch.object(
            refresh, "validate_geonames", side_effect=ValueError("schema")
        ):
            self.assertEqual(
                refresh.refresh(self.root, self.cache, True)["status"], "retained"
            )
        stats = copy.deepcopy(self.previous["stats"])
        stats["towns"] //= 2
        self.assertEqual(self.run_refresh(b"bad output", stats)["status"], "retained")
        self.assert_untouched()

    def test_identical_output_does_not_repin_for_changed_zip_metadata(self):
        result = self.run_refresh(self.old)
        self.assertEqual(result["status"], "unchanged")
        self.assertFalse(result["sources_unchanged"])
        self.assert_untouched()

    def test_shrunken_input_retains_every_file_before_build(self):
        self.previous["input_bytes"] = {"cities500.zip": 100_000}
        self.files[refresh.GEO / "sources.json"] = json.dumps(self.previous).encode()
        (self.root / refresh.GEO / "sources.json").write_bytes(
            self.files[refresh.GEO / "sources.json"]
        )
        with mock.patch.object(refresh, "build") as builder:
            result = refresh.refresh(self.root, self.cache, True)
        self.assertEqual(result["status"], "retained")
        self.assertIn("shrunken input", result["reason"])
        builder.assert_not_called()
        self.assert_untouched()

    def test_unknown_country_schema_is_rejected_by_the_real_parser(self):
        self.inputs["countryInfo.txt"] = b"unexpected upstream schema\n"
        with mock.patch.object(refresh, "build") as builder:
            result = refresh.refresh(self.root, self.cache, True)
        self.assertEqual(result["status"], "retained")
        self.assertIn("countryInfo.txt: unexpected row schema", result["reason"])
        builder.assert_not_called()
        self.assert_untouched()

    def test_no_cut_checks_without_updating_the_checkout(self):
        result = self.run_refresh(b"new area tier", apply=False)
        self.assertEqual(result["status"], "available")
        self.assertFalse(result["applied"])
        self.assert_untouched()

    def test_changed_output_updates_all_pins_and_manifest(self):
        output = b"new area tier"
        result = self.run_refresh(output)
        self.assertEqual(result["status"], "refreshed")
        self.assertEqual(
            (self.root / refresh.GEO / "area-tier.bin").read_bytes(), output
        )
        for path in (refresh.ASSET, refresh.NOTICES):
            self.assertIn(refresh.digest(output), (self.root / path).read_text())
        self.assertEqual(
            (self.root / refresh.COMPONENT).read_text().count(result["version"]), 2
        )
        current = json.loads((self.root / refresh.GEO / "sources.json").read_text())
        self.assertEqual(current["sha256"], result["sha256"])
        self.assertEqual(current["inputs"], result["inputs"])

    def test_natural_earth_cache_is_digest_checked_and_reused(self):
        self.run_refresh(self.old)
        self.downloader.reset_mock()
        self.run_refresh(self.old)
        self.assertEqual(self.downloader.call_count, 3)
        cached = self.cache / f"{self.ne[refresh.INPUTS[3]]}-{refresh.INPUTS[3]}"
        cached.write_bytes(b"truncated")
        self.downloader.reset_mock()
        self.run_refresh(self.old)
        self.assertEqual(self.downloader.call_count, 4)
        self.assertEqual(
            refresh.digest(cached.read_bytes()), self.ne[refresh.INPUTS[3]]
        )

    def test_bad_pinned_natural_earth_retains_old_file(self):
        self.inputs[refresh.INPUTS[3]] = b"changed upstream pinned bytes"
        result = self.run_refresh(b"must not publish")
        self.assertEqual(result["status"], "retained")
        self.assertIn("digest mismatch", result["reason"])
        self.assert_untouched()

    def test_local_pin_drift_is_fatal(self):
        (self.root / refresh.ASSET).write_text("wrong local digest")
        with self.assertRaisesRegex(ValueError, "expected one occurrence"):
            self.run_refresh(self.old)
        self.downloader.assert_not_called()


if __name__ == "__main__":
    unittest.main()
