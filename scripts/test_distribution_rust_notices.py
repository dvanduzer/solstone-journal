#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import distribution_rust_notices as notices


class NoticePathDependencyTests(unittest.TestCase):
    def test_unknown_path_dependency_is_refused_in_the_actual_closure_walk(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dist = root / "core/distribution"
            dist.mkdir(parents=True)
            (root / "core/Cargo.toml").write_text('[workspace]\nmembers = ["crates/owned"]\nexclude = ["external", "crates/solstone-core-service-legacy-evidence"]\n')
            (dist / "inventory.toml").write_text('[[entry]]\nkind="bin"\npackage="owned"\ntargets=["linux-x86_64"]\n')
            def package(name, manifest):
                return {"id": name, "name": name, "version": "1.0.0", "source": None, "manifest_path": str(manifest)}
            metadata = {
                "packages": [package("owned", root / "core/crates/owned/Cargo.toml"),
                             package("unreviewed", root / "core/external/Cargo.toml")],
                "resolve": {"nodes": [{"id": "owned", "deps": [{"pkg": "unreviewed", "dep_kinds": [{"kind": None}]}]},
                                      {"id": "unreviewed", "deps": []}]}
            }
            result = subprocess.CompletedProcess([], 0, stdout=json.dumps(metadata).encode(), stderr=b"")
            with patch.object(notices, "ROOT", root), patch.object(notices, "DIST", dist), \
                 patch.object(notices.subprocess, "run", return_value=result), \
                 patch.object(notices, "shipped_feature_arguments", return_value=[]):
                with self.assertRaisesRegex(RuntimeError, "unaccounted no-source dependency: unreviewed"):
                    notices.closure("linux-x86_64", "x86_64-unknown-linux-gnu")
                metadata["packages"][1]["name"] = "solstone-core-service-legacy-evidence"
                metadata["packages"][1]["manifest_path"] = str(root / "core/crates/solstone-core-service-legacy-evidence/Cargo.toml")
                result.stdout = json.dumps(metadata).encode()
                self.assertEqual(notices.closure("linux-x86_64", "x86_64-unknown-linux-gnu"), ([], {}))

    def test_old_index_refuses_vendor_and_origin_changed_together(self):
        original_root = notices.ROOT
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dist = root / "core/distribution"
            for relative in ["core/vendor/velopack", "core/distribution/licenses/velopack"]:
                shutil.copytree(original_root / relative, root / relative)
            lock = root / "core/Cargo.lock"
            lock.write_text("version = 4\n")
            identities = [notices.VELOPACK_IDENTITY]
            metadata = {notices.VELOPACK_IDENTITY: {"name": "velopack", "version": "1.2.0", "source": None}}
            with patch.object(notices, "ROOT", root), patch.object(notices, "DIST", dist), \
                 patch.object(notices, "LOCK", lock), \
                 patch.object(notices, "closure", return_value=(identities, metadata)), \
                 patch.object(notices, "shipped_feature_arguments", return_value=[]):
                notices.write("linux-x86_64", "x86_64-unknown-linux-gnu", {"packages": []}, {})
                notices.check("linux-x86_64", "x86_64-unknown-linux-gnu")
                vendor = root / "core/vendor/velopack"
                before = notices.patched_velopack_tree_sha256(vendor)
                source = vendor / "src/bundle.rs"
                source.write_bytes(source.read_bytes() + b"\n")
                after = notices.patched_velopack_tree_sha256(vendor)
                origin = vendor / "ORIGIN.toml"
                text = origin.read_text()
                self.assertEqual(text.count(before), 1)
                origin.write_text(text.replace(before, after))
                notices.prove_velopack_provenance()
                with self.assertRaisesRegex(RuntimeError, "Rust notices are missing or stale"):
                    notices.check("linux-x86_64", "x86_64-unknown-linux-gnu")


if __name__ == "__main__":
    unittest.main()
