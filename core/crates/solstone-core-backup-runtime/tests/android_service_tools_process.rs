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
use solstone_core_installed_payload::sha256_hex;
use solstone_core_spp_attest::android_service_tools::{
    ANDROID_RCLONE_MEMBER, ANDROID_RESTIC_MEMBER, AndroidServiceToolFile, AndroidServiceToolRole,
    AndroidServiceToolsRefusal, admit_android_service_tools, get_admitted_android_service_tool,
    get_admitted_android_service_tools, install_admitted_android_service_tools,
    render_android_service_tools_manifest, revalidate_installed_android_service_tools,
};
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

#[test]
#[allow(unsafe_code)]
fn android_service_tools_process_lifecycle_and_env_isolation() {
    let dummy_exe = Path::new("/system/bin/app_process64");
    let journal = configured_journal();
    let capability = prepare(journal.path(), &FixedClock).unwrap();

    // 1. Pre-installation state:
    assert!(get_admitted_android_service_tools().is_none());
    assert!(get_admitted_android_service_tool(AndroidServiceToolRole::Restic).is_none());
    assert!(get_admitted_android_service_tool(AndroidServiceToolRole::Rclone).is_none());
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

    // 2. Prepare valid tools in a temporary directory
    let temp = tempdir().unwrap();
    let root = temp.path();

    let restic_rel = ANDROID_RESTIC_MEMBER;
    let rclone_rel = ANDROID_RCLONE_MEMBER;

    let restic_full = root.join(restic_rel);
    let rclone_full = root.join(rclone_rel);

    fs::create_dir_all(restic_full.parent().unwrap()).unwrap();
    fs::create_dir_all(rclone_full.parent().unwrap()).unwrap();

    let restic_bytes = b"\x7fELFfake-android-restic-pie";
    let rclone_bytes = b"\x7fELFfake-android-rclone-pie";

    fs::write(&restic_full, restic_bytes).unwrap();
    fs::write(&rclone_full, rclone_bytes).unwrap();

    let files = vec![
        AndroidServiceToolFile {
            path: rclone_rel.to_owned(),
            bytes: rclone_bytes.len() as u64,
            sha256: sha256_hex(rclone_bytes),
            role: Some(AndroidServiceToolRole::Rclone),
            version: Some("1.73.1".to_owned()),
        },
        AndroidServiceToolFile {
            path: restic_rel.to_owned(),
            bytes: restic_bytes.len() as u64,
            sha256: sha256_hex(restic_bytes),
            role: Some(AndroidServiceToolRole::Restic),
            version: Some(solstone_core_backup::SUPPORTED_RESTIC_VERSION.to_owned()),
        },
    ];

    let manifest_bytes = render_android_service_tools_manifest(&files).unwrap();
    let admitted = admit_android_service_tools(root, &manifest_bytes).unwrap();

    // 3. Install into OnceLock
    install_admitted_android_service_tools(admitted.clone()).unwrap();

    // Attempt second install: must fail
    let second_err = install_admitted_android_service_tools(admitted).unwrap_err();
    assert!(matches!(
        second_err,
        AndroidServiceToolsRefusal::ManifestInvalid { .. }
    ));

    // 4. Post-installation queries:
    assert!(get_admitted_android_service_tools().is_some());
    assert_eq!(
        get_admitted_android_service_tool(AndroidServiceToolRole::Restic).unwrap(),
        restic_full
    );
    assert_eq!(
        get_admitted_android_service_tool(AndroidServiceToolRole::Rclone).unwrap(),
        rclone_full
    );
    assert!(revalidate_installed_android_service_tools().is_ok());

    // 5. Environmental isolation: modifying environment must not alter resolution
    unsafe {
        std::env::set_var("PATH", "/injected/bin:/usr/bin");
        std::env::set_var("LD_LIBRARY_PATH", "/injected/lib");
        std::env::set_var("SOLSTONE_RESTIC_BUNDLE", "/injected/bundle");
        std::env::set_var("SOLSTONE_RCLONE_BUNDLE", "/injected/bundle");
    }

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

    // Operated mode needs rclone:
    solstone_core_backup::set_mode(journal.path(), "operated").unwrap();
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
    assert_eq!(
        reval_err,
        AndroidServiceToolsRefusal::MemberChanged {
            path: restic_rel.to_owned(),
        }
    );

    let tampered_res = resolve_tools_for_platform(
        &capability,
        &NoOpRunner,
        dummy_exe,
        ToolPlatform::AndroidAarch64,
        &LiveAndroidAuthority,
    )
    .unwrap_err();
    assert_eq!(tampered_res.detail(), "member-changed");
}
