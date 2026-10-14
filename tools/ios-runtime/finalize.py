#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
"""Generate ExtensionKit metadata with the selected SDK tool, then development-sign."""
import argparse
import pathlib
import plistlib
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("build_root", type=pathlib.Path)
parser.add_argument("--keychain", default=str(pathlib.Path.home() / "Library/Keychains/sol-signing.keychain-db"))
parser.add_argument("--identity", default="Apple Development: Created via API (SNP7CMKMZ5)")
a = parser.parse_args()
products = a.build_root / "derived/Build/Products/Debug-iphoneos"
intermediates = a.build_root / "derived/Build/Intermediates.noindex/JournalRuntimeDev.build/Debug-iphoneos"
app = products / "JournalRuntimeDev.app"
sdk = subprocess.check_output(["xcrun", "--sdk", "iphoneos", "--show-sdk-path"], text=True).strip()
xcode_version = subprocess.check_output(["xcodebuild", "-version"], text=True).split()[-1]
for name, bundle in [("JournalWorker", app / "Extensions/JournalWorker.appex"),
                     ("Appraisal", app / "Extensions/Appraisal.appex"),
                     ("JournalRuntimeDev", app)]:
    info_path = bundle / "Info.plist"
    info = plistlib.loads(info_path.read_bytes())
    if name == "JournalRuntimeDev":
        source = "Host"
    elif name == "Appraisal":
        source = "Appraisal"
    else:
        source = "Helper"
    inputs = [intermediates / f"{name}.build/Objects-normal/arm64/{source}.swiftconstvalues"]
    assert inputs, f"No compiler metadata for {name}"
    host = name == "JournalRuntimeDev"
    output = bundle / "Extensions/JournalRuntimeDev.appexpt" if host else a.build_root / f"{name}-metadata.plist"
    output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["xcrun", "exutil", "extract-extension-points" if host else "generate-appextension-plist",
                    "--module-name", name, "--sdk-root", sdk, "--xcode-version", xcode_version,
                    "--platform-family", "iOS", "--deployment-target", "26.0",
                    "--bundle-identifier", info["CFBundleIdentifier"], "--output", str(output),
                    *map(str, inputs)], check=True)
    generated = plistlib.loads(output.read_bytes())
    if not host:
        assert "EXAppExtensionAttributes" in generated, generated
        info.update(generated)
        info_path.write_bytes(plistlib.dumps(info))
    else:
        assert generated, "No extension points generated"
    entitlement = intermediates / f"{name}.build/{bundle.name}.xcent"
    subprocess.run(["codesign", "--force", "--sign", a.identity, "--keychain", a.keychain,
                    "--entitlements", str(entitlement), "--timestamp=none", "--generate-entitlement-der", str(bundle)], check=True)
subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
print("Generated ExtensionKit metadata and verified development signatures")
