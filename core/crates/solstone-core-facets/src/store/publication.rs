// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::path::Path;

use solstone_core_indexer_store::{SavedPublicationOutcome, attempt_saved_publication};

/// Bring a saved facet day file into search now, not at the next daily scan.
///
/// Call only after the store's own locks are released: reconciliation takes
/// the index writer lease before entity and facet trust. A failed update
/// leaves the file to the daily scan, and health counts it as pending.
pub(super) fn publish_saved(journal_root: &Path, saved: &Path) {
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
        let _ = std::fs::remove_dir_all(root.path().join("indexer"));
        publish_anticipation_batch(root.path(), &batch, true, || Ok(())).unwrap();
        assert!(searchable(
            root.path(),
            "facets/work/activities/20260920.jsonl",
            "Osprey planning"
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
