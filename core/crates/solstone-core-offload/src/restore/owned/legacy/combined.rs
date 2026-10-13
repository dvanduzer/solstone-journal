// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Continue one admitted restore through the historical manifests it selected.
//! The owner's restore request is the approval: each review is approved with
//! exactly the candidate it produced, inside the same live read.

use super::*;
use std::collections::BTreeMap;

/// Owner-level token for a refused review; the exact review reason stays in detail.
fn owner_reason(reason: &'static str) -> &'static str {
    match reason {
        "legacy_insufficient_free_space" => "insufficient_free_space",
        "legacy_verification_failed" | "legacy_resolution_changed" => "legacy_verification_failed",
        "legacy_read_refused" => "legacy_read_failed",
        _ => "legacy_review_held",
    }
}

/// Any owner that a later read must not run past.
fn held(file: &FileRecord) -> bool {
    match &file.owner {
        FileOwner::None => false,
        FileOwner::Transaction(transaction) => !transaction.completed(),
        FileOwner::Staging(_) | FileOwner::Review(_) => true,
    }
}

/// Unattempted unbound files, grouped by segment and manifest in catalog order.
fn manifests(files: &[FileRecord]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if file.witness.repository_id.is_some() || !matches!(file.owner, FileOwner::None) {
            continue;
        }
        let same = |other: &FileRecord| {
            other.day == file.day
                && other.stream == file.stream
                && other.segment == file.segment
                && other.witness.manifest_id == file.witness.manifest_id
        };
        match groups.iter_mut().find(|group| same(&files[group[0]])) {
            Some(group) => group.push(index),
            None => groups.push(vec![index]),
        }
    }
    groups
}

/// The historical record names only stream and segment key: the destination is
/// the one existing directory with that identity, and it must still admit a restore.
fn selection(
    root: &Arc<JournalRoot>,
    discovered: &mut BTreeMap<String, Result<Vec<Segment>, String>>,
    files: &[FileRecord],
    group: &[usize],
) -> Result<LegacyRestoreSelection, &'static str> {
    let file = &files[group[0]];
    let segments = discovered
        .entry(file.day.clone())
        .or_insert_with(|| discover(root, &file.day))
        .as_ref()
        .map_err(|_| "segment_identity")?;
    let mut matching = segments.iter().filter_map(|segment| {
        segment
            .locator_identity()
            .ok()
            .filter(|identity| identity.stream == file.stream && identity.key == file.segment)
            .map(|identity| (segment, identity))
    });
    let (segment, identity) = matching.next().ok_or("segment_missing")?;
    if matching.next().is_some() {
        return Err("segment_identity");
    }
    // Owner deletion or a replaced directory refuses here, before any read.
    PreparedRestore::admit(Arc::clone(root), &file.day, segment)
        .map_err(|_| "destination_admission_failed")?;
    Ok(LegacyRestoreSelection {
        target: Target {
            day: file.day.clone(),
            stream: file.stream.clone(),
            dir: identity.name.to_owned(),
        },
        layout: identity.layout,
        manifest_id: file.witness.manifest_id.clone(),
        names: group
            .iter()
            .map(|index| files[*index].witness.file.name.clone())
            .collect(),
    })
}

/// Move per-file results back in selection order. No owner is ever dropped.
fn merge(files: &mut Vec<FileRecord>, group: &[usize], restored: Vec<FileRecord>) {
    let mut restored = restored.into_iter();
    for index in group {
        let Some(record) = restored.next() else {
            break;
        };
        let file = &mut files[*index];
        file.disposition = record.disposition;
        file.reason = record.reason;
        file.files = record.files;
        file.bytes = record.bytes;
        file.owner = record.owner;
    }
    files.extend(restored);
}

pub(in crate::restore::owned) fn continue_with_legacy<'clock, R: RestoreRead<'clock>>(
    execution: RestoreExecution<R>,
    time: u64,
) -> RestoreExecution<R> {
    let RestoreExecution {
        mut report,
        _operation: mut operation,
        _binding: mut recovery,
        mut files,
        mut legacy,
    } = execution;
    let groups = manifests(&files);
    if groups.is_empty() {
        return RestoreExecution {
            report,
            _operation: operation,
            _binding: recovery,
            files,
            legacy,
        };
    }
    let root = Arc::clone(operation.as_read().journal_root());
    let mut discovered = BTreeMap::new();
    let mut stopped = files.iter().any(held);
    for group in groups {
        if stopped {
            for index in &group {
                files[*index].reason = Some("restore_held");
            }
            continue;
        }
        let selection = match selection(&root, &mut discovered, &files, &group) {
            Ok(selection) => selection,
            Err(reason) => {
                for index in &group {
                    files[*index].reason = Some(reason);
                }
                continue;
            }
        };
        let review = preview::prepare(operation, recovery, selection);
        let approval = review
            .candidate()
            .map(|candidate| (candidate.adoption_id.clone(), candidate.revision.clone()));
        let review = match approval {
            Some((adoption_id, revision)) => {
                match review.approve_inner(&adoption_id, &revision, time) {
                    Ok(restored) => {
                        let LegacyRestoreExecution { inner, evidence } = restored;
                        operation = inner._operation;
                        recovery = inner._binding;
                        legacy.push(evidence);
                        merge(&mut files, &group, inner.files);
                        stopped = files.iter().any(held);
                        continue;
                    }
                    Err(review) => *review,
                }
            }
            None => review,
        };
        let LegacyRestoreReview {
            operation: returned,
            recovery_binding,
            destination,
            candidate,
            probes,
            probe_owner,
            binding_write,
            refusal,
            ..
        } = review;
        operation = returned;
        recovery = recovery_binding;
        let refusal = refusal.unwrap_or_else(|| refuse("legacy_review_incomplete"));
        report
            .reason_detail
            .get_or_insert_with(|| refusal.reason.to_owned());
        for index in &group {
            files[*index].reason = Some(owner_reason(refusal.reason));
        }
        if probe_owner.is_some() || binding_write.is_some() {
            files[group[0]].owner = FileOwner::Review(Box::new(ReviewRemnant {
                refusal,
                probes,
                probe_owner,
                binding_write,
                _candidate: candidate,
                _destination: destination,
            }));
            stopped = true;
        }
    }
    summarize(&mut report, &files);
    RestoreExecution {
        report,
        _operation: operation,
        _binding: recovery,
        files,
        legacy,
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use super::*;

    fn record(stream: &str, segment: &str, manifest: &str, bound: bool) -> FileRecord {
        let mut witness = crate::test_restore_intent::fixture(false).witness;
        witness.manifest_id = manifest.into();
        if !bound {
            witness.repository_id = None;
        }
        FileRecord {
            detail: 0,
            day: "20261009".into(),
            stream: stream.into(),
            segment: segment.into(),
            witness,
            disposition: RestoreDisposition::Unattempted,
            reason: None,
            files: 0,
            bytes: 0,
            owner: FileOwner::None,
        }
    }

    #[test]
    fn only_unattempted_unbound_files_group_by_segment_and_manifest_in_order() {
        let files = vec![
            record("_default", "120000_60", "v1:20261009:1", false),
            record("_default", "120000_60", "bound", true),
            record("_default", "130000_60", "v1:20261009:2", false),
            record("_default", "120000_60", "v1:20261009:1", false),
            record("other", "120000_60", "v1:20261009:1", false),
        ];
        assert_eq!(manifests(&files), vec![vec![0, 3], vec![2], vec![4]]);
    }

    #[test]
    fn historical_destination_is_the_one_admissible_directory_with_that_identity() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let day = temp.path().join("chronicle/20261009");
        fs::create_dir_all(day.join("120000_60_fixture")).unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let files = vec![
            record("_default", "120000_60", "v1:20261009:1", false),
            record("_default", "130000_60", "v1:20261009:2", false),
            record("other", "120000_60", "v1:20261009:3", false),
        ];
        let resolve = |group: &[usize]| selection(&root, &mut BTreeMap::new(), &files, group);

        let selected = resolve(&[0]).unwrap_or_else(|reason| panic!("{reason}"));
        assert_eq!(selected.target.day, "20261009");
        assert_eq!(selected.target.stream, "_default");
        assert_eq!(selected.target.dir, "120000_60_fixture");
        assert_eq!(selected.layout, SegmentLayout::Direct);
        assert_eq!(selected.manifest_id, "v1:20261009:1");
        assert_eq!(selected.names, vec![files[0].witness.file.name.clone()]);
        assert_eq!(resolve(&[1]).err(), Some("segment_missing"));
        assert_eq!(resolve(&[2]).err(), Some("segment_missing"));

        // An owner-deleted segment is refused before any read, and left as found.
        let tombstone = day.join("120000_60_fixture/tombstone.json");
        fs::write(&tombstone, b"{}").unwrap();
        assert_eq!(resolve(&[0]).err(), Some("destination_admission_failed"));
        assert_eq!(fs::read(&tombstone).unwrap(), b"{}");
        assert_eq!(
            fs::read_dir(day.join("120000_60_fixture")).unwrap().count(),
            1
        );
        fs::remove_file(&tombstone).unwrap();

        // Two directories with the same stream and key are never guessed between.
        fs::create_dir(day.join("120000_60_copy")).unwrap();
        assert_eq!(resolve(&[0]).err(), Some("segment_identity"));
    }

    #[test]
    fn refused_probe_is_removed_by_identity_unless_its_writer_is_uncertain() {
        use solstone_core_journal_io::activity::ActivityLease;
        use solstone_core_journal_io::media::{MediaDigest, RootActivityCarrier};
        use std::ffi::OsStr;
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let day = temp.path().join("chronicle/20261009");
        fs::create_dir_all(day.join("120000_60_fixture")).unwrap();
        fs::create_dir(temp.path().join("source")).unwrap();
        fs::write(temp.path().join("source/raw.webm"), b"backup bytes").unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let segment = iter_segments(root.canonical_path(), PathOrDay::Day("20261009"))
            .unwrap()
            .remove(0);
        let destination = PreparedRestore::admit(Arc::clone(&root), "20261009", &segment)
            .unwrap_or_else(|error| panic!("{:?}", error.reason));
        let carrier = Arc::new(
            RootActivityCarrier::prepare(ActivityLease::try_acquire(Arc::clone(&root)).unwrap())
                .unwrap_or_else(|error| panic!("{:?}", error.error())),
        );
        let source = MediaDirectory::open(Arc::clone(&root), Path::new("source")).unwrap();
        let stage = || {
            MediaStage::create(destination.stage_parent(), Arc::clone(carrier.activity()))
                .unwrap_or_else(|failure| panic!("{}", failure.error))
        };
        let scratch = || {
            fs::read_dir(&day)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".tmp-offload-")
                })
                .count()
        };
        let record = || ProbeRecord {
            name: "raw.webm".into(),
            capacity: None,
            digest: None,
            cleanup: None,
        };
        let matching = MediaDigest {
            size: 12,
            sha256: format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(b"backup bytes")
            ),
        };
        let mismatched = MediaDigest {
            size: 12,
            sha256: "0".repeat(64),
        };

        // Finished writes, matching or not, and an unlaunched stage leave nothing behind.
        let mut owners = vec![ProbeOwner::Stage(stage())];
        let Err(failure) = stage()
            .create_target()
            .unwrap_or_else(|failure| panic!("{}", failure.error))
            .copy_from(&source, OsStr::new("raw.webm"), &mismatched)
        else {
            panic!("a copy that doesn't match its record is refused");
        };
        owners.push(ProbeOwner::ReadyTarget(failure.owned));
        let prepared = stage()
            .create_target()
            .unwrap_or_else(|failure| panic!("{}", failure.error))
            .copy_from(&source, OsStr::new("raw.webm"), &matching)
            .unwrap_or_else(|failure| panic!("{}", failure.error));
        owners.push(ProbeOwner::Prepared(prepared));
        assert_eq!(scratch(), 3);
        for probe in owners {
            let mut owner = Some(probe);
            let mut probes = vec![record()];
            release_probe(&mut owner, &mut probes);
            assert!(owner.is_none());
            assert_eq!(probes[0].cleanup, Some(Cleanup::Complete));
        }
        assert_eq!(scratch(), 0);

        // A target already handed to a writer is never removed here.
        let mut target = stage()
            .create_target()
            .unwrap_or_else(|failure| panic!("{}", failure.error));
        #[cfg(unix)]
        drop(target.helper_transport().unwrap());
        #[cfg(windows)]
        drop(target.issue_stage_file().unwrap());
        let mut owner = Some(ProbeOwner::ReadyTarget(target));
        let mut probes = vec![record()];
        release_probe(&mut owner, &mut probes);
        assert!(owner.is_some());
        assert_eq!(probes[0].cleanup, Some(Cleanup::Refused));
        assert_eq!(scratch(), 1);
        assert_eq!(
            fs::read(temp.path().join("source/raw.webm")).unwrap(),
            b"backup bytes"
        );
    }

    #[test]
    fn owner_reasons_keep_a_small_vocabulary() {
        assert_eq!(
            owner_reason("legacy_insufficient_free_space"),
            "insufficient_free_space"
        );
        assert_eq!(
            owner_reason("legacy_verification_failed"),
            "legacy_verification_failed"
        );
        assert_eq!(owner_reason("legacy_read_refused"), "legacy_read_failed");
        assert_eq!(owner_reason("legacy_mark_changed"), "legacy_review_held");
    }
}
