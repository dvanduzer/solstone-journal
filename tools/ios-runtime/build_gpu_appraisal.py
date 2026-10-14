#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc

"""Build and receipt generator for the iOS confidential GPU appraisal helper.

Constructs the static libnvat library and Appraisal.appex companion for iOS,
records receipt metadata with cryptographic pins, and verifies link symbols.
Boundary: compile, link, packaging, and signing identity; not a device run.
"""

import json
import os
import pathlib
import re
import sys
from typing import Any, Dict, List, Optional

# Import Build, acquire, sha256, INPUTS, ROOT from scripts/mobile_native_build.py
SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
sys.path.insert(0, str(REPO_ROOT / "scripts"))

from mobile_native_build import Build, acquire, sha256, INPUTS, ROOT, tree_digest

NVAT_PRODUCTION_SYMBOLS = [
    "nvat_str_length",
    "nvat_str_get_data",
    "nvat_str_free",
    "nvat_sdk_opts_create",
    "nvat_sdk_opts_free",
    "nvat_sdk_init",
    "nvat_ocsp_client_create_raw_proofs",
    "nvat_ocsp_client_free",
    "nvat_rim_store_create_filesystem",
    "nvat_rim_store_free",
    "nvat_nonce_from_bytes",
    "nvat_nonce_free",
    "nvat_gpu_evidence_source_from_json_string",
    "nvat_gpu_evidence_source_free",
    "nvat_gpu_evidence_collect",
    "nvat_gpu_evidence_array_free",
    "nvat_evidence_policy_create_default",
    "nvat_evidence_policy_set_gpu_claims_version",
    "nvat_evidence_policy_free",
    "nvat_gpu_local_verifier_create",
    "nvat_gpu_local_verifier_upcast",
    "nvat_gpu_verifier_free",
    "nvat_verify_gpu_evidence",
    "nvat_claims_collection_serialize_json",
    "nvat_claims_collection_free",
]

LINK_PROBE_BOUNDARY = (
    "compile, link, packaging, and signing identity; not a device run"
)


def load_regorus_build_revision() -> Dict[str, str]:
    sources_path = ROOT / "core/distribution/nvattest-windows-sources.json"
    sources_data = json.loads(sources_path.read_text(encoding="utf-8"))
    for item in sources_data.get("native_sources", []):
        if item.get("name") == "regorus-build-revision":
            return {
                "revision": item.get("revision", ""),
                "original_sha256": item.get("original_sha256", ""),
                "sha256": item.get("sha256", ""),
            }
    raise ValueError("regorus-build-revision not found in nvattest-windows-sources.json")


def load_production_pcr_pins() -> List[str]:
    pins_rs = ROOT / "core/crates/solstone-core-spp-attest/src/pins.rs"
    content = pins_rs.read_text(encoding="utf-8")
    m = re.search(
        r"pub const PRODUCTION_PCR_SHA256_PINS:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
        content,
        re.DOTALL,
    )
    if not m:
        raise ValueError("PRODUCTION_PCR_SHA256_PINS not found in pins.rs")
    pins = re.findall(r'"([0-9a-fA-F]{64})"', m.group(1))
    if not pins:
        raise ValueError("No PCR pins parsed from pins.rs")
    return pins


def format_receipt(
    steps: List[Dict[str, Any]],
    platform: str,
    arch: str,
    driver_sha256: str,
    inputs_sha256: str,
    patch_sha256: str,
    nvattest_revision: str,
    regorus_revision: str,
    regorus_original_sha256: str,
    regorus_patched_sha256: str,
    ca_bundle_sha256: str,
    pcr_sha256_pins: List[str],
    rims: List[Dict[str, str]],
    needed: List[str],
    helper_sha256: Optional[str] = None,
    is_partial: bool = False,
) -> Dict[str, Any]:
    all_steps_ok = len(steps) > 0 and all(s.get("exit", 1) == 0 for s in steps)
    passed = all_steps_ok and not is_partial and helper_sha256 is not None
    exit_code = 0 if passed else 1
    status = "passed" if passed else "failed"

    return {
        "status": status,
        "complete": passed,
        "exit": exit_code,
        "platform": platform,
        "arch": arch,
        "driver_sha256": driver_sha256,
        "inputs_sha256": inputs_sha256,
        "patch_sha256": patch_sha256,
        "nvattest_revision": nvattest_revision,
        "regorus_revision": regorus_revision,
        "regorus_original_sha256": regorus_original_sha256,
        "regorus_patched_sha256": regorus_patched_sha256,
        "ca_bundle_sha256": ca_bundle_sha256,
        "pcr_sha256_pins": pcr_sha256_pins,
        "rims": rims,
        "needed": needed,
        "helper_sha256": helper_sha256 or "",
        "schema": "gpu-appraisal-receipt-v1",
        "steps": steps,
        "link_boundary": LINK_PROBE_BOUNDARY,
    }


def write_receipt(
    destination: pathlib.Path,
    steps: List[Dict[str, Any]],
    needed: Optional[List[str]] = None,
    platform: str = "ios",
    arch: str = "arm64",
    helper_path: Optional[pathlib.Path] = None,
    is_partial: bool = False,
) -> Dict[str, Any]:
    driver_path = ROOT / "scripts/mobile_native_build.py"
    inputs_path = ROOT / "core/distribution/mobile/inputs.json"
    patch_path = ROOT / "core/distribution/mobile/nvattest-mobile.patch"
    ca_bundle_path = ROOT / "core/crates/solstone-core-spp-attest/share/ca/ca-bundle.pem"

    if not hasattr(Build, "pins"):
        Build.pins = json.loads(inputs_path.read_text(encoding="utf-8"))["inputs"]
    nvattest_rev = Build.pins["nvattest"]["revision"]

    regorus_info = load_regorus_build_revision()
    regorus_rev = regorus_info.get("revision", "")
    regorus_orig = regorus_info.get("original_sha256", "")
    regorus_patched = regorus_info.get("sha256", "")

    rims_dir = ROOT / "core/crates/solstone-core-spp-attest/rims"
    rim_entries = []
    if rims_dir.exists():
        for r_file in sorted(rims_dir.iterdir()):
            if r_file.suffix in (".xml", ".corim"):
                rim_entries.append({
                    "filename": r_file.name,
                    "sha256": sha256(r_file),
                })

    pcr_pins = load_production_pcr_pins()

    helper_digest = sha256(helper_path) if helper_path and helper_path.exists() else None

    receipt = format_receipt(
        steps=steps,
        platform=platform,
        arch=arch,
        driver_sha256=sha256(driver_path) if driver_path.exists() else "",
        inputs_sha256=sha256(inputs_path) if inputs_path.exists() else "",
        patch_sha256=sha256(patch_path) if patch_path.exists() else "",
        nvattest_revision=nvattest_rev,
        regorus_revision=regorus_rev,
        regorus_original_sha256=regorus_orig,
        regorus_patched_sha256=regorus_patched,
        ca_bundle_sha256=sha256(ca_bundle_path) if ca_bundle_path.exists() else "",
        pcr_sha256_pins=pcr_pins,
        rims=rim_entries,
        needed=needed if needed is not None else [],
        helper_sha256=helper_digest,
        is_partial=is_partial,
    )

    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    return receipt


def main() -> int:
    sys.stderr.write("build_gpu_appraisal main is not intended for standalone execution\n")
    return 1


if __name__ == "__main__":
    sys.exit(main())
