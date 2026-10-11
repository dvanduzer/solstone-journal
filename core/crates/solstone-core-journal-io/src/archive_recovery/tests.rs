// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
use super::*;

fn binding() -> ArchiveRecoveryBinding {
    let json = serde_json::json!({"format_version":1,"origin":{
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
        "repository_id":"c".repeat(64)});
    serde_json::from_value(json).unwrap()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    binding: ArchiveRecoveryBinding,
}

fn rejects(bytes: &[u8]) {
    assert!(ArchiveRecoveryBinding::from_json(bytes).is_err());
    assert!(serde_json::from_slice::<ArchiveRecoveryBinding>(bytes).is_err());
    let mut nested = b"{\"binding\":".to_vec();
    nested.extend_from_slice(bytes);
    nested.push(b'}');
    assert!(serde_json::from_slice::<Record>(&nested).is_err());
}

#[test]
fn strict_binding_decode_uses_one_validated_nested_wire() {
    let original = binding();
    let bytes = serde_json::to_vec(&original).unwrap();
    assert_eq!(ArchiveRecoveryBinding::from_json(&bytes).unwrap(), original);
    let record: Record = serde_json::from_value(serde_json::json!({"binding":original})).unwrap();
    assert_eq!(record.binding, original);
    for (from, to) in [
        (
            "\"boot_id\":",
            "\"boot_id\":\"11111111-1111-1111-1111-111111111111\",\"boot_id\":",
        ),
        ("\"inode\":4", "\"inode\":4,\"inode\":4"),
        ("\"inode\":3", "\"inode\":3,\"inode\":3"),
        ("\"links\":1", "\"links\":1,\"unknown\":true"),
    ] {
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains(from));
        rejects(text.replacen(from, to, 1).as_bytes());
    }
    for cut in 0..13 {
        let mut value = serde_json::to_value(&original).unwrap();
        match cut {
            0 => value["origin"]["boot_id"] = "invalid".into(),
            1 => value["reservation_name"] = "../bad".into(),
            2 => value["reservation_identity"]["inode"] = 0.into(),
            3 => value["activity_lock"]["links"] = 2.into(),
            4 => value["activity_lock"]["modified_nanoseconds"] = (-1).into(),
            5 => value["activity_lock"]["changed_nanoseconds"] = 1_000_000_000.into(),
            6 => value["activity_lock_name"] = "other.lock".into(),
            7 => value["repository_id"] = "C".repeat(64).into(),
            8 => value["origin"]["unknown"] = true.into(),
            9 => {
                value["reservation_identity"] =
                    serde_json::json!({"platform":"windows","volume":1,"file_id":vec![1;16]})
            }
            10 => value["activity_lock"]["device"] = 0.into(),
            11 => value["activity_lock"]["mode"] = 0o100666.into(),
            _ => value["format_version"] = 2.into(),
        }
        rejects(&serde_json::to_vec(&value).unwrap());
    }
    assert!(matches!(
        ArchiveRecoveryBinding::from_json(&vec![b' '; MAX_BINDING_BYTES + 1]),
        Err(RecoveryBindingDecodeError::TooLarge)
    ));
}
