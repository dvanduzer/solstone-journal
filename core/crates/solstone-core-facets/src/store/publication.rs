// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;

use solstone_core_indexer_store::{SavedPublicationOutcome, attempt_saved_publication};

/// Bring a saved facet day file into search now, not at the next daily scan.
///
/// The index writer lease is taken before entity and facet trust, and trust
/// locks are reentrant, so a caller may still hold trust here. The update then
/// waits until this thread's outermost trust guard drops, and a file written
/// several times under one guard is indexed once. A failed update leaves the
/// file to the daily scan.
pub(super) fn publish_saved(journal_root: &Path, saved: &Path) {
    let journal = journal_root.to_path_buf();
    let path = saved.to_path_buf();
    solstone_core_entity::after_entity_trust_released(journal_root, saved, move || {
        publish_now(&journal, &path);
    });
}

fn publish_now(journal_root: &Path, saved: &Path) {
    let attempt = attempt_saved_publication(journal_root, saved, |journal, path| {
        solstone_core_indexer_store::scan::rescan_file(journal, path).map_err(|e| e.to_string())
    });
    if attempt.outcome == SavedPublicationOutcome::Failed {
        log::warn!(
            target: "solstone::indexer",
            "saved facet file left for the daily scan path={} cause={}",
            attempt.path,
            attempt.cause.as_deref().unwrap_or("unknown")
        );
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        ActivityRecord, AppendOutcome, append_activity_record, create_facet,
        prepare_anticipation_batch, publish_anticipation_batch, save_detected_entity,
    };

    fn record(value: Value) -> ActivityRecord {
        value.as_object().unwrap().clone()
    }

    fn searchable(journal: &std::path::Path, path: &str, text: &str) -> bool {
        let conn = solstone_core_indexer_store::db::open_index(journal).unwrap();
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM chunks WHERE path=?1 AND content LIKE ?2)",
            [path, &format!("%{text}%")],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_saved_activity_record_is_searchable_at_once() {
        let root = tempfile::tempdir().unwrap();
        create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let appended = append_activity_record(
            root.path(),
            "work",
            "20260510",
            record(json!({"id": "a1", "activity": "meeting", "title": "Kestrel review"})),
        )
        .unwrap();
        assert!(matches!(appended, AppendOutcome::Written(_)));
        assert!(searchable(
            root.path(),
            "facets/work/activities/20260510.jsonl",
            "Kestrel review"
        ));
    }

    #[test]
    fn an_anticipated_batch_is_searchable_once_its_retry_succeeds() {
        let root = tempfile::tempdir().unwrap();
        create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let batch = prepare_anticipation_batch(
            root.path(),
            "work",
            "20260920",
            &[(
                record(json!({"id": "p1", "source": "anticipated", "title": "Osprey planning", "hidden": false})),
                vec![],
            )],
            "2026-09-14T10:00:00Z",
        )
        .unwrap();
        // The first attempt writes the file and then fails its receipt; the
        // retry finds the batch already written.
        assert!(
            publish_anticipation_batch(root.path(), &batch, true, || Err("interrupt".into()))
                .is_err()
        );
        let index = root.path().join("indexer");
        if index.exists() {
            std::fs::remove_dir_all(&index).unwrap();
        }
        assert!(!index.exists());
        // The daily writer publishes its batch under facet trust.
        let trust = crate::hold_facet_trust_lock(root.path()).unwrap();
        publish_anticipation_batch(root.path(), &batch, true, || Ok(())).unwrap();
        drop(trust);
        assert!(searchable(
            root.path(),
            "facets/work/activities/20260920.jsonl",
            "Osprey planning"
        ));
    }

    #[test]
    fn a_write_under_trust_reaches_search_when_the_trust_is_released() {
        let root = tempfile::tempdir().unwrap();
        create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let appended = append_activity_record(
            root.path(),
            "work",
            "20260510",
            record(json!({"id": "a1", "activity": "meeting", "title": "Heron sync"})),
        )
        .unwrap();
        assert!(matches!(appended, AppendOutcome::Written(_)));
        // A scan holds the writer lease, and the caller holds facet trust
        // around the write, as enrichment and the daily schedule do.
        let lease = solstone_core_journal_io::hold_lock(
            solstone_core_indexer_store::db::db_path(root.path()),
            solstone_core_journal_io::LockOptions::default(),
        )
        .unwrap();
        let trust = crate::hold_facet_trust_lock(root.path()).unwrap();
        let started = std::time::Instant::now();
        crate::update_activity_record(
            root.path(),
            "work",
            "20260510",
            "a1",
            &serde_json::Map::from_iter([("title".to_owned(), json!("Heron retro"))]),
            "test",
            "renamed",
            "2026-05-10T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let path = "facets/work/activities/20260510.jsonl";
        assert!(!searchable(root.path(), path, "Heron retro"));
        // The scan finishes, then the caller's guard drops: the update runs.
        drop(lease);
        drop(trust);
        assert!(searchable(root.path(), path, "Heron retro"));
    }

    #[test]
    fn a_release_never_waits_on_the_index_lease_while_holding_trust() {
        let root = tempfile::tempdir().unwrap();
        create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        let appended = append_activity_record(
            root.path(),
            "work",
            "20260510",
            record(json!({"id": "a1", "activity": "meeting", "title": "Plover sync"})),
        )
        .unwrap();
        assert!(matches!(appended, AppendOutcome::Written(_)));
        let trust = crate::hold_facet_trust_lock(root.path()).unwrap();
        // Another thread takes the index lease and then asks for trust, the
        // order reconciliation uses.
        let (lease_held, lease_is_held) = std::sync::mpsc::channel();
        let journal = root.path().to_path_buf();
        let reconcile = std::thread::spawn(move || {
            let lease = solstone_core_journal_io::hold_lock(
                solstone_core_indexer_store::db::db_path(&journal),
                solstone_core_journal_io::LockOptions::default(),
            )
            .unwrap();
            lease_held.send(()).unwrap();
            let trust = crate::hold_facet_trust_lock(&journal).unwrap();
            drop(trust);
            drop(lease);
        });
        lease_is_held.recv().unwrap();
        crate::update_activity_record(
            root.path(),
            "work",
            "20260510",
            "a1",
            &serde_json::Map::from_iter([("title".to_owned(), json!("Plover retro"))]),
            "test",
            "renamed",
            "2026-05-10T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        let started = std::time::Instant::now();
        drop(trust);
        reconcile.join().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(searchable(
            root.path(),
            "facets/work/activities/20260510.jsonl",
            "Plover retro"
        ));
    }

    #[test]
    fn a_saved_entity_detection_is_searchable_at_once() {
        let root = tempfile::tempdir().unwrap();
        create_facet(root.path(), "work", "Work", "", "", "", None).unwrap();
        save_detected_entity(
            root.path(),
            "work",
            "20260510",
            "Person",
            "Wren Calloway",
            "Joined the launch call.",
        )
        .unwrap();
        assert!(searchable(
            root.path(),
            "facets/work/entities/20260510.jsonl",
            "Wren Calloway"
        ));
    }
}
