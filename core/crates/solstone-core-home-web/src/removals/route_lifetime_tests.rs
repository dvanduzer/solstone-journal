// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;

use solstone_core_backup_web::BackupRouteDrain;
use solstone_core_journal_io::JournalRoot;

use super::*;

struct ReleaseGuard(Option<std::sync::mpsc::Sender<()>>);

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn home_decline_with_lifetime_holds_drain() {
    let (dir, marks) = super::tests::review_fixture(1, 10);
    let root = Arc::new(JournalRoot::open(dir.path()).unwrap());
    let ids = vec![marks[0].id.clone()];
    let observed = super::read_review_context(&root, &ids).unwrap();
    let serialized = serde_json::to_string(&observed).unwrap();
    let request_body = json!({ "review_context": serialized, "mark_ids": ids });
    let body_bytes = Bytes::from(serde_json::to_vec(&request_body).unwrap());

    let deps = solstone_core_backup_web::BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();

    let other_dir = tempfile::tempdir().unwrap();
    let refused = decline(
        Some(Extension(deps.clone())),
        State(other_dir.path().to_path_buf()),
        Ok(body_bytes.clone()),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(deps.operations.lock().unwrap().writes_in_flight, 0);

    let _ = decline(
        None,
        State(dir.path().to_path_buf()),
        Ok(body_bytes.clone()),
    )
    .await;
    assert_eq!(deps.operations.lock().unwrap().writes_in_flight, 0);

    let (entered_rx, release_tx) = arm_decline_pause();
    let mut guard = ReleaseGuard(Some(release_tx));

    let decline_task = tokio::spawn(decline(
        Some(Extension(deps.clone())),
        State(dir.path().to_path_buf()),
        Ok(body_bytes),
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("entered decline pause");
    decline_task.abort();
    assert!(deps.operations.lock().unwrap().writes_in_flight > 0);
    let second = deps.lifetime();
    assert_eq!(
        lifetime.drain(join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert_eq!(
        second.drain(join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    lifetime.request_stop();
    assert_eq!(
        lifetime.drain(join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    let mark_id = marks[0].id.as_str().to_owned();
    let _ = guard.0.take().unwrap().send(());
    let register = dir.path().join("health/retention-marks.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(&register)
        .unwrap_or_default()
        .contains(&mark_id)
    {
        assert!(Instant::now() < deadline, "decline did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        lifetime.drain(join_executor(), Instant::now() + Duration::from_secs(5)),
        BackupRouteDrain::Drained
    );
    assert_eq!(deps.operations.lock().unwrap().writes_in_flight, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn home_recover_with_lifetime_holds_drain() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("health")).unwrap();

    let deps = solstone_core_backup_web::BackupWebDeps::new(dir.path().to_path_buf());
    let lifetime = deps.lifetime();
    let clock = crate::Clock::system();

    let other_dir = tempfile::tempdir().unwrap();
    let refused = recover(
        Some(Extension(deps.clone())),
        State(other_dir.path().to_path_buf()),
        Extension(clock.clone()),
        Bytes::new(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(deps.operations.lock().unwrap().writes_in_flight, 0);

    let _ = recover(
        None,
        State(dir.path().to_path_buf()),
        Extension(clock.clone()),
        Bytes::new(),
    )
    .await;
    assert_eq!(deps.operations.lock().unwrap().writes_in_flight, 0);
    // The unguarded call already appends. The held call's row is the one this test watches.
    let _ = fs::remove_dir_all(dir.path().join("config/actions"));

    let (entered_recover_rx, release_recover_tx) = arm_recover_pause();
    let (entered_append_rx, release_append_tx) = arm_recover_append_pause();

    let mut guard1 = ReleaseGuard(Some(release_recover_tx));
    let mut guard2 = ReleaseGuard(Some(release_append_tx));

    let recover_task = tokio::spawn(recover(
        Some(Extension(deps.clone())),
        State(dir.path().to_path_buf()),
        Extension(clock),
        Bytes::new(),
    ));

    entered_recover_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pause before retention::recover was not entered");
    recover_task.abort();
    assert!(deps.operations.lock().unwrap().writes_in_flight > 0);
    let second = deps.lifetime();
    assert_eq!(
        lifetime.drain(join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );
    assert_eq!(
        second.drain(join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    let _ = guard1.0.take().unwrap().send(());

    entered_append_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("retention::recover did not return before append_action");
    assert!(!removal_recover_logged(dir.path()));
    assert_eq!(
        lifetime.drain(join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    lifetime.request_stop();
    assert_eq!(
        lifetime.drain(join_executor(), Instant::now()),
        BackupRouteDrain::Pending
    );

    let _ = guard2.0.take().unwrap().send(());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !removal_recover_logged(dir.path()) {
        assert!(
            Instant::now() < deadline,
            "removal_recover was not recorded"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        lifetime.drain(join_executor(), Instant::now() + Duration::from_secs(5)),
        BackupRouteDrain::Drained
    );
    assert_eq!(deps.operations.lock().unwrap().writes_in_flight, 0);
}

fn removal_recover_logged(journal: &std::path::Path) -> bool {
    let Ok(entries) = fs::read_dir(journal.join("config/actions")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        fs::read_to_string(entry.path())
            .unwrap_or_default()
            .contains("removal_recover")
    })
}

fn join_executor() -> &'static tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
        })
        .handle()
}
