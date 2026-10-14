#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import json
import pathlib
import struct
import tempfile
import unittest
from types import SimpleNamespace

from build_gpu_appraisal import (
    _configure_argv,
    _record_digest,
    _static_dependencies,
)
from finalize import canonical_digest, macho_content_sha256, package_members


class AppraisalBuildTests(unittest.TestCase):
    def test_static_reconfigure_preserves_exact_driver_command(self):
        build = SimpleNamespace(
            summary={
                "steps": [
                    {
                        "name": "nvattest-configure",
                        "argv": [
                            "cmake",
                            "-S",
                            "/source",
                            "-B",
                            "/build",
                            "-DBUILD_SHARED_LIBS=ON",
                            "-DFETCHCONTENT_SOURCE_DIR_REGORUS=/pinned",
                        ],
                    }
                ]
            }
        )
        self.assertEqual(
            _configure_argv(build),
            [
                "cmake",
                "-S",
                "/source",
                "-B",
                "/build",
                "-DFETCHCONTENT_SOURCE_DIR_REGORUS=/pinned",
                "-DBUILD_SHARED_LIBS=OFF",
                "-DCMAKE_SKIP_INSTALL_RULES=ON",
            ],
        )

    def test_dependency_census_is_ordered_and_deduplicated(self):
        root = pathlib.Path("/tmp/build")
        command = "clang -o libnvat.dylib dep/a.a /fixed/b.a dep/a.a -lz"
        self.assertEqual(
            _static_dependencies(command, root),
            [(root / "dep/a.a").resolve(), pathlib.Path("/fixed/b.a")],
        )

    def test_build_record_digest_excludes_no_fields_implicitly(self):
        record = {"complete": True, "steps": [{"exit": 0}]}
        self.assertEqual(_record_digest(record), canonical_digest(record))
        self.assertNotEqual(
            _record_digest(record),
            _record_digest({**record, "extra": "substitution"}),
        )

    def test_package_census_binds_all_non_signature_members(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as temp:
            root = pathlib.Path(temp)
            (root / "_CodeSignature").mkdir()
            (root / "_CodeSignature/CodeResources").write_bytes(b"mutable signature")
            (root / "gpu-appraisal-receipt.json").write_text("{}")
            header = bytearray(32)
            header[:4] = b"\xcf\xfa\xed\xfe"
            struct.pack_into("<II", header, 16, 1, 16)
            command = struct.pack("<IIII", 0x1D, 16, 48, 4)
            (root / "Appraisal").write_bytes(header + command + b"sign")
            (root / "Info.plist").write_bytes(b"plist")
            members = package_members(root)
            self.assertEqual([item["path"] for item in members], ["Appraisal", "Info.plist"])

    def test_macho_content_digest_excludes_only_signature_blob(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as temp:
            path = pathlib.Path(temp) / "Appraisal"
            header = bytearray(32)
            header[:4] = b"\xcf\xfa\xed\xfe"
            struct.pack_into("<II", header, 16, 1, 16)
            command = struct.pack("<IIII", 0x1D, 16, 48, 4)
            path.write_bytes(header + command + b"aaaa")
            first = macho_content_sha256(path)
            path.write_bytes(header + command + b"bbbb")
            self.assertEqual(first, macho_content_sha256(path))
            header[8] = 1
            path.write_bytes(header + command + b"bbbb")
            self.assertNotEqual(first, macho_content_sha256(path))

    def test_package_census_refuses_symlinks(self):
        with tempfile.TemporaryDirectory(dir="/var/tmp") as temp:
            root = pathlib.Path(temp)
            (root / "target").write_bytes(b"x")
            (root / "alias").symlink_to("target")
            with self.assertRaisesRegex(ValueError, "symlink"):
                package_members(root)


if __name__ == "__main__":
    unittest.main()
