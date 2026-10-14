// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Process-global integration test for Android service tools OnceLock lifecycle
//! and environment variable isolation.
//!
//! Because `solstone-core-spp-attest::android_service_tools` uses a process-global
//! `OnceLock` for admitted tools, this test runs in its own binary target to ensure
//! isolation from other tests.

use std::fs;
use std::path::Path;

use solstone_core_backup_runtime::engine::{Clock, prepare};
use solstone_core_backup_runtime::resolve::{
    LiveAndroidAuthority, ToolPlatform, resolve_operational_tools_for_platform,
    resolve_restic_member_for_platform, resolve_tools_for_platform,
};
use solstone_core_backup_runtime::runner::ToolRunner;
use solstone_core_spp_attest::android_service_tools::{
    ANDROID_CA_BUNDLE_FILENAME, ANDROID_NVAT_FILENAME, ANDROID_NVATTEST_FILENAME,
    ANDROID_RCLONE_FILENAME, ANDROID_RESTIC_FILENAME, AndroidServiceToolRole,
    AndroidServiceToolsRefusal, has_admitted_android_service_tools, install_android_service_tools,
    read_test_ca_bundle_pem, render_android_service_tools_manifest,
    resolve_admitted_android_service_tool, revalidate_installed_android_service_tools,
};
use solstone_core_spp_attest::nvgpu::ManifestSet;
use tempfile::tempdir;

struct NoOpRunner;

impl ToolRunner for NoOpRunner {
    fn run(
        &self,
        _: &solstone_core_backup_runtime::runner::ToolRequest<'_>,
    ) -> std::io::Result<solstone_core_backup_runtime::runner::ToolOutput> {
        panic!("resolve must not invoke runner")
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        1_700_000_000
    }
    fn iso_week(&self) -> u8 {
        1
    }
}

fn configured_journal() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let destination = solstone_core_backup::Destination {
        repository: "s3:repo".into(),
        backend: "s3".into(),
        credentials: serde_json::json!({"access_key_id":"access","secret_access_key":"secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    solstone_core_backup::set_destination(dir.path(), &destination).unwrap();
    solstone_core_backup::generate_and_store_keys(dir.path()).unwrap();
    solstone_core_backup::set_enabled(dir.path(), true).unwrap();
    dir
}

const MOBILE_NATIVE_DRIVER: &[u8] = include_bytes!("../../../../scripts/mobile_native_build.py");
const MOBILE_NATIVE_INPUTS: &[u8] = include_bytes!("../../../distribution/mobile/inputs.json");
const MOBILE_NVATTEST_PATCH: &[u8] =
    include_bytes!("../../../distribution/mobile/nvattest-mobile.patch");

fn setup_fixture_roots(native_dir: &Path, resource_dir: &Path) {
    let restic_bytes = b"\x7fELFdummy-restic-pie";
    let rclone_bytes = b"\x7fELFdummy-rclone-pie";
    let nvattest_bytes = solstone_core_distribution::elf::fixture_shared_needed(
        solstone_core_distribution::elf::machine_aarch64(),
        &["libnvat.so", "libm.so", "libdl.so", "libc.so"],
        None,
        None,
    );
    let nvat_bytes = solstone_core_distribution::elf::fixture_shared_needed(
        solstone_core_distribution::elf::machine_aarch64(),
        &["libz.so", "liblog.so", "libdl.so", "libm.so", "libc.so"],
        Some(ANDROID_NVAT_FILENAME),
        None,
    );
    fs::write(native_dir.join(ANDROID_RESTIC_FILENAME), restic_bytes).unwrap();
    fs::write(native_dir.join(ANDROID_RCLONE_FILENAME), rclone_bytes).unwrap();
    fs::write(native_dir.join(ANDROID_NVATTEST_FILENAME), nvattest_bytes).unwrap();
    fs::write(native_dir.join(ANDROID_NVAT_FILENAME), nvat_bytes).unwrap();

    let ca_bytes = read_test_ca_bundle_pem().unwrap();
    fs::write(resource_dir.join(ANDROID_CA_BUNDLE_FILENAME), ca_bytes).unwrap();

    for rim in ManifestSet::QUALIFIED_595_71_05.entries() {
        fs::write(resource_dir.join(rim.filename()), rim.xml()).unwrap();
    }
}

fn receipt_for_tree(native_dir: &Path, resource_dir: &Path) -> Vec<u8> {
    let sha = solstone_core_distribution::digest::sha256_hex;
    let artifact = |component: &str, bytes: &[u8]| {
        serde_json::json!({
            "component": component,
            "path": component,
            "sha256": sha(bytes),
            "size_bytes": bytes.len(),
        })
    };
    let restic_pin = solstone_core_distribution::restic_input_pin();
    let rclone_pin = solstone_core_distribution::rclone_input_pin();
    let nvattest_pin = solstone_core_distribution::nvattest_input_pin();
    serde_json::to_vec(&serde_json::json!({
        "platform": "android",
        "complete": true,
        "status": "passed",
        "exit": 0,
        "driver_sha256": sha(MOBILE_NATIVE_DRIVER),
        "inputs_sha256": sha(MOBILE_NATIVE_INPUTS),
        "inputs": {
            "restic": {"revision": restic_pin.revision, "sha256": restic_pin.archive_sha256},
            "rclone": {"revision": rclone_pin.revision, "sha256": rclone_pin.archive_sha256},
            "nvattest": {
                "revision": solstone_core_distribution::NVATTEST_SDK_REVISION,
                "sha256": nvattest_pin.archive_sha256,
            },
        },
        "patches": [{"component": "nvattest", "sha256": sha(MOBILE_NVATTEST_PATCH)}],
        "artifacts": [
            artifact("restic-cli", &fs::read(native_dir.join(ANDROID_RESTIC_FILENAME)).unwrap()),
            artifact("rclone-cli", &fs::read(native_dir.join(ANDROID_RCLONE_FILENAME)).unwrap()),
            artifact("nvattest-cli", &fs::read(native_dir.join(ANDROID_NVATTEST_FILENAME)).unwrap()),
            artifact("nvattest-library", &fs::read(native_dir.join(ANDROID_NVAT_FILENAME)).unwrap()),
            artifact("nvattest-ca-bundle", &fs::read(resource_dir.join(ANDROID_CA_BUNDLE_FILENAME)).unwrap()),
        ],
    }))
    .unwrap()
}

#[test]
#[allow(unsafe_code)]
fn android_service_tools_process_lifecycle_and_env_isolation() {
    let dummy_exe = Path::new("/system/bin/app_process64");
    let journal = configured_journal();
    let capability = prepare(journal.path(), &FixedClock).unwrap();

    // 1. Pre-installation state:
    assert!(!has_admitted_android_service_tools());
    assert!(resolve_admitted_android_service_tool(AndroidServiceToolRole::Restic).is_err());
    assert!(resolve_admitted_android_service_tool(AndroidServiceToolRole::Rclone).is_err());
    assert!(revalidate_installed_android_service_tools().is_err());

    let err = resolve_tools_for_platform(
        &capability,
        &NoOpRunner,
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap_err();
    assert_eq!(err.detail(), "member-missing");

    let err_restic = resolve_restic_member_for_platform(
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap_err();
    assert_eq!(err_restic.detail(), "member-missing");

    let home = tempdir().unwrap();
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("PATH", "/injected/bin:/usr/bin");
        std::env::set_var("LD_LIBRARY_PATH", "/injected/lib");
        std::env::set_var("SOLSTONE_RESTIC_BUNDLE", "/injected/bundle");
        std::env::set_var("SOLSTONE_RCLONE_BUNDLE", "/injected/bundle");
    }

    // 2. Prepare valid fixture roots
    let native_temp = tempdir().unwrap();
    let resource_temp = tempdir().unwrap();
    setup_fixture_roots(native_temp.path(), resource_temp.path());

    let restic_full = native_temp.path().join(ANDROID_RESTIC_FILENAME);
    let rclone_full = native_temp.path().join(ANDROID_RCLONE_FILENAME);

    let receipt = receipt_for_tree(native_temp.path(), resource_temp.path());
    let manifest_bytes =
        render_android_service_tools_manifest(native_temp.path(), resource_temp.path(), &receipt)
            .unwrap();

    let mut stamped = serde_json::from_slice::<serde_json::Value>(&receipt).unwrap();
    for artifact in stamped["artifacts"].as_array_mut().unwrap() {
        if artifact["component"] == "restic-cli" {
            artifact["sha256"] = serde_json::Value::String("0".repeat(64));
        }
    }
    let stamp_err = install_android_service_tools(
        native_temp.path(),
        resource_temp.path(),
        &manifest_bytes,
        &serde_json::to_vec(&stamped).unwrap(),
    )
    .unwrap_err();
    assert_eq!(stamp_err.code(), "pin-mismatch");
    assert!(!has_admitted_android_service_tools());

    // 3. Install into OnceLock
    install_android_service_tools(
        native_temp.path(),
        resource_temp.path(),
        &manifest_bytes,
        &receipt,
    )
    .unwrap();

    // Idempotent reinstall: identical roots and manifest bytes must succeed
    install_android_service_tools(
        native_temp.path(),
        resource_temp.path(),
        &manifest_bytes,
        &receipt,
    )
    .unwrap();

    // A receipt that does not match the packaged bytes fails admission and does
    // not replace the authority.
    let stamp_after = install_android_service_tools(
        native_temp.path(),
        resource_temp.path(),
        &manifest_bytes,
        &serde_json::to_vec(&stamped).unwrap(),
    )
    .unwrap_err();
    assert_eq!(stamp_after.code(), "pin-mismatch");

    // Rejection on difference: different root or manifest bytes returns AlreadyInstalled
    let other_temp = tempdir().unwrap();
    setup_fixture_roots(other_temp.path(), resource_temp.path());
    let other_receipt = receipt_for_tree(other_temp.path(), resource_temp.path());
    let diff_err = install_android_service_tools(
        other_temp.path(),
        resource_temp.path(),
        &manifest_bytes,
        &other_receipt,
    )
    .unwrap_err();
    assert!(matches!(
        diff_err,
        AndroidServiceToolsRefusal::AlreadyInstalled
    ));
    assert_eq!(diff_err.code(), "already-installed");

    // 4. Post-installation queries:
    assert!(has_admitted_android_service_tools());
    assert_eq!(
        resolve_admitted_android_service_tool(AndroidServiceToolRole::Restic).unwrap(),
        restic_full
    );
    assert_eq!(
        resolve_admitted_android_service_tool(AndroidServiceToolRole::Rclone).unwrap(),
        rclone_full
    );
    assert!(revalidate_installed_android_service_tools().is_ok());

    // 5. Environmental isolation: planted environment must not alter resolution
    let resolved = resolve_tools_for_platform(
        &capability,
        &NoOpRunner,
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap();
    assert_eq!(resolved.restic_path, restic_full);
    assert_eq!(resolved.rclone_path, None);

    let resolved_restic = resolve_restic_member_for_platform(
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap();
    assert_eq!(resolved_restic, restic_full);

    let non_operated = resolve_operational_tools_for_platform(
        &NoOpRunner,
        journal.path(),
        true,
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap();
    assert_eq!(non_operated.restic_path, restic_full);
    assert_eq!(non_operated.rclone_path, None);

    // Operated mode needs rclone:
    solstone_core_backup::save_hosted_binding(
        journal.path(),
        &solstone_core_backup::HostedBinding {
            broker_endpoint: "https://backup.example".into(),
            account_id: "account".into(),
            instance_id: "instance".into(),
            bucket: "bucket".into(),
            prefix: "prefix".into(),
            broker_token: "token".into(),
        },
    )
    .unwrap();
    solstone_core_backup::set_mode(journal.path(), "operated").unwrap();
    let operated = prepare(journal.path(), &FixedClock).unwrap();
    let resolved_operated = resolve_tools_for_platform(
        &operated,
        &NoOpRunner,
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap();
    assert_eq!(resolved_operated.restic_path, restic_full);
    assert_eq!(resolved_operated.rclone_path, Some(rclone_full.clone()));

    let resolved_op = resolve_operational_tools_for_platform(
        &NoOpRunner,
        journal.path(),
        true,
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap();
    assert_eq!(resolved_op.restic_path, restic_full);
    assert_eq!(resolved_op.rclone_path, Some(rclone_full));

    // 6. Tamper check: modifying restic binary on disk triggers member-changed refusal
    fs::write(&restic_full, b"tampered-bytes-after-admission").unwrap();

    let reval_err = revalidate_installed_android_service_tools().unwrap_err();
    assert!(matches!(
        reval_err,
        AndroidServiceToolsRefusal::MemberChanged { .. }
    ));
    assert_eq!(reval_err.code(), "member-changed");

    let tampered_res = resolve_tools_for_platform(
        &capability,
        &NoOpRunner,
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap_err();
    assert_eq!(tampered_res.detail(), "member-changed");

    let lookup_err =
        resolve_admitted_android_service_tool(AndroidServiceToolRole::Restic).unwrap_err();
    assert_eq!(lookup_err.code(), "member-changed");
}
