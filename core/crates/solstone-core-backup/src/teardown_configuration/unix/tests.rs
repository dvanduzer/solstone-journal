// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;

use serde_json::json;
use solstone_core_journal_io::media::{MediaPrimitive, run_with_media_hook};

use super::*;

fn value(mode: &str) -> Value {
    json!({
        "backup": {
            "enabled": true, "mode": mode,
            "daily_key": "daily-secret", "recovery_key": "A".repeat(crate::RECOVERY_KEY_LENGTH),
            "destination": {"repository":"s3:bucket/repo", "backend":"s3",
                "credentials":{"access_key_id":"access-secret","secret_access_key":"storage-secret"}}
        },
        "providers":{"credential":"provider-secret"}
    })
}

fn write_config(path: &Path, value: &Value) -> Vec<u8> {
    fs::create_dir_all(path.join("config")).unwrap();
    let bytes = serde_json::to_vec(value).unwrap();
    fs::write(path.join("config/journal.json"), &bytes).unwrap();
    bytes
}

fn binding_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "broker_endpoint":"https://example.invalid", "account_id":"account",
        "instance_id":"instance", "bucket":"bucket", "prefix":"prefix/",
        "broker_token":"broker-secret", "unknown":"retained"
    }))
    .unwrap()
}

fn fixture(mode: &str) -> (tempfile::TempDir, Arc<JournalRoot>) {
    let temp = tempfile::tempdir().unwrap();
    write_config(temp.path(), &value(mode));
    if mode == "operated" {
        fs::create_dir_all(temp.path().join("backup/hosted")).unwrap();
        fs::write(
            temp.path().join("backup/hosted/binding.json"),
            binding_bytes(),
        )
        .unwrap();
    }
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    (temp, root)
}

#[test]
fn capture_preserves_existing_getter_semantics_and_original_sha() {
    for mode in ["byo", "operated"] {
        let (temp, root) = fixture(mode);
        let captured = TeardownConfiguration::capture(&root).unwrap();
        assert_eq!(
            captured.backup(),
            &crate::get_backup_config(temp.path()).unwrap()
        );
        assert_eq!(
            captured.keys(),
            crate::get_keys(temp.path()).unwrap().as_ref()
        );
        assert_eq!(
            captured.destination(),
            crate::get_destination(temp.path()).unwrap().as_ref()
        );
        assert_eq!(
            captured.binding(),
            crate::load_hosted_binding(temp.path()).as_ref()
        );
        assert_eq!(
            Some(captured.sha256()),
            solstone_core_journal_config::read_journal_config(temp.path())
                .unwrap()
                .sha256
                .as_deref()
        );
        captured.revalidate().unwrap();
        assert!(!temp.path().join("backup-activity.lock").exists());
        if mode == "byo" {
            assert!(!temp.path().join("backup").exists());
        }
    }
    // Existing getters keep missing-key and optional-destination interpretation.
    let (temp, root) = fixture("byo");
    let mut raw = value("byo");
    raw["backup"]["recovery_key"] = Value::Null;
    raw["backup"]["destination"]["credentials"] = Value::Null;
    write_config(temp.path(), &raw);
    let captured = TeardownConfiguration::capture(&root).unwrap();
    assert_eq!(captured.keys(), None);
    assert_eq!(
        captured.destination(),
        crate::get_destination(temp.path()).unwrap().as_ref()
    );
}

#[test]
fn config_failures_preserve_missing_and_invalid_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
    assert!(matches!(
        TeardownConfiguration::capture(&root),
        Err(Refusal::ConfigurationUnavailable)
    ));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    fs::create_dir(temp.path().join("config")).unwrap();
    assert!(matches!(
        TeardownConfiguration::capture(&root),
        Err(Refusal::ConfigurationUnavailable)
    ));
    assert_eq!(fs::read_dir(temp.path().join("config")).unwrap().count(), 0);
    for bytes in [
        b"{".to_vec(),
        b"[]".to_vec(),
        vec![b' '; MAX_BOUND_CONFIG_BYTES + 1],
    ] {
        fs::write(temp.path().join("config/journal.json"), &bytes).unwrap();
        assert!(matches!(
            TeardownConfiguration::capture(&root),
            Err(Refusal::ConfigurationUnavailable)
        ));
        assert_eq!(
            fs::read(temp.path().join("config/journal.json")).unwrap(),
            bytes
        );
        assert!(!temp.path().join("backup").exists());
    }
    let mut invalid = value("byo");
    invalid["backup"]["recovery_key"] = json!("invalid-secret");
    let bytes = write_config(temp.path(), &invalid);
    let refusal = TeardownConfiguration::capture(&root).err().unwrap();
    assert_eq!(refusal, Refusal::KeysUnavailable);
    assert!(!format!("{refusal:?}").contains("secret"));
    assert_eq!(
        fs::read(temp.path().join("config/journal.json")).unwrap(),
        bytes
    );
}

#[test]
fn operated_binding_requires_complete_existing_plain_valid_bytes() {
    let (temp, root) = fixture("byo");
    write_config(temp.path(), &value("operated"));
    assert!(matches!(
        TeardownConfiguration::capture(&root),
        Err(Refusal::BindingUnavailable)
    ));
    assert!(!temp.path().join("backup").exists());
    fs::create_dir_all(temp.path().join("backup/hosted")).unwrap();
    let path = temp.path().join("backup/hosted/binding.json");
    for bytes in [b"{".to_vec(), b"[]".to_vec(), b"{}".to_vec()] {
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            TeardownConfiguration::capture(&root),
            Err(Refusal::BindingUnavailable)
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let target = temp.path().join("binding-target.json");
    fs::write(&target, binding_bytes()).unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(matches!(
        TeardownConfiguration::capture(&root),
        Err(Refusal::BindingUnavailable)
    ));
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), binding_bytes());
}

#[test]
fn binding_byte_limit_has_exact_acceptance_and_refusal_controls() {
    let (temp, root) = fixture("operated");
    let path = temp.path().join("backup/hosted/binding.json");
    let mut bytes = binding_bytes();
    bytes.resize(MAX_BINDING_BYTES, b' ');
    fs::write(&path, &bytes).unwrap();
    TeardownConfiguration::capture(&root)
        .unwrap()
        .revalidate()
        .unwrap();
    bytes.push(b' ');
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        TeardownConfiguration::capture(&root),
        Err(Refusal::BindingUnavailable)
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn binding_byte_and_identical_object_replacements_refuse() {
    for replacement in [
        binding_bytes(),
        {
            let mut raw: Value = serde_json::from_slice(&binding_bytes()).unwrap();
            raw["unknown"] = json!("changed");
            serde_json::to_vec(&raw).unwrap()
        },
        {
            let mut raw: Value = serde_json::from_slice(&binding_bytes()).unwrap();
            raw["broker_token"] = json!("new-secret");
            serde_json::to_vec(&raw).unwrap()
        },
    ] {
        let (temp, root) = fixture("operated");
        let captured = TeardownConfiguration::capture(&root).unwrap();
        let path = temp.path().join("backup/hosted/binding.json");
        fs::rename(&path, path.with_extension("old")).unwrap();
        fs::write(&path, &replacement).unwrap();
        assert_eq!(captured.revalidate(), Err(Refusal::BindingChanged));
        assert_eq!(fs::read(&path).unwrap(), replacement);
    }
}

#[test]
fn unrelated_config_drift_refuses_but_identical_file_bytes_remain_compatible() {
    let (temp, root) = fixture("byo");
    let captured = TeardownConfiguration::capture(&root).unwrap();
    let path = temp.path().join("config/journal.json");
    let bytes = fs::read(&path).unwrap();
    fs::rename(&path, path.with_extension("old")).unwrap();
    fs::write(&path, &bytes).unwrap();
    captured.revalidate().unwrap();
    let mut changed = value("byo");
    changed["providers"]["credential"] = json!("changed-secret");
    let changed = write_config(temp.path(), &changed);
    assert_eq!(
        captured.backup(),
        &crate::get_backup_config(temp.path()).unwrap()
    );
    assert_eq!(captured.revalidate(), Err(Refusal::ConfigurationChanged));
    assert_eq!(fs::read(&path).unwrap(), changed);
}

#[test]
fn identical_config_parent_and_root_replacements_are_not_reacquired() {
    let (temp, root) = fixture("byo");
    let captured = TeardownConfiguration::capture(&root).unwrap();
    fs::rename(temp.path().join("config"), temp.path().join("old-config")).unwrap();
    let bytes = write_config(temp.path(), &value("byo"));
    assert_eq!(captured.revalidate(), Err(Refusal::ConfigurationChanged));
    assert_eq!(
        fs::read(temp.path().join("config/journal.json")).unwrap(),
        bytes
    );

    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("journal");
    let replacement = parent.path().join("replacement");
    write_config(&path, &value("byo"));
    let bytes = write_config(&replacement, &value("byo"));
    let root = Arc::new(JournalRoot::open(&path).unwrap());
    let captured = TeardownConfiguration::capture(&root).unwrap();
    fs::rename(&path, parent.path().join("original")).unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert_eq!(captured.revalidate(), Err(Refusal::RootChanged));
    assert_eq!(fs::read(path.join("config/journal.json")).unwrap(), bytes);
}

#[test]
fn config_change_between_initial_read_and_binding_capture_refuses() {
    let (temp, root) = fixture("operated");
    let directory = MediaDirectory::open(root.clone(), Path::new("config")).unwrap();
    capture_in_directory(root.clone(), directory.clone()).unwrap();
    let path = temp.path().join("config/journal.json");
    let mut changed = value("operated");
    changed["providers"]["credential"] = json!("new-secret");
    let bytes = serde_json::to_vec(&changed).unwrap();
    let changed_bytes = bytes.clone();
    let changed_path = path.clone();
    let (result, fired) = run_with_media_hook(
        MediaPrimitive::AfterChildOpen,
        move || fs::write(&changed_path, &changed_bytes),
        || capture_in_directory(root, directory),
    );
    assert!(fired, "binding traversal must reach the exact Native cut");
    assert!(matches!(result, Err(Refusal::ConfigurationChanged)));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
