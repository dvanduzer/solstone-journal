// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Descriptive DATA only; no qualified native owner is constructed here.
use super::*;
use solstone_core_journal_io::archive_recovery::ArchiveRecoveryBinding;

fn data() -> ArchiveRecoveryBinding {
    serde_json::from_value(serde_json::json!({"format_version":1,"origin":{
        "format_version":1,"boot_id":"11111111-1111-1111-1111-111111111111",
        "machine_app_id_sha256":"a".repeat(64),
        "root_identity":"native-root-v1:unix:0000000000010306:0000000000000002",
        "canonical_chain":[
            {"name":[],"identity":"native-root-v1:unix:0000000000010306:0000000000000001"},
            {"name":[106,111,117,114,110,97,108],"identity":"native-root-v1:unix:0000000000010306:0000000000000002"}],
        "filesystem":"ext4","storage_topology":"direct-pcie-nvme",
        "filesystem_device":{"major":259,"minor":6},"namespace_device":{"major":259,"minor":0},"partition":1},
        "reservation_name":format!(".tmp-offload-{}","b".repeat(32)),
        "reservation_identity":{"platform":"unix","device":66310,"inode":3},
        "activity_lock_name":"backup-activity.lock",
        "activity_lock":{"device":66310,"inode":4,"size":0,"modified_seconds":5,
            "modified_nanoseconds":6,"changed_seconds":7,"changed_nanoseconds":8,"mode":33152,"links":1},
        "repository_id":"c".repeat(64)})).unwrap()
}

#[test]
fn absent_data_preserves_old_bytes_and_bound_data_roundtrips_strictly() {
    let old = tests::fixture(true);
    let bytes = old.encode().unwrap();
    assert!(
        !String::from_utf8(bytes.clone())
            .unwrap()
            .contains("recovery_binding")
    );
    assert_eq!(
        RestoreIntent::decode(Ok(&bytes)).unwrap().encode().unwrap(),
        bytes
    );
    let mut bound = old;
    bound.recovery_binding = Some(data());
    bound.validate().unwrap();
    assert_eq!(
        RestoreIntent::decode(Ok(&bound.encode().unwrap())).unwrap(),
        bound
    );
    for change in ["unknown", "repository_id", "origin"] {
        let mut value = serde_json::to_value(&bound).unwrap();
        match change {
            "unknown" => value["recovery_binding"]["unknown"] = true.into(),
            "repository_id" => value["recovery_binding"]["repository_id"] = "d".repeat(64).into(),
            _ => value["recovery_binding"]["origin"]["boot_id"] = "invalid".into(),
        }
        assert!(RestoreIntent::decode(Ok(&serde_json::to_vec(&value).unwrap())).is_err());
    }
}

#[test]
fn existing_record_cannot_add_remove_or_substitute_recovery_data() {
    let old = tests::fixture(false);
    let mut bound = old.clone();
    bound.recovery_binding = Some(data());
    assert_eq!(
        old.validate_successor(&bound),
        Err(RepairHold::ImmutableEvent)
    );
    assert_eq!(
        bound.validate_successor(&old),
        Err(RepairHold::ImmutableEvent)
    );
    let mut value = serde_json::to_value(bound.recovery_binding.as_ref().unwrap()).unwrap();
    value["reservation_name"] = format!(".tmp-offload-{}", "d".repeat(32)).into();
    let mut changed = bound.clone();
    changed.recovery_binding = Some(serde_json::from_value(value).unwrap());
    changed.validate().unwrap();
    assert_eq!(
        bound.validate_successor(&changed),
        Err(RepairHold::ImmutableEvent)
    );
    assert!(bound.validate_successor(&bound).is_ok());
}
