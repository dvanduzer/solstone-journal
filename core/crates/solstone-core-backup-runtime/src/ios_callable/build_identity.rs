// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Read-only linked provenance. This creates no journal or callable authority.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "solstone-mobile-backup-linked-build-v1";
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const BUILDER: &[u8] = include_bytes!("../../../../../scripts/backup_bridge_build.py");
const ACQUISITION_HELPER: &[u8] = include_bytes!("../../../../../scripts/android_cross_build.py");
const INPUTS: &[u8] = include_bytes!("../../../../distribution/mobile/inputs.json");
const SAFETY_PATCH: &[u8] =
    include_bytes!("../../../../distribution/mobile/backup-bridge/restic-library-safety.patch");
const ROOTED_PATCH: &[u8] =
    include_bytes!("../../../../distribution/mobile/backup-bridge/restic-rooted-restorer.patch");
const EXISTING_PATCH: &[u8] =
    include_bytes!("../../../../distribution/mobile/backup-bridge/restic-existing-root.patch");

// Maintained separately from the packaged receipt. Qualification must verify
// this against the complete reviewed bridge tree, including added files.
include!("build_source_pin.rs");

const REQUIRED_STEPS: &[&str] = &[
    "library-safety-overlay",
    "rooted-restorer-overlay",
    "existing-root-overlay",
    "go-download",
    "go-verify",
    "go-deps",
    "go-test",
    "rooted-metadata-tests",
    "existing-root-tests",
    "receipt-control-tests",
    "go-compile-integration",
    "go-run-integration",
    "host-c-archive-build",
    "host-c-link",
    "host-c-run",
    "metadata-c-link",
    "metadata-c-run",
    "selection-c-link",
    "selection-c-run",
    "options-c-link",
    "options-c-run",
    "directories-c-link",
    "directories-c-run",
    "identity-c-link",
    "identity-c-run",
    "existing-root-c-link",
    "existing-root-c-run",
    "workspace-c-link",
    "workspace-c-run",
    "s3-c-link",
    "s3-c-run",
    "c-archive-build",
    "target-source-selection",
    "target-archive-symbols",
    "c-link-check",
    "caller-platform",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    schema: String,
    abi: String,
    goos: String,
    goarch: String,
    inputs_sha256: String,
    builder_sha256: String,
    acquisition_helper_sha256: String,
    bridge_source_sha256: String,
    library_safety_patch_sha256: String,
    rooted_restorer_patch_sha256: String,
    existing_root_patch_sha256: String,
    restic_archive_sha256: String,
    toolchain_archive_sha256: String,
}

#[derive(Deserialize)]
struct Step {
    name: String,
    exit: i32,
    log_sha256: String,
}

#[derive(Deserialize)]
struct Record {
    status: String,
    complete: bool,
    exit: i32,
    target: String,
    goos: String,
    goarch: String,
    compile_exit: i32,
    archive_sha256: String,
    header_sha256: String,
    inputs_sha256: String,
    builder_sha256: String,
    bridge_source_sha256: String,
    library_safety_patch_sha256: String,
    rooted_restorer_patch_sha256: String,
    existing_root_patch_sha256: String,
    restic_archive_sha256: String,
    toolchain_archive_sha256: String,
    linked_build_identity: Identity,
    linked_build_identity_sha256: String,
    steps: Vec<Step>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BuildIdentityRefusal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ValidatedLinkedBuild {
    pub identity: [u8; 32],
    pub record_sha256: [u8; 32],
    pub archive_sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    value
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn valid_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn compiled_identity(
    goos: &str,
    goarch: &str,
    toolchain: &str,
) -> Result<Identity, BuildIdentityRefusal> {
    let inputs: serde_json::Value =
        serde_json::from_slice(INPUTS).map_err(|_| BuildIdentityRefusal)?;
    let pin = |component: &str| -> Result<String, BuildIdentityRefusal> {
        inputs["inputs"][component]["sha256"]
            .as_str()
            .filter(|value| valid_sha(value))
            .map(str::to_owned)
            .ok_or(BuildIdentityRefusal)
    };
    if !valid_sha(BRIDGE_SOURCE_SHA256) {
        return Err(BuildIdentityRefusal);
    }
    Ok(Identity {
        schema: SCHEMA.into(),
        abi: "1".into(),
        goos: goos.into(),
        goarch: goarch.into(),
        inputs_sha256: sha(INPUTS),
        builder_sha256: sha(BUILDER),
        acquisition_helper_sha256: sha(ACQUISITION_HELPER),
        bridge_source_sha256: BRIDGE_SOURCE_SHA256.into(),
        library_safety_patch_sha256: sha(SAFETY_PATCH),
        rooted_restorer_patch_sha256: sha(ROOTED_PATCH),
        existing_root_patch_sha256: sha(EXISTING_PATCH),
        restic_archive_sha256: pin("restic")?,
        toolchain_archive_sha256: pin(toolchain)?,
    })
}

fn fingerprint(identity: &Identity) -> Result<[u8; 32], BuildIdentityRefusal> {
    // BTreeMap fixes order even when serde_json's preserve_order is enabled by
    // another workspace consumer. The closed Identity decoder rejects ambiguity.
    let sorted: BTreeMap<String, String> =
        serde_json::from_value(serde_json::to_value(identity).map_err(|_| BuildIdentityRefusal)?)
            .map_err(|_| BuildIdentityRefusal)?;
    let canonical = serde_json::to_vec(&sorted).map_err(|_| BuildIdentityRefusal)?;
    let mut digest = Sha256::new();
    digest.update(SCHEMA.as_bytes());
    digest.update([0]);
    digest.update(canonical);
    Ok(digest.finalize().into())
}

fn validate_record(
    bytes: &[u8],
    expected: &Identity,
    target: &str,
) -> Result<ValidatedLinkedBuild, BuildIdentityRefusal> {
    if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
        return Err(BuildIdentityRefusal);
    }
    let r: Record = serde_json::from_slice(bytes).map_err(|_| BuildIdentityRefusal)?;
    let names: BTreeSet<_> = r.steps.iter().map(|step| step.name.as_str()).collect();
    let required: BTreeSet<_> = REQUIRED_STEPS.iter().copied().collect();
    if r.status != "passed"
        || !r.complete
        || r.exit != 0
        || r.compile_exit != 0
        || r.target != target
        || r.goos != expected.goos
        || r.goarch != expected.goarch
        || r.linked_build_identity != *expected
        || r.steps.len() != REQUIRED_STEPS.len()
        || names != required
        || r.steps
            .iter()
            .any(|step| step.exit != 0 || !valid_sha(&step.log_sha256))
        || !valid_sha(&r.archive_sha256)
        || !valid_sha(&r.header_sha256)
        || r.inputs_sha256 != expected.inputs_sha256
        || r.builder_sha256 != expected.builder_sha256
        || r.bridge_source_sha256 != expected.bridge_source_sha256
        || r.library_safety_patch_sha256 != expected.library_safety_patch_sha256
        || r.rooted_restorer_patch_sha256 != expected.rooted_restorer_patch_sha256
        || r.existing_root_patch_sha256 != expected.existing_root_patch_sha256
        || r.restic_archive_sha256 != expected.restic_archive_sha256
        || r.toolchain_archive_sha256 != expected.toolchain_archive_sha256
    {
        return Err(BuildIdentityRefusal);
    }
    let identity = fingerprint(expected)?;
    if r.linked_build_identity_sha256 != hex(&identity) {
        return Err(BuildIdentityRefusal);
    }
    Ok(ValidatedLinkedBuild {
        identity,
        record_sha256: Sha256::digest(bytes).into(),
        archive_sha256: r.archive_sha256,
    })
}

#[allow(unsafe_code)] // One read-only, bounded export from the linked Go archive.
fn actual_linked_identity() -> Result<[u8; 32], BuildIdentityRefusal> {
    // Only the app links the Go archive (`ios-linked-bridge`); without it no
    // linked build exists to admit.
    #[cfg(feature = "ios-linked-bridge")]
    unsafe extern "C" {
        fn restic_bridge_build_identity_v1(output: *mut u8, capacity: u32) -> i32;
    }
    #[cfg(not(feature = "ios-linked-bridge"))]
    unsafe fn restic_bridge_build_identity_v1(_: *mut u8, _: u32) -> i32 {
        1
    }
    let mut actual = [0u8; 32];
    // SAFETY: output is a live exactly32-byte array for the synchronous copy.
    if unsafe { restic_bridge_build_identity_v1(actual.as_mut_ptr(), 32) } != 0 {
        return Err(BuildIdentityRefusal);
    }
    Ok(actual)
}

fn validate_with_expected(
    bytes: &[u8],
    expected: &Identity,
    target: &str,
) -> Result<ValidatedLinkedBuild, BuildIdentityRefusal> {
    let validated = validate_record(bytes, expected, target)?;
    if actual_linked_identity()? != validated.identity {
        return Err(BuildIdentityRefusal);
    }
    Ok(validated)
}

/// The future startup scanner must supply its fixed signed-bundle record bytes.
/// Production accepts only the independently compiled iOS/arm64 provenance.
pub(super) fn validate_linked_build(
    bytes: &[u8],
) -> Result<ValidatedLinkedBuild, BuildIdentityRefusal> {
    validate_with_expected(bytes, &compiled_identity("ios", "arm64", "go-ios")?, "ios")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn complete_record(identity: &Identity, target: &str) -> Value {
        let mut value = json!({
            "status":"passed", "complete":true, "exit":0, "target":target,
            "goos":identity.goos, "goarch":identity.goarch, "compile_exit":0,
            "archive_sha256":"a".repeat(64), "header_sha256":"b".repeat(64),
            "linked_build_identity":identity, "linked_build_identity_sha256":hex(&fingerprint(identity).unwrap()),
            "steps": REQUIRED_STEPS.iter().map(|name| json!({"name":name,"exit":0,"log_sha256":"c".repeat(64)})).collect::<Vec<_>>()
        });
        for (key, part) in serde_json::to_value(identity).unwrap().as_object().unwrap() {
            if key.ends_with("_sha256") && key != "acquisition_helper_sha256" {
                value[key] = part.clone();
            }
        }
        value
    }

    #[test]
    fn actual_linked_archive_control() {
        // The external integration controller supplies only synthetic host
        // fixtures. No environment value participates in production admission.
        let Ok(path) = std::env::var("IOS_LINKED_IDENTITY_TEST_RECEIPT") else {
            return;
        };
        let expectation = std::env::var("IOS_LINKED_IDENTITY_TEST_EXPECT").unwrap();
        assert!(matches!(expectation.as_str(), "valid" | "refused"));
        let bytes = std::fs::read(path).unwrap();
        let expected = compiled_identity("linux", "amd64", "go-android").unwrap();
        let result = validate_with_expected(&bytes, &expected, "linux");
        assert_eq!(
            result.is_ok(),
            expectation == "valid",
            "actual linked archive admission differed"
        );
        assert!(
            validate_linked_build(&bytes).is_err(),
            "host archive admitted as iOS"
        );
        println!("ACTUAL_LINKED_IDENTITY_CONTROL_PROVED {expectation}");
    }

    #[test]
    fn actual_immutable_ios_recipe_control() {
        let Ok(path) = std::env::var("IOS_LINKED_IDENTITY_TEST_IOS_RECIPE") else {
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let expected = compiled_identity("ios", "arm64", "go-ios").unwrap();
        let validated = validate_record(&bytes, &expected, "ios").unwrap();
        assert_eq!(validated.identity, fingerprint(&expected).unwrap());
        // Receipt validation is demonstrated here; this host-linked process
        // must still refuse iOS execution authority from that real receipt.
        assert!(validate_linked_build(&bytes).is_err());
        println!("ACTUAL_IMMUTABLE_IOS_RECIPE_CONTROL_PROVED");
    }

    #[test]
    fn compiled_pins_and_closed_record_refuse_each_changed_field() {
        let expected = compiled_identity("ios", "arm64", "go-ios").unwrap();
        let good = complete_record(&expected, "ios");
        assert!(validate_record(&serde_json::to_vec(&good).unwrap(), &expected, "ios").is_ok());
        for key in serde_json::to_value(&expected)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
        {
            let mut bad = good.clone();
            bad["linked_build_identity"][key] = json!("changed");
            assert!(
                validate_record(&serde_json::to_vec(&bad).unwrap(), &expected, "ios").is_err(),
                "accepted changed {key}"
            );
        }
        for key in [
            "status",
            "complete",
            "exit",
            "target",
            "goos",
            "goarch",
            "compile_exit",
            "archive_sha256",
            "header_sha256",
            "linked_build_identity_sha256",
        ] {
            let mut bad = good.clone();
            bad[key] = Value::Null;
            assert!(
                validate_record(&serde_json::to_vec(&bad).unwrap(), &expected, "ios").is_err(),
                "accepted missing {key}"
            );
        }
        for bad in [json!({"extra":"value"}), Value::Null] {
            let mut r = good.clone();
            r["linked_build_identity"] = bad;
            assert!(validate_record(&serde_json::to_vec(&r).unwrap(), &expected, "ios").is_err());
        }
        let mut extra = good.clone();
        extra["linked_build_identity"]["extra"] = json!("value");
        assert!(validate_record(&serde_json::to_vec(&extra).unwrap(), &expected, "ios").is_err());
        let mut partial = good.clone();
        partial["steps"].as_array_mut().unwrap().pop();
        assert!(validate_record(&serde_json::to_vec(&partial).unwrap(), &expected, "ios").is_err());
        let mut failed = good.clone();
        failed["steps"][0]["exit"] = json!(7);
        assert!(validate_record(&serde_json::to_vec(&failed).unwrap(), &expected, "ios").is_err());
        let mut duplicate = good.clone();
        duplicate["steps"][0] = duplicate["steps"][1].clone();
        assert!(
            validate_record(&serde_json::to_vec(&duplicate).unwrap(), &expected, "ios").is_err()
        );
    }
}
