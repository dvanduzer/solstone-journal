// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::HashMap;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono_tz::Tz;
use sha2::{Digest, Sha256};
use solstone_core_format::audio_archive::AUDIO_ARCHIVE_PLACEMENT_FILENAME;
use tempfile::TempDir;

use super::*;
use crate::metadata::{admit_running_attempt, record_completed_attempt};

const NY: Tz = Tz::America__New_York;
const UTC_ZONE: Tz = Tz::UTC;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn default_settings() -> AudioArchiveSettings {
    AudioArchiveSettings {
        qualifier: "clips".to_owned(),
        archive_identity: "archive_test_run_1".to_owned(),
    }
}

fn dummy_hash(idx: u8) -> String {
    format!("{:02x}", idx).repeat(32)
}

#[test]
fn pure_plan_bad_durations_and_unsettled_times() {
    let inputs = vec![
        AudioArchiveMemberInput {
            original_relative_name: "archive/empty.ogg".to_owned(),
            byte_len: 100,
            sha256: dummy_hash(1),
            duration: DurationInput::Empty,
            start: MemberStartEvidence {
                instant: Some("2026-01-15T10:00:00.500Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        },
        AudioArchiveMemberInput {
            original_relative_name: "archive/nonfinite.ogg".to_owned(),
            byte_len: 100,
            sha256: dummy_hash(2),
            duration: DurationInput::NonFinite,
            start: MemberStartEvidence {
                instant: Some("2026-01-15T10:00:00.500Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        },
        AudioArchiveMemberInput {
            original_relative_name: "archive/zero.ogg".to_owned(),
            byte_len: 100,
            sha256: dummy_hash(3),
            duration: DurationInput::Milliseconds(0),
            start: MemberStartEvidence {
                instant: Some("2026-01-15T10:00:00.500Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        },
        AudioArchiveMemberInput {
            original_relative_name: "archive/negative.ogg".to_owned(),
            byte_len: 100,
            sha256: dummy_hash(4),
            duration: DurationInput::Milliseconds(-500),
            start: MemberStartEvidence {
                instant: Some("2026-01-15T10:00:00.500Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        },
        AudioArchiveMemberInput {
            original_relative_name: "archive/no_evidence.ogg".to_owned(),
            byte_len: 100,
            sha256: dummy_hash(5),
            duration: DurationInput::Milliseconds(1000),
            start: MemberStartEvidence {
                instant: None,
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        },
        AudioArchiveMemberInput {
            original_relative_name: "archive/no_offset.ogg".to_owned(),
            byte_len: 100,
            sha256: dummy_hash(6),
            duration: DurationInput::Milliseconds(1000),
            start: MemberStartEvidence {
                instant: Some("2026-01-15T10:00:00.500".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        },
    ];

    let plan = plan_audio_archive(&inputs, NY, default_settings()).unwrap();
    assert_eq!(plan.members.len(), 6);
    assert_eq!(plan.tiles.len(), 0);
    assert_eq!(plan.size.bytes, 0);
    assert_eq!(plan.size.members, 6);
    assert_eq!(plan.size.tiles, 0);

    assert_eq!(
        plan.members[0].disposition,
        MemberDisposition::LeftOut(LeftOutReason::BadDuration)
    );
    assert_eq!(
        plan.members[1].disposition,
        MemberDisposition::LeftOut(LeftOutReason::BadDuration)
    );
    assert_eq!(
        plan.members[2].disposition,
        MemberDisposition::LeftOut(LeftOutReason::BadDuration)
    );
    assert_eq!(
        plan.members[3].disposition,
        MemberDisposition::LeftOut(LeftOutReason::BadDuration)
    );
    assert_eq!(
        plan.members[4].disposition,
        MemberDisposition::LeftOut(LeftOutReason::UnsettledTime)
    );
    assert_eq!(
        plan.members[5].disposition,
        MemberDisposition::LeftOut(LeftOutReason::UnsettledTime)
    );
}

#[test]
fn subsecond_and_short_durations() {
    // Under 1s duration (500 ms) and 7_500 ms
    let inputs = vec![AudioArchiveMemberInput {
        original_relative_name: "archive/half_second.ogg".to_owned(),
        byte_len: 200,
        sha256: dummy_hash(10),
        duration: DurationInput::Milliseconds(500),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.500Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    }];

    let plan = plan_audio_archive(&inputs, NY, default_settings()).unwrap();
    assert_eq!(plan.tiles.len(), 1);
    assert_eq!(plan.tiles[0].parts.len(), 1);
    let p = &plan.tiles[0].parts[0];
    assert_eq!(p.length_ms, 500);
    assert_eq!(p.offset_ms, 500);
    assert_eq!(p.origin_ms, 0);
}

#[test]
fn worked_fixtures_hardcoded_numbers() {
    // Short Auckland member:
    // Auckland 2026-01-15T10:00:00.500, duration 7_500 ms.
    // NZDT = UTC+13 -> UTC = 2026-01-14T21:00:00.500Z.
    // Epoch arithmetic: 2026-01-14 21:00:00 UTC is 1768424400s -> 1_768_424_400_500 ms.
    // America/New_York is UTC-5 -> 2026-01-14 16:00:00.500.
    // Day 20260114, key 160000_300, offset 500, origin 0, length 7500.
    let short_member = AudioArchiveMemberInput {
        original_relative_name: "archive/short.ogg".to_owned(),
        byte_len: 1024,
        sha256: dummy_hash(20),
        duration: DurationInput::Milliseconds(7_500),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-01-15T10:00:00.500".to_owned()),
            source_zone: Some("Pacific/Auckland".to_owned()),
        },
        run_note: None,
    };

    // Long Auckland member:
    // Auckland 2026-01-15T18:07:30.250, duration 400_000 ms.
    // UTC = 2026-01-15T05:07:30.250Z.
    // America/New_York (UTC-5) = 2026-01-15 00:07:30.250.
    // Part 0: Day 20260115, key 000500_300, offset 150250, origin 0, length 149750.
    // Part 1: Day 20260115, key 001000_300, offset 0, origin 149750, length 250250.
    let long_member = AudioArchiveMemberInput {
        original_relative_name: "archive/long.ogg".to_owned(),
        byte_len: 2048,
        sha256: dummy_hash(21),
        duration: DurationInput::Milliseconds(400_000),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-01-15T18:07:30.250".to_owned()),
            source_zone: Some("Pacific/Auckland".to_owned()),
        },
        run_note: None,
    };

    let plan = plan_audio_archive(&[short_member, long_member], NY, default_settings()).unwrap();
    assert_eq!(plan.tiles.len(), 3);

    // Tile 0: Short member
    assert_eq!(plan.tiles[0].day, "20260114");
    assert_eq!(plan.tiles[0].segment_key, "160000_300");
    assert_eq!(plan.tiles[0].parts.len(), 1);
    assert_eq!(plan.tiles[0].parts[0].offset_ms, 500);
    assert_eq!(plan.tiles[0].parts[0].origin_ms, 0);
    assert_eq!(plan.tiles[0].parts[0].length_ms, 7500);

    // Tile 1: Long member part 0
    assert_eq!(plan.tiles[1].day, "20260115");
    assert_eq!(plan.tiles[1].segment_key, "000500_300");
    assert_eq!(plan.tiles[1].parts.len(), 1);
    assert_eq!(plan.tiles[1].parts[0].offset_ms, 150250);
    assert_eq!(plan.tiles[1].parts[0].origin_ms, 0);
    assert_eq!(plan.tiles[1].parts[0].length_ms, 149750);

    // Tile 2: Long member part 1
    assert_eq!(plan.tiles[2].day, "20260115");
    assert_eq!(plan.tiles[2].segment_key, "001000_300");
    assert_eq!(plan.tiles[2].parts.len(), 1);
    assert_eq!(plan.tiles[2].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[2].parts[0].origin_ms, 149750);
    assert_eq!(plan.tiles[2].parts[0].length_ms, 250250);
}

#[test]
fn settled_fold_and_unsettled_folds() {
    // Settled Fold:
    // America/New_York 2026-11-01T01:30:00.000 local, duration 10_000 ms, owner America/New_York.
    // Key 013000_300, offset 0, origin 0, length 10000.
    // Canonical occurrence: local|America/New_York|2026-11-01T01:30:00.000
    let fold_member = AudioArchiveMemberInput {
        original_relative_name: "archive/fold.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(30),
        duration: DurationInput::Milliseconds(10_000),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-11-01T01:30:00.000".to_owned()),
            source_zone: Some("America/New_York".to_owned()),
        },
        run_note: None,
    };

    let plan_ny =
        plan_audio_archive(std::slice::from_ref(&fold_member), NY, default_settings()).unwrap();
    assert_eq!(plan_ny.members[0].disposition, MemberDisposition::Included);
    assert_eq!(
        plan_ny.members[0].identity.occurrence,
        "local|America/New_York|2026-11-01T01:30:00.000"
    );
    assert_eq!(plan_ny.tiles.len(), 1);
    assert_eq!(plan_ny.tiles[0].day, "20261101");
    assert_eq!(plan_ny.tiles[0].segment_key, "013000_300");
    assert_eq!(plan_ny.tiles[0].parts[0].offset_ms, 0);
    assert_eq!(plan_ny.tiles[0].parts[0].origin_ms, 0);
    assert_eq!(plan_ny.tiles[0].parts[0].length_ms, 10000);

    // Same local start, owner UTC -> UnsettledTime
    let plan_utc = plan_audio_archive(
        std::slice::from_ref(&fold_member),
        UTC_ZONE,
        default_settings(),
    )
    .unwrap();
    assert_eq!(
        plan_utc.members[0].disposition,
        MemberDisposition::LeftOut(LeftOutReason::UnsettledTime)
    );

    // 90-minute fold with America/New_York owner -> UnsettledTime
    let long_fold = AudioArchiveMemberInput {
        original_relative_name: "archive/long_fold.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(31),
        duration: DurationInput::Milliseconds(5_400_000), // 90 min
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-11-01T01:30:00.000".to_owned()),
            source_zone: Some("America/New_York".to_owned()),
        },
        run_note: None,
    };
    let plan_long_fold = plan_audio_archive(&[long_fold], NY, default_settings()).unwrap();
    assert_eq!(
        plan_long_fold.members[0].disposition,
        MemberDisposition::LeftOut(LeftOutReason::UnsettledTime)
    );

    // Spring forward gap: America/New_York 2026-03-08T02:30:00.000 -> NonexistentLocalTime
    let gap_member = AudioArchiveMemberInput {
        original_relative_name: "archive/gap.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(32),
        duration: DurationInput::Milliseconds(10_000),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-03-08T02:30:00.000".to_owned()),
            source_zone: Some("America/New_York".to_owned()),
        },
        run_note: None,
    };
    let plan_gap = plan_audio_archive(&[gap_member], NY, default_settings()).unwrap();
    assert_eq!(
        plan_gap.members[0].disposition,
        MemberDisposition::LeftOut(LeftOutReason::NonexistentLocalTime)
    );
}

#[test]
fn agreeing_and_conflicting_evidence() {
    // Agreeing: instant 2026-01-15T05:07:30.250Z + Auckland 2026-01-15T18:07:30.250
    let agreeing = AudioArchiveMemberInput {
        original_relative_name: "archive/agree.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(40),
        duration: DurationInput::Milliseconds(1000),
        start: MemberStartEvidence {
            instant: Some("2026-01-15T05:07:30.250Z".to_owned()),
            local_civil: Some("2026-01-15T18:07:30.250".to_owned()),
            source_zone: Some("Pacific/Auckland".to_owned()),
        },
        run_note: None,
    };
    let plan_agree = plan_audio_archive(&[agreeing], NY, default_settings()).unwrap();
    assert_eq!(
        plan_agree.members[0].disposition,
        MemberDisposition::Included
    );

    // Conflicting: instant 2026-01-15T05:07:30.250Z + America/New_York 2026-01-15T18:07:30.250
    let conflicting = AudioArchiveMemberInput {
        original_relative_name: "archive/conflict.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(41),
        duration: DurationInput::Milliseconds(1000),
        start: MemberStartEvidence {
            instant: Some("2026-01-15T05:07:30.250Z".to_owned()),
            local_civil: Some("2026-01-15T18:07:30.250".to_owned()),
            source_zone: Some("America/New_York".to_owned()),
        },
        run_note: None,
    };
    let plan_conflict = plan_audio_archive(&[conflicting], NY, default_settings()).unwrap();
    assert_eq!(
        plan_conflict.members[0].disposition,
        MemberDisposition::LeftOut(LeftOutReason::ConflictingEvidence)
    );
}

#[test]
fn shared_tile_and_same_start_conflict() {
    // Shared tile: two members in 160000_300 at offsets 0 and 20_000, each 10_000 ms
    let m1 = AudioArchiveMemberInput {
        original_relative_name: "archive/m1.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(50),
        duration: DurationInput::Milliseconds(10_000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let m2 = AudioArchiveMemberInput {
        original_relative_name: "archive/m2.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(51),
        duration: DurationInput::Milliseconds(10_000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:20.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };

    let plan_shared = plan_audio_archive(&[m1, m2], NY, default_settings()).unwrap();
    assert_eq!(plan_shared.tiles.len(), 1);
    assert_eq!(plan_shared.tiles[0].parts.len(), 2);
    assert_eq!(plan_shared.tiles[0].parts[0].offset_ms, 0);
    assert_eq!(plan_shared.tiles[0].parts[1].offset_ms, 20_000);

    // Same-start different hashes: later is IntervalConflict at key 160000_300
    let c1 = AudioArchiveMemberInput {
        original_relative_name: "archive/c1.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(60),
        duration: DurationInput::Milliseconds(10_000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let c2 = AudioArchiveMemberInput {
        original_relative_name: "archive/c2.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(61), // different hash
        duration: DurationInput::Milliseconds(10_000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()), // same start
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };

    let plan_conflict = plan_audio_archive(&[c1, c2], NY, default_settings()).unwrap();
    assert_eq!(
        plan_conflict.members[0].disposition,
        MemberDisposition::Included
    );
    match &plan_conflict.members[1].disposition {
        MemberDisposition::LeftOut(LeftOutReason::IntervalConflict(payload)) => {
            assert_eq!(payload.day, "20260114");
            assert_eq!(payload.segment_key, "160000_300");
            assert_eq!(payload.conflicting_sha256, dummy_hash(60));
        }
        other => panic!("expected IntervalConflict, got {other:?}"),
    }
}

#[test]
fn deduplication_and_naming_rules() {
    // Rename dedupes: same bytes + canonical occurrence, different relative names
    let d1 = AudioArchiveMemberInput {
        original_relative_name: "dir1/clip.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(70),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let d2 = AudioArchiveMemberInput {
        original_relative_name: "dir2/renamed_clip.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(70),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000+00:00".to_owned()), // Z and +00:00 dedupe
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };

    let plan = plan_audio_archive(&[d1, d2], NY, default_settings()).unwrap();
    assert_eq!(plan.members.len(), 1); // Deduped to 1 member
    assert_eq!(plan.members[0].original_relative_name, "dir1/clip.ogg"); // Retains first name

    // Equal bytes at different instants stay distinct
    let t1 = AudioArchiveMemberInput {
        original_relative_name: "dir1/clip.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(71),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let t2 = AudioArchiveMemberInput {
        original_relative_name: "dir2/clip.ogg".to_owned(),
        byte_len: 500,
        sha256: dummy_hash(71),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:05:00.000Z".to_owned()), // 5 min later
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let plan_distinct = plan_audio_archive(&[t1, t2], NY, default_settings()).unwrap();
    assert_eq!(plan_distinct.members.len(), 2);
    assert_eq!(plan_distinct.tiles.len(), 2);
}

#[test]
fn claim_index_semantics_and_missing_run() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    // find_held_members does not create database
    let plan = plan_audio_archive(
        &[AudioArchiveMemberInput {
            original_relative_name: "clip.ogg".to_owned(),
            byte_len: 500,
            sha256: dummy_hash(80),
            duration: DurationInput::Milliseconds(5000),
            start: MemberStartEvidence {
                instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        }],
        NY,
        default_settings(),
    )
    .unwrap();

    let held = find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT).unwrap();
    assert!(held.is_empty());
    assert!(!claim_db_path(journal).exists());

    // Plant garbage at claim path -> corrupt error
    fs::create_dir_all(journal.join("imports")).unwrap();
    fs::write(claim_db_path(journal), b"not a sqlite db").unwrap();
    let err =
        find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT).unwrap_err();
    assert!(matches!(err, AudioArchiveError::ClaimDatabaseCorrupt(_)));
    fs::remove_file(claim_db_path(journal)).unwrap();

    // Insert claims under run1
    let id1 = plan.members[0].identity.clone();
    insert_tile_claims(
        journal,
        "20260114_100001",
        std::slice::from_ref(&id1),
        DEFAULT_WRITE_BUSY_TIMEOUT,
    )
    .unwrap();

    // With run directory present, run2 sees it held
    let run1_dir = journal.join("imports").join("20260114_100001");
    fs::create_dir_all(&run1_dir).unwrap();
    let held = find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT).unwrap();
    assert!(held.contains(&id1));

    // Remove run1 directory -> claim does not block and no tombstone is written
    fs::remove_dir_all(&run1_dir).unwrap();
    let held_after_remove =
        find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT).unwrap();
    assert!(held_after_remove.is_empty());
    assert!(!run1_dir.exists());

    // Insert under run2 updates claim row to run2 without error
    insert_tile_claims(
        journal,
        "20260114_100002",
        std::slice::from_ref(&id1),
        DEFAULT_WRITE_BUSY_TIMEOUT,
    )
    .unwrap();
}

#[test]
fn publication_and_fault_seams() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let member_bytes_data = b"synthetic ogg audio bytes".to_vec();
    let byte_len = member_bytes_data.len() as u64;
    let sha256 = format!("{:x}", Sha256::digest(&member_bytes_data));

    let member = AudioArchiveMemberInput {
        original_relative_name: "archive/clip.ogg".to_owned(),
        byte_len,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(7500),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.500Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };

    let mut bytes_map = HashMap::new();
    bytes_map.insert(sha256.clone(), member_bytes_data.clone());

    // Seam 1: AfterRecordFirstSaved
    let seams_fault1 = AudioArchivePublishSeams {
        fault: Some(ArchiveFault::AfterRecordFirstSaved),
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };

    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: bytes_map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_fault1),
    );
    assert!(matches!(
        res,
        Err(AudioArchiveError::Fault(
            ArchiveFault::AfterRecordFirstSaved
        ))
    ));

    // Verify recovery on clean publish
    let seams_clean = AudioArchivePublishSeams {
        fault: None,
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };
    let clean_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: bytes_map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_clean),
    )
    .unwrap();

    assert_eq!(clean_res.tiles_saved, 1);
    assert_eq!(clean_res.parts_saved, 1);

    // Frontier test on published run
    let frontier = load_frontier(journal, &clean_res.import_id).unwrap();
    assert_eq!(frontier.published_tiles.len(), 1);
    assert_eq!(frontier.pending_tiles.len(), 0);
    assert_eq!(frontier.unavailable_parts.len(), 0);

    // Remove chronicle raw -> frontier reports SavedButUnavailable
    let raw_path = journal
        .join("chronicle")
        .join("20260114")
        .join("import.clips")
        .join("160000_300")
        .join(&clean_res.record.parts[0].stored_name);
    fs::remove_file(&raw_path).unwrap();

    let frontier_after_removal = load_frontier(journal, &clean_res.import_id).unwrap();
    assert_eq!(frontier_after_removal.unavailable_parts.len(), 1);
    assert_eq!(frontier_after_removal.published_tiles.len(), 1);
    assert_eq!(
        frontier_after_removal.published_tiles[0].parts[0].outcome,
        PartOutcome::SavedButUnavailable
    );
}

#[test]
fn negative_offset_instant_dedupes_with_z_and_auckland_original_time() {
    let m1 = AudioArchiveMemberInput {
        original_relative_name: "a/one.ogg".to_owned(),
        byte_len: 1024,
        sha256: dummy_hash(20),
        duration: DurationInput::Milliseconds(7_500),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T16:00:00.500-05:00".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let m2 = AudioArchiveMemberInput {
        original_relative_name: "b/two.ogg".to_owned(),
        byte_len: 1024,
        sha256: dummy_hash(20),
        duration: DurationInput::Milliseconds(7_500),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.500Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let short_auckland = AudioArchiveMemberInput {
        original_relative_name: "archive/short.ogg".to_owned(),
        byte_len: 1024,
        sha256: dummy_hash(21),
        duration: DurationInput::Milliseconds(7_500),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-01-15T10:00:00.500".to_owned()),
            source_zone: Some("Pacific/Auckland".to_owned()),
        },
        run_note: None,
    };

    let plan_dedupe = plan_audio_archive(&[m1, m2], NY, default_settings()).unwrap();
    assert_eq!(plan_dedupe.members.len(), 1);
    assert_eq!(
        plan_dedupe.members[0].identity.occurrence,
        "utc|1768424400500"
    );
    assert_eq!(plan_dedupe.members[0].original_relative_name, "a/one.ogg");
    assert_eq!(
        plan_dedupe.members[0].disposition,
        MemberDisposition::Included
    );

    let plan_auckland = plan_audio_archive(&[short_auckland], NY, default_settings()).unwrap();
    assert_eq!(
        plan_auckland.members[0].identity.occurrence,
        "utc|1768424400500"
    );
    assert_eq!(
        plan_auckland.tiles[0].parts[0].original_time_ms,
        Some(1_768_424_400_500)
    );
}

#[test]
fn qualifier_audio_refused() {
    let settings = AudioArchiveSettings {
        qualifier: "audio".to_owned(),
        archive_identity: "test_archive".to_owned(),
    };
    let res = plan_audio_archive(&[], NY, settings);
    assert!(matches!(res, Err(AudioArchiveError::InvalidStreamName(_))));
}

#[test]
fn distinct_stored_names_and_long_member_second_part() {
    let m1 = AudioArchiveMemberInput {
        original_relative_name: "dir_a/clip.ogg".to_owned(),
        byte_len: 100,
        sha256: dummy_hash(30),
        duration: DurationInput::Milliseconds(5_000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let m2 = AudioArchiveMemberInput {
        original_relative_name: "dir_b/clip.ogg".to_owned(),
        byte_len: 200,
        sha256: dummy_hash(31),
        duration: DurationInput::Milliseconds(5_000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:05:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let long_member = AudioArchiveMemberInput {
        original_relative_name: "archive/long.ogg".to_owned(),
        byte_len: 2048,
        sha256: dummy_hash(32),
        duration: DurationInput::Milliseconds(400_000),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-01-15T18:07:30.250".to_owned()),
            source_zone: Some("Pacific/Auckland".to_owned()),
        },
        run_note: None,
    };

    let plan = plan_audio_archive(&[m1, m2, long_member], NY, default_settings()).unwrap();
    let name1 = &plan.tiles[0].parts[0].stored_name;
    let name2 = &plan.tiles[1].parts[0].stored_name;
    assert_ne!(name1, name2);

    let p1 = &plan.tiles[3].parts[0];
    assert_eq!(p1.offset_ms, 0);
    assert_eq!(p1.origin_ms, 149750);
    assert_eq!(p1.length_ms, 250250);
    assert!(!p1.stored_name.contains("000500"));
    assert!(!p1.stored_name.contains("001000"));
}

#[test]
fn malformed_manifest_ignored_by_claims() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let legacy_import = journal.join("imports").join("20260101_000000");
    fs::create_dir_all(&legacy_import).unwrap();
    fs::write(legacy_import.join("manifest.json"), b"{").unwrap();

    let plan = plan_audio_archive(
        &[AudioArchiveMemberInput {
            original_relative_name: "clip.ogg".to_owned(),
            byte_len: 500,
            sha256: dummy_hash(40),
            duration: DurationInput::Milliseconds(5000),
            start: MemberStartEvidence {
                instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        }],
        NY,
        default_settings(),
    )
    .unwrap();

    let res = find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT);
    assert!(res.is_ok());
    assert!(res.unwrap().is_empty());
}

#[test]
fn each_archive_fault_and_resume_publishes_raw() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let raw_bytes = b"fault_testing_audio_content".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let byte_len = raw_bytes.len() as u64;

    let member = AudioArchiveMemberInput {
        original_relative_name: "test.ogg".to_owned(),
        byte_len,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut bytes_map = HashMap::new();
    bytes_map.insert(sha256.clone(), raw_bytes.clone());

    let faults = [
        ArchiveFault::AfterRecordFirstSaved,
        ArchiveFault::AfterTileBytesStaged,
        ArchiveFault::AfterChronicleRawsWritten,
        ArchiveFault::AfterPublishBeforeSavedMark,
        ArchiveFault::AfterSavedMarkBeforeEmit,
    ];

    for fault in faults {
        let t = TempDir::new().unwrap();
        let seams = AudioArchivePublishSeams {
            fault: Some(fault),
            emit_observed: None,
            heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
        };
        let res = publish_audio_archive(
            t.path(),
            AudioArchivePublishOptions {
                import_id: None,
                members: vec![member.clone()],
                member_bytes: bytes_map.clone(),
                owner_zone: NY,
                settings: default_settings(),
                source_timestamp: None,
            },
            Some(seams),
        );
        assert!(matches!(res, Err(AudioArchiveError::Fault(f)) if f == fault));
    }

    let seams_claim = AudioArchivePublishSeams {
        fault: Some(ArchiveFault::AfterClaimAndIntent),
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };
    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: bytes_map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_claim),
    );
    assert!(matches!(
        res,
        Err(AudioArchiveError::Fault(ArchiveFault::AfterClaimAndIntent))
    ));

    let imports_dir = journal.join("imports");
    let run_id = fs::read_dir(&imports_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    let seams_clean = AudioArchivePublishSeams {
        fault: None,
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };
    let resume_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&run_id),
            members: vec![member.clone()],
            member_bytes: bytes_map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_clean),
    )
    .unwrap();

    assert_eq!(resume_res.tiles_saved, 1);
    assert_eq!(resume_res.parts_saved, 1);

    let part = &resume_res.record.parts[0];
    let raw_file = journal
        .join("chronicle")
        .join(&part.day)
        .join("import.clips")
        .join(&part.segment_key)
        .join(&part.stored_name);
    assert!(raw_file.is_file());

    let claim_db = claim_db_path(journal);
    let conn = rusqlite::Connection::open(&claim_db).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM audio_archive_claim", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn between_multi_tile_parts_fault_semantics() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let raw_bytes = vec![42u8; 2048];
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));

    let long_member = AudioArchiveMemberInput {
        original_relative_name: "archive/long.ogg".to_owned(),
        byte_len: 2048,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(400_000),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-01-15T18:07:30.250".to_owned()),
            source_zone: Some("Pacific/Auckland".to_owned()),
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256.clone(), raw_bytes);

    let seams = AudioArchivePublishSeams {
        fault: Some(ArchiveFault::BetweenMultiTileParts),
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };

    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![long_member],
            member_bytes: map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams),
    );
    assert!(matches!(
        res,
        Err(AudioArchiveError::Fault(
            ArchiveFault::BetweenMultiTileParts
        ))
    ));

    let imports_dir = journal.join("imports");
    let run_id = fs::read_dir(&imports_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    let record = read_audio_archive(journal, &run_id)
        .unwrap()
        .expect("record exists");
    assert_eq!(record.parts.len(), 2);

    let part0 = &record.parts[0];
    assert_eq!(part0.outcome, PartOutcome::Saved);
    let tile0_placement = journal
        .join("chronicle")
        .join(&part0.day)
        .join("import.clips")
        .join(&part0.segment_key)
        .join(AUDIO_ARCHIVE_PLACEMENT_FILENAME);
    let tile0_raw = journal
        .join("chronicle")
        .join(&part0.day)
        .join("import.clips")
        .join(&part0.segment_key)
        .join(&part0.stored_name);
    assert!(tile0_placement.is_file());
    assert!(tile0_raw.is_file());

    let part1 = &record.parts[1];
    assert_ne!(part1.outcome, PartOutcome::Saved);
    let tile1_placement = journal
        .join("chronicle")
        .join(&part1.day)
        .join("import.clips")
        .join(&part1.segment_key)
        .join(AUDIO_ARCHIVE_PLACEMENT_FILENAME);
    assert!(!tile1_placement.exists());

    let conn = rusqlite::Connection::open(claim_db_path(journal)).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM audio_archive_claim", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn resume_settings_mismatch_leaves_record_unchanged() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let raw_bytes = b"mismatch_audio_test".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let member = AudioArchiveMemberInput {
        original_relative_name: "test.ogg".to_owned(),
        byte_len: raw_bytes.len() as u64,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256, raw_bytes);

    let seams_fault = AudioArchivePublishSeams {
        fault: Some(ArchiveFault::AfterRecordFirstSaved),
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };
    let _ = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_fault),
    );

    let imports_dir = journal.join("imports");
    let run_id = fs::read_dir(&imports_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    let record_path = journal
        .join("imports")
        .join(&run_id)
        .join("audio-archive.json");
    let original_bytes = fs::read(&record_path).unwrap();

    let mismatched_settings = AudioArchiveSettings {
        qualifier: "clips".to_owned(),
        archive_identity: "completely_different_identity".to_owned(),
    };
    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&run_id),
            members: vec![member],
            member_bytes: map,
            owner_zone: NY,
            settings: mismatched_settings,
            source_timestamp: None,
        },
        None,
    );
    assert!(matches!(
        res,
        Err(AudioArchiveError::SettingsMismatch { .. })
    ));

    let bytes_after = fs::read(&record_path).unwrap();
    assert_eq!(original_bytes, bytes_after);
}

#[test]
fn delete_chronicle_raw_and_resume_does_not_recreate() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let raw_bytes = b"delete_test_audio".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let member = AudioArchiveMemberInput {
        original_relative_name: "test.ogg".to_owned(),
        byte_len: raw_bytes.len() as u64,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256, raw_bytes);

    let clean = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    )
    .unwrap();

    let part = &clean.record.parts[0];
    let raw_path = journal
        .join("chronicle")
        .join(&part.day)
        .join("import.clips")
        .join(&part.segment_key)
        .join(&part.stored_name);
    assert!(raw_path.is_file());

    fs::remove_file(&raw_path).unwrap();
    assert!(!raw_path.exists());

    let resume_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&clean.import_id),
            members: vec![member],
            member_bytes: map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(resume_res.tiles_saved, 1);
    assert!(!raw_path.exists());

    let frontier = load_frontier(journal, &clean.import_id).unwrap();
    assert_eq!(frontier.unavailable_parts.len(), 1);
    assert_eq!(
        frontier.published_tiles[0].parts[0].outcome,
        PartOutcome::SavedButUnavailable
    );
}

#[test]
fn foreign_file_in_segment_refused_no_key_advance() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let clip_dir = journal
        .join("chronicle")
        .join("20260114")
        .join("import.clips")
        .join("160000_300");
    fs::create_dir_all(&clip_dir).unwrap();
    let foreign_path = clip_dir.join("foreign.txt");
    fs::write(&foreign_path, b"foreign content").unwrap();

    let phone_dir = journal
        .join("chronicle")
        .join("20260114")
        .join("device.phone")
        .join("160000_300");
    fs::create_dir_all(&phone_dir).unwrap();
    let phone_path = phone_dir.join("phone.txt");
    fs::write(&phone_path, b"phone data").unwrap();

    let raw_bytes = b"planned_member_bytes".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let member = AudioArchiveMemberInput {
        original_relative_name: "test.ogg".to_owned(),
        byte_len: raw_bytes.len() as u64,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256, raw_bytes);

    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member],
            member_bytes: map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    );
    assert!(matches!(
        res,
        Err(AudioArchiveError::ChronicleSegmentRefused(_))
    ));

    assert_eq!(fs::read(&foreign_path).unwrap(), b"foreign content");
    assert_eq!(fs::read(&phone_path).unwrap(), b"phone data");
    let parent = clip_dir.parent().unwrap();
    let entries: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file_name(), "160000_300");
}

#[test]
fn frontier_invariants_across_attempts_and_parts() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let import_id = "20260114_210000";
    let run_dir = journal.join("imports").join(import_id);
    fs::create_dir_all(&run_dir).unwrap();

    let record = AudioArchiveRecord {
        version: 1,
        settings: AudioArchiveRecordSettings {
            owner_zone: "America/New_York".to_owned(),
            window_ms: 300_000,
            stream: "import.clips".to_owned(),
            qualifier: "clips".to_owned(),
            archive_identity: "frontier_test".to_owned(),
            len: 300,
        },
        members: vec![
            AudioArchiveRecordMember {
                byte_len: 100,
                sha256: dummy_hash(1),
                canonical_occurrence: "utc|1768424400000".to_owned(),
                original_relative_name: "a.ogg".to_owned(),
                run_note: None,
                disposition: MemberDisposition::Included,
            },
            AudioArchiveRecordMember {
                byte_len: 100,
                sha256: dummy_hash(2),
                canonical_occurrence: "utc|1768424400000".to_owned(),
                original_relative_name: "b.ogg".to_owned(),
                run_note: None,
                disposition: MemberDisposition::Included,
            },
            AudioArchiveRecordMember {
                byte_len: 200,
                sha256: dummy_hash(3),
                canonical_occurrence: "utc|1768453650250".to_owned(),
                original_relative_name: "long.ogg".to_owned(),
                run_note: None,
                disposition: MemberDisposition::Included,
            },
            AudioArchiveRecordMember {
                byte_len: 100,
                sha256: dummy_hash(4),
                canonical_occurrence: "utc|1768564800000".to_owned(),
                original_relative_name: "d.ogg".to_owned(),
                run_note: None,
                disposition: MemberDisposition::Included,
            },
        ],
        parts: vec![
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(1),
                member_occurrence: "utc|1768424400000".to_owned(),
                day: "20260114".to_owned(),
                segment_key: "160000_300".to_owned(),
                offset_ms: 0,
                origin_ms: 0,
                length_ms: 5000,
                stored_name: "p0000_1111111111111111.ogg".to_owned(),
                outcome: PartOutcome::Saved,
            },
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(2),
                member_occurrence: "utc|1768424400000".to_owned(),
                day: "20260114".to_owned(),
                segment_key: "160000_300".to_owned(),
                offset_ms: 5000,
                origin_ms: 0,
                length_ms: 5000,
                stored_name: "p0000_2222222222222222.ogg".to_owned(),
                outcome: PartOutcome::Planned,
            },
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(3),
                member_occurrence: "utc|1768453650250".to_owned(),
                day: "20260115".to_owned(),
                segment_key: "000500_300".to_owned(),
                offset_ms: 150250,
                origin_ms: 0,
                length_ms: 149750,
                stored_name: "p0000_3333333333333333.ogg".to_owned(),
                outcome: PartOutcome::Saved,
            },
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(3),
                member_occurrence: "utc|1768453650250".to_owned(),
                day: "20260115".to_owned(),
                segment_key: "001000_300".to_owned(),
                offset_ms: 0,
                origin_ms: 149750,
                length_ms: 250250,
                stored_name: "p0001_3333333333333333.ogg".to_owned(),
                outcome: PartOutcome::Planned,
            },
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(4),
                member_occurrence: "utc|1768564800000".to_owned(),
                day: "20260116".to_owned(),
                segment_key: "120000_300".to_owned(),
                offset_ms: 0,
                origin_ms: 0,
                length_ms: 5000,
                stored_name: "p0000_4444444444444444.ogg".to_owned(),
                outcome: PartOutcome::Planned,
            },
        ],
    };
    save_audio_archive_record(journal, import_id, &record).unwrap();

    let t1_dir = journal.join("chronicle/20260114/import.clips/160000_300");
    fs::create_dir_all(&t1_dir).unwrap();
    fs::write(t1_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME), b"{}").unwrap();

    let t2_dir = journal.join("chronicle/20260115/import.clips/000500_300");
    fs::create_dir_all(&t2_dir).unwrap();
    fs::write(t2_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME), b"{}").unwrap();
    fs::write(t2_dir.join("p0000_3333333333333333.ogg"), b"audio").unwrap();

    admit_running_attempt(journal, import_id, now_ms(), None).unwrap();
    record_completed_attempt(journal, import_id, 1, now_ms(), None, None).unwrap();

    let frontier = load_frontier(journal, import_id).unwrap();

    assert!(
        frontier
            .pending_tiles
            .iter()
            .any(|t| t.parts.iter().any(|p| p.outcome == PartOutcome::Planned))
    );

    let t1 = frontier
        .published_tiles
        .iter()
        .chain(&frontier.pending_tiles)
        .find(|t| t.day == "20260114" && t.segment_key == "160000_300")
        .expect("tile 1 found");
    assert_eq!(t1.parts.len(), 2);
    assert!(t1.parts.iter().any(|p| p.outcome == PartOutcome::Planned));

    assert!(
        frontier.pending_tiles.iter().any(|t| t
            .parts
            .iter()
            .any(|p| p.stored_name == "p0001_3333333333333333.ogg"
                && p.outcome == PartOutcome::Planned))
    );

    assert_eq!(frontier.unavailable_parts.len(), 1);
    assert_eq!(
        frontier.unavailable_parts[0].stored_name,
        "p0000_1111111111111111.ogg"
    );
    assert!(
        frontier
            .pending_tiles
            .iter()
            .any(|t| t.day == "20260116" && t.segment_key == "120000_300")
    );
}

#[test]
fn emit_seam_invoked_once_per_finished_tile() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = count.clone();

    let emit_fn: EmitObservedFn = Box::new(move |_day, _key, _stream| {
        count_clone.fetch_add(1, Ordering::SeqCst);
    });

    let raw_bytes = b"emit_seam_audio_bytes".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let member = AudioArchiveMemberInput {
        original_relative_name: "test.ogg".to_owned(),
        byte_len: raw_bytes.len() as u64,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256, raw_bytes);

    let seams_fault = AudioArchivePublishSeams {
        fault: Some(ArchiveFault::AfterClaimAndIntent),
        emit_observed: Some(emit_fn),
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };

    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_fault),
    );
    assert!(res.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);

    let clean_count = Arc::new(AtomicUsize::new(0));
    let clean_count_clone = clean_count.clone();
    let emit_clean: EmitObservedFn = Box::new(move |_day, _key, _stream| {
        clean_count_clone.fetch_add(1, Ordering::SeqCst);
    });

    let seams_clean = AudioArchivePublishSeams {
        fault: None,
        emit_observed: Some(emit_clean),
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };

    let temp_clean = TempDir::new().unwrap();
    let clean_res = publish_audio_archive(
        temp_clean.path(),
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member],
            member_bytes: map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_clean),
    )
    .unwrap();
    assert_eq!(clean_res.tiles_saved, 1);
    assert_eq!(clean_count.load(Ordering::SeqCst), 1);
}

#[test]
fn claim_contention_zero_timeout_returns_busy() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let id1 = MemberIdentity::new(100, dummy_hash(1), "utc|1768424400500");
    insert_tile_claims(journal, "run_1", &[id1], Duration::from_millis(2000)).unwrap();

    let path = claim_db_path(journal);
    let blocker_conn = rusqlite::Connection::open(&path).unwrap();
    blocker_conn.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let id2 = MemberIdentity::new(200, dummy_hash(2), "utc|1768424400500");
    let res = insert_tile_claims(journal, "run_2", &[id2], Duration::ZERO);
    assert!(matches!(res, Err(AudioArchiveError::ClaimDatabaseBusy(_))));

    blocker_conn.execute_batch("COMMIT;").unwrap();
}

#[test]
fn after_chronicle_raws_written_resume_publishes_raw_and_leaves_one_claim_row() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let member_bytes_data = b"synthetic test audio raw bytes".to_vec();
    let byte_len = member_bytes_data.len() as u64;
    let sha256 = format!("{:x}", Sha256::digest(&member_bytes_data));

    let member = AudioArchiveMemberInput {
        original_relative_name: "archive/clip.ogg".to_owned(),
        byte_len,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(7500),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.500Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };

    let mut bytes_map = HashMap::new();
    bytes_map.insert(sha256.clone(), member_bytes_data.clone());

    let seams_fault = AudioArchivePublishSeams {
        fault: Some(ArchiveFault::AfterChronicleRawsWritten),
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };

    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: bytes_map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_fault),
    );
    assert!(matches!(
        res,
        Err(AudioArchiveError::Fault(
            ArchiveFault::AfterChronicleRawsWritten
        ))
    ));

    // Find the import_id created by this attempt
    let import_id = fs::read_dir(journal.join("imports"))
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    let record = read_audio_archive(journal, &import_id).unwrap().unwrap();
    let part = &record.parts[0];
    let segment_dir = journal
        .join("chronicle")
        .join(&part.day)
        .join("import.clips")
        .join(&part.segment_key);
    let partial_path = segment_dir.join(format!("{}.partial", part.stored_name));
    let raw_path = segment_dir.join(&part.stored_name);

    assert!(partial_path.is_file());
    assert!(!raw_path.exists());

    // Resume with same import_id and no fault
    let seams_clean = AudioArchivePublishSeams {
        fault: None,
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };
    let clean_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&import_id),
            members: vec![member],
            member_bytes: bytes_map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_clean),
    )
    .unwrap();

    assert_eq!(clean_res.tiles_saved, 1);
    assert_eq!(clean_res.parts_saved, 1);
    assert!(raw_path.is_file());
    assert!(!partial_path.exists());

    // Exactly one claim row in claim db
    let conn = rusqlite::Connection::open(claim_db_path(journal)).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audio_archive_claim WHERE import_id = ?",
            [&import_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn after_publish_before_saved_mark_delete_raw_resume() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let member_bytes_data = b"synthetic test audio bytes for unavailable".to_vec();
    let byte_len = member_bytes_data.len() as u64;
    let sha256 = format!("{:x}", Sha256::digest(&member_bytes_data));

    let member = AudioArchiveMemberInput {
        original_relative_name: "archive/clip.ogg".to_owned(),
        byte_len,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(7500),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.500Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };

    let mut bytes_map = HashMap::new();
    bytes_map.insert(sha256.clone(), member_bytes_data.clone());

    let seams_fault = AudioArchivePublishSeams {
        fault: Some(ArchiveFault::AfterPublishBeforeSavedMark),
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };

    let res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: bytes_map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_fault),
    );
    assert!(matches!(
        res,
        Err(AudioArchiveError::Fault(
            ArchiveFault::AfterPublishBeforeSavedMark
        ))
    ));

    // Find the import_id
    let import_id = fs::read_dir(journal.join("imports"))
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();

    let record = read_audio_archive(journal, &import_id).unwrap().unwrap();
    let part = &record.parts[0];
    let segment_dir = journal
        .join("chronicle")
        .join(&part.day)
        .join("import.clips")
        .join(&part.segment_key);
    let raw_path = segment_dir.join(&part.stored_name);
    assert!(raw_path.is_file());

    // Delete the chronicle raw
    fs::remove_file(&raw_path).unwrap();
    assert!(!raw_path.exists());

    // Resume same id with no fault
    let seams_clean = AudioArchivePublishSeams {
        fault: None,
        emit_observed: None,
        heartbeat_spawner: Some(|_, _| Ok(std::thread::spawn(|| {}))),
    };
    let clean_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&import_id),
            members: vec![member],
            member_bytes: bytes_map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        Some(seams_clean),
    )
    .unwrap();

    assert_eq!(clean_res.tiles_saved, 1);
    assert_eq!(clean_res.parts_saved, 1);
    // The raw stays absent and was not recreated
    assert!(!raw_path.exists());
    // The saved outcome is SavedButUnavailable
    assert_eq!(
        clean_res.record.parts[0].outcome,
        PartOutcome::SavedButUnavailable
    );
}

#[test]
fn corrupt_audio_archive_json_returns_claim_database_corrupt() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let plan = plan_audio_archive(
        &[AudioArchiveMemberInput {
            original_relative_name: "clip.ogg".to_owned(),
            byte_len: 500,
            sha256: dummy_hash(80),
            duration: DurationInput::Milliseconds(5000),
            start: MemberStartEvidence {
                instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        }],
        NY,
        default_settings(),
    )
    .unwrap();

    let id1 = plan.members[0].identity.clone();
    insert_tile_claims(
        journal,
        "20260114_100003",
        std::slice::from_ref(&id1),
        DEFAULT_WRITE_BUSY_TIMEOUT,
    )
    .unwrap();

    let run_dir = journal.join("imports").join("20260114_100003");
    fs::create_dir_all(&run_dir).unwrap();

    // Write {} to audio-archive.json -> returns ClaimDatabaseCorrupt
    let archive_json = run_dir.join("audio-archive.json");
    fs::write(&archive_json, b"{}").unwrap();

    let err =
        find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT).unwrap_err();
    assert!(matches!(err, AudioArchiveError::ClaimDatabaseCorrupt(_)));

    // Deleting that file restores the existing hold behavior. No tombstone.
    fs::remove_file(&archive_json).unwrap();
    let held = find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT).unwrap();
    assert!(held.contains(&id1));
    assert!(!run_dir.join("tombstone.json").exists());
}

#[test]
fn dst_fixture_fall_back_boundary() {
    let m = AudioArchiveMemberInput {
        original_relative_name: "fall_back_boundary.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(31),
        duration: DurationInput::Milliseconds(20_000),
        start: MemberStartEvidence {
            instant: Some("2026-11-01T01:59:50-04:00".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let plan = plan_audio_archive(&[m], NY, default_settings()).unwrap();
    assert_eq!(plan.members[0].disposition, MemberDisposition::Included);
    assert_eq!(plan.tiles.len(), 2);

    let t0 = plan
        .tiles
        .iter()
        .find(|t| t.segment_key == "015500_300")
        .expect("015500_300");
    assert_eq!(t0.day, "20261101");
    assert_eq!(t0.segment_key, "015500_300");
    assert_eq!(t0.parts.len(), 1);
    assert_eq!(t0.parts[0].offset_ms, 290_000);
    assert_eq!(t0.parts[0].origin_ms, 0);
    assert_eq!(t0.parts[0].length_ms, 10_000);
    assert_eq!(t0.parts[0].offset_ms + t0.parts[0].length_ms, 300_000);

    let t1 = plan
        .tiles
        .iter()
        .find(|t| t.segment_key == "010000_300")
        .expect("010000_300");
    assert_eq!(t1.day, "20261101");
    assert_eq!(t1.segment_key, "010000_300");
    assert_eq!(t1.parts.len(), 1);
    assert_eq!(t1.parts[0].offset_ms, 0);
    assert_eq!(t1.parts[0].origin_ms, 10_000);
    assert_eq!(t1.parts[0].length_ms, 10_000);
}

#[test]
fn dst_fixture_fall_back_crossing_unique() {
    let m = AudioArchiveMemberInput {
        original_relative_name: "fall_back_crossing.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(32),
        duration: DurationInput::Milliseconds(360_000),
        start: MemberStartEvidence {
            instant: Some("2026-11-01T01:30:00-04:00".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let plan = plan_audio_archive(&[m], NY, default_settings()).unwrap();
    assert_eq!(plan.members[0].disposition, MemberDisposition::Included);
    assert_eq!(plan.tiles.len(), 2);

    assert_eq!(plan.tiles[0].segment_key, "013000_300");
    assert_eq!(plan.tiles[0].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[0].parts[0].origin_ms, 0);
    assert_eq!(plan.tiles[0].parts[0].length_ms, 300_000);

    assert_eq!(plan.tiles[1].segment_key, "013500_300");
    assert_eq!(plan.tiles[1].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[1].parts[0].origin_ms, 300_000);
    assert_eq!(plan.tiles[1].parts[0].length_ms, 60_000);
}

#[test]
fn dst_fixture_ambiguous_civil_included() {
    let m = AudioArchiveMemberInput {
        original_relative_name: "ambiguous_settled.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(33),
        duration: DurationInput::Milliseconds(360_000),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-11-01T01:30:00.000".to_owned()),
            source_zone: Some("America/New_York".to_owned()),
        },
        run_note: None,
    };
    let plan = plan_audio_archive(&[m], NY, default_settings()).unwrap();
    assert_eq!(plan.members[0].disposition, MemberDisposition::Included);
    assert_eq!(
        plan.members[0].identity.occurrence,
        "local|America/New_York|2026-11-01T01:30:00.000"
    );
    assert_eq!(plan.tiles.len(), 2);

    assert_eq!(plan.tiles[0].segment_key, "013000_300");
    assert_eq!(plan.tiles[0].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[0].parts[0].origin_ms, 0);
    assert_eq!(plan.tiles[0].parts[0].length_ms, 300_000);
    assert_eq!(plan.tiles[0].parts[0].original_time_ms, None);

    assert_eq!(plan.tiles[1].segment_key, "013500_300");
    assert_eq!(plan.tiles[1].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[1].parts[0].origin_ms, 300_000);
    assert_eq!(plan.tiles[1].parts[0].length_ms, 60_000);
    assert_eq!(plan.tiles[1].parts[0].original_time_ms, None);
}

#[test]
fn dst_fixture_spring_forward_gap_crossing() {
    let m = AudioArchiveMemberInput {
        original_relative_name: "spring_forward_1200s.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(34),
        duration: DurationInput::Milliseconds(1_200_000),
        start: MemberStartEvidence {
            instant: None,
            local_civil: Some("2026-03-08T01:50:00.000".to_owned()),
            source_zone: Some("America/New_York".to_owned()),
        },
        run_note: None,
    };
    let plan = plan_audio_archive(&[m], NY, default_settings()).unwrap();
    assert_eq!(plan.members[0].disposition, MemberDisposition::Included);
    assert_eq!(plan.tiles.len(), 3);

    assert_eq!(plan.tiles[0].segment_key, "015000_300");
    assert_eq!(plan.tiles[0].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[0].parts[0].origin_ms, 0);
    assert_eq!(plan.tiles[0].parts[0].length_ms, 300_000);

    assert_eq!(plan.tiles[1].segment_key, "015500_300");
    assert_eq!(plan.tiles[1].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[1].parts[0].origin_ms, 300_000);
    assert_eq!(plan.tiles[1].parts[0].length_ms, 300_000);

    assert_eq!(plan.tiles[2].segment_key, "030000_300");
    assert_eq!(plan.tiles[2].parts[0].offset_ms, 0);
    assert_eq!(plan.tiles[2].parts[0].origin_ms, 600_000);
    assert_eq!(plan.tiles[2].parts[0].length_ms, 600_000);

    for tile in &plan.tiles {
        assert!(!tile.segment_key.starts_with("02"));
    }
}

#[test]
fn duration_i64_max_left_out_unsettled_time() {
    let m = AudioArchiveMemberInput {
        original_relative_name: "overflow_i64_max.ogg".to_owned(),
        byte_len: 1000,
        sha256: dummy_hash(35),
        duration: DurationInput::Milliseconds(i64::MAX),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let plan = plan_audio_archive(&[m], NY, default_settings()).unwrap();
    assert_eq!(
        plan.members[0].disposition,
        MemberDisposition::LeftOut(LeftOutReason::UnsettledTime)
    );
    assert_eq!(plan.tiles.len(), 0);
}

#[test]
fn placement_different_offset_refuses_chronicle_segment() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let raw_bytes = b"different_offset_audio".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let member = AudioArchiveMemberInput {
        original_relative_name: "offset_test.ogg".to_owned(),
        byte_len: raw_bytes.len() as u64,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256.clone(), raw_bytes.clone());

    let clean = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    )
    .unwrap();

    let part = &clean.record.parts[0];
    let seg_dir = journal
        .join("chronicle")
        .join(&part.day)
        .join("import.clips")
        .join(&part.segment_key);
    let placement_path = seg_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME);

    let mismatched_placement = solstone_core_format::audio_archive::AudioArchivePlacement {
        version: 1,
        parts: vec![
            solstone_core_format::audio_archive::AudioArchivePlacementPart {
                stored_name: part.stored_name.clone(),
                original_occurrence: part.member_occurrence.clone(),
                original_time_ms: None,
                origin_ms: part.origin_ms,
                offset_ms: part.offset_ms + 100,
                length_ms: part.length_ms,
            },
        ],
    };
    fs::write(
        &placement_path,
        serde_json::to_vec_pretty(&mismatched_placement).unwrap(),
    )
    .unwrap();

    let resume_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&clean.import_id),
            members: vec![member],
            member_bytes: map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    );
    assert!(matches!(
        resume_res,
        Err(AudioArchiveError::ChronicleSegmentRefused(_))
    ));
}

#[test]
fn publication_candidate_file_does_not_refuse_and_resume_does_not_recreate() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let raw_bytes = b"pub_candidate_probe_audio".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let member = AudioArchiveMemberInput {
        original_relative_name: "pub_candidate.ogg".to_owned(),
        byte_len: raw_bytes.len() as u64,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256, raw_bytes);

    let clean = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    )
    .unwrap();

    let part = &clean.record.parts[0];
    let seg_dir = journal
        .join("chronicle")
        .join(&part.day)
        .join("import.clips")
        .join(&part.segment_key);
    let raw_path = seg_dir.join(&part.stored_name);
    assert!(raw_path.is_file());

    fs::remove_file(&raw_path).unwrap();

    let probe_file = seg_dir.join(".tmp_resume_probe.tmp");
    fs::write(&probe_file, b"probe candidate data").unwrap();

    let resume_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&clean.import_id),
            members: vec![member],
            member_bytes: map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(resume_res.tiles_saved, 1);
    assert!(!raw_path.exists());
    assert_eq!(
        resume_res.record.parts[0].outcome,
        PartOutcome::SavedButUnavailable
    );
}

#[test]
fn short_staged_raw_replaced_from_member_bytes_on_resume() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let raw_bytes = b"complete_valid_audio_payload_bytes".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&raw_bytes));
    let member = AudioArchiveMemberInput {
        original_relative_name: "staged_truncate.ogg".to_owned(),
        byte_len: raw_bytes.len() as u64,
        sha256: sha256.clone(),
        duration: DurationInput::Milliseconds(5000),
        start: MemberStartEvidence {
            instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
            local_civil: None,
            source_zone: None,
        },
        run_note: None,
    };
    let mut map = HashMap::new();
    map.insert(sha256.clone(), raw_bytes.clone());

    let clean = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: None,
            members: vec![member.clone()],
            member_bytes: map.clone(),
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    )
    .unwrap();

    let part = &clean.record.parts[0];
    let staged_file = journal
        .join("imports")
        .join(&clean.import_id)
        .join("staged")
        .join(&part.day)
        .join(&part.segment_key)
        .join(&part.stored_name);
    assert!(staged_file.is_file());

    fs::write(&staged_file, b"short").unwrap();

    let seg_dir = journal
        .join("chronicle")
        .join(&part.day)
        .join("import.clips")
        .join(&part.segment_key);
    let raw_path = seg_dir.join(&part.stored_name);
    let placement_path = seg_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME);

    fs::remove_file(&raw_path).unwrap();
    fs::remove_file(&placement_path).unwrap();

    let resume_res = publish_audio_archive(
        journal,
        AudioArchivePublishOptions {
            import_id: Some(&clean.import_id),
            members: vec![member],
            member_bytes: map,
            owner_zone: NY,
            settings: default_settings(),
            source_timestamp: None,
        },
        None,
    )
    .unwrap();

    assert_eq!(resume_res.tiles_saved, 1);
    assert_eq!(resume_res.parts_saved, 1);

    let chronicle_bytes = fs::read(&raw_path).unwrap();
    assert_eq!(chronicle_bytes, raw_bytes);
}

#[test]
fn find_held_members_zero_timeout_under_immediate_returns_busy() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let plan = plan_audio_archive(
        &[AudioArchiveMemberInput {
            original_relative_name: "busy_test.ogg".to_owned(),
            byte_len: 500,
            sha256: dummy_hash(40),
            duration: DurationInput::Milliseconds(5000),
            start: MemberStartEvidence {
                instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        }],
        NY,
        default_settings(),
    )
    .unwrap();

    let id = plan.members[0].identity.clone();
    insert_tile_claims(
        journal,
        "20260114_100005",
        std::slice::from_ref(&id),
        DEFAULT_WRITE_BUSY_TIMEOUT,
    )
    .unwrap();

    let db_path = claim_db_path(journal);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();

    let res = find_held_members(journal, None, &plan.members, Duration::ZERO);
    assert!(matches!(res, Err(AudioArchiveError::ClaimDatabaseBusy(_))));

    conn.execute_batch("ROLLBACK;").unwrap();
}

#[test]
fn claim_row_with_dot_import_id_returns_claim_database_corrupt() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let plan = plan_audio_archive(
        &[AudioArchiveMemberInput {
            original_relative_name: "dot_id.ogg".to_owned(),
            byte_len: 500,
            sha256: dummy_hash(41),
            duration: DurationInput::Milliseconds(5000),
            start: MemberStartEvidence {
                instant: Some("2026-01-14T21:00:00.000Z".to_owned()),
                local_civil: None,
                source_zone: None,
            },
            run_note: None,
        }],
        NY,
        default_settings(),
    )
    .unwrap();

    let id = plan.members[0].identity.clone();
    insert_tile_claims(
        journal,
        "20260114_100006",
        std::slice::from_ref(&id),
        DEFAULT_WRITE_BUSY_TIMEOUT,
    )
    .unwrap();

    let db_path = claim_db_path(journal);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE audio_archive_claim SET import_id = '.' WHERE sha256 = ?;",
        rusqlite::params![id.sha256],
    )
    .unwrap();

    let res = find_held_members(journal, None, &plan.members, DEFAULT_WRITE_BUSY_TIMEOUT);
    assert!(matches!(
        res,
        Err(AudioArchiveError::ClaimDatabaseCorrupt(_))
    ));

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audio_archive_claim WHERE import_id = '.';",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn frontier_directory_and_symlink_are_saved_but_unavailable() {
    let temp = TempDir::new().unwrap();
    let journal = temp.path();

    let import_id = "20260114_100007";
    let run_dir = journal.join("imports").join(import_id);
    fs::create_dir_all(&run_dir).unwrap();

    let record = AudioArchiveRecord {
        version: 1,
        settings: AudioArchiveRecordSettings {
            owner_zone: "America/New_York".to_owned(),
            window_ms: 300_000,
            stream: "import.clips".to_owned(),
            qualifier: "clips".to_owned(),
            archive_identity: "frontier_dir_sym_test".to_owned(),
            len: 300,
        },
        members: vec![
            AudioArchiveRecordMember {
                byte_len: 100,
                sha256: dummy_hash(42),
                canonical_occurrence: "utc|1768424400000".to_owned(),
                original_relative_name: "m1.ogg".to_owned(),
                run_note: None,
                disposition: MemberDisposition::Included,
            },
            AudioArchiveRecordMember {
                byte_len: 200,
                sha256: dummy_hash(43),
                canonical_occurrence: "utc|1768424700000".to_owned(),
                original_relative_name: "m2.ogg".to_owned(),
                run_note: None,
                disposition: MemberDisposition::Included,
            },
            AudioArchiveRecordMember {
                byte_len: 300,
                sha256: dummy_hash(44),
                canonical_occurrence: "utc|1768425000000".to_owned(),
                original_relative_name: "m3.ogg".to_owned(),
                run_note: None,
                disposition: MemberDisposition::Included,
            },
        ],
        parts: vec![
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(42),
                member_occurrence: "utc|1768424400000".to_owned(),
                day: "20260114".to_owned(),
                segment_key: "160000_300".to_owned(),
                stored_name: "p0000_2222222222222222.ogg".to_owned(),
                offset_ms: 0,
                origin_ms: 0,
                length_ms: 1000,
                outcome: PartOutcome::Saved,
            },
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(43),
                member_occurrence: "utc|1768424700000".to_owned(),
                day: "20260114".to_owned(),
                segment_key: "160500_300".to_owned(),
                stored_name: "p0000_3333333333333333.ogg".to_owned(),
                offset_ms: 0,
                origin_ms: 0,
                length_ms: 1000,
                outcome: PartOutcome::Saved,
            },
            AudioArchiveRecordPart {
                member_sha256: dummy_hash(44),
                member_occurrence: "utc|1768425000000".to_owned(),
                day: "20260114".to_owned(),
                segment_key: "161000_300".to_owned(),
                stored_name: "p0000_4444444444444444.ogg".to_owned(),
                offset_ms: 0,
                origin_ms: 0,
                length_ms: 1000,
                outcome: PartOutcome::Planned,
            },
        ],
    };

    save_audio_archive_record(journal, import_id, &record).unwrap();
    admit_running_attempt(journal, import_id, now_ms(), None).unwrap();
    record_completed_attempt(journal, import_id, 1, now_ms(), None, None).unwrap();

    let t1_dir = journal
        .join("chronicle")
        .join("20260114")
        .join("import.clips")
        .join("160000_300");
    fs::create_dir_all(&t1_dir).unwrap();
    let pl1 = solstone_core_format::audio_archive::AudioArchivePlacement {
        version: 1,
        parts: vec![
            solstone_core_format::audio_archive::AudioArchivePlacementPart {
                stored_name: "p0000_2222222222222222.ogg".to_owned(),
                original_occurrence: "utc|1768424400000".to_owned(),
                original_time_ms: Some(1768424400000),
                origin_ms: 0,
                offset_ms: 0,
                length_ms: 1000,
            },
        ],
    };
    fs::write(
        t1_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME),
        serde_json::to_vec_pretty(&pl1).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(t1_dir.join("p0000_2222222222222222.ogg")).unwrap();

    let t2_dir = journal
        .join("chronicle")
        .join("20260114")
        .join("import.clips")
        .join("160500_300");
    fs::create_dir_all(&t2_dir).unwrap();
    let pl2 = solstone_core_format::audio_archive::AudioArchivePlacement {
        version: 1,
        parts: vec![
            solstone_core_format::audio_archive::AudioArchivePlacementPart {
                stored_name: "p0000_3333333333333333.ogg".to_owned(),
                original_occurrence: "utc|1768424700000".to_owned(),
                original_time_ms: Some(1768424700000),
                origin_ms: 0,
                offset_ms: 0,
                length_ms: 1000,
            },
        ],
    };
    fs::write(
        t2_dir.join(AUDIO_ARCHIVE_PLACEMENT_FILENAME),
        serde_json::to_vec_pretty(&pl2).unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        "target_does_not_matter",
        t2_dir.join("p0000_3333333333333333.ogg"),
    )
    .unwrap();

    let frontier = load_frontier(journal, import_id).unwrap();

    assert_eq!(frontier.unavailable_parts.len(), 2);
    assert!(
        frontier
            .unavailable_parts
            .iter()
            .any(|p| p.stored_name == "p0000_2222222222222222.ogg")
    );
    assert!(
        frontier
            .unavailable_parts
            .iter()
            .any(|p| p.stored_name == "p0000_3333333333333333.ogg")
    );

    let pub1 = frontier
        .published_tiles
        .iter()
        .find(|t| t.segment_key == "160000_300")
        .expect("pub1");
    assert_eq!(pub1.parts[0].outcome, PartOutcome::SavedButUnavailable);

    let pub2 = frontier
        .published_tiles
        .iter()
        .find(|t| t.segment_key == "160500_300")
        .expect("pub2");
    assert_eq!(pub2.parts[0].outcome, PartOutcome::SavedButUnavailable);

    assert_eq!(frontier.pending_tiles.len(), 1);
    assert_eq!(frontier.pending_tiles[0].segment_key, "161000_300");
    assert_eq!(
        frontier.pending_tiles[0].parts[0].outcome,
        PartOutcome::Planned
    );
}
