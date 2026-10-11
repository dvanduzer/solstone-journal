// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! The owner's "location accuracy" choice: what is offered, what is
//! installed, and one download or removal at a time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use solstone_core_convey_http::envelope::error_envelope;
use solstone_core_convey_http::owner_read::{OwnerReadRole, spawn_blocking_response};
use solstone_core_location::accuracy::{installed_packs, read_installed};
use solstone_core_location::install;

use crate::AppState;

#[derive(Debug, Clone, PartialEq)]
enum Job {
    Idle,
    Downloading { received: u64, total: u64 },
    Failed { reason_code: &'static str },
}

static JOB: Mutex<Job> = Mutex::new(Job::Idle);

fn job() -> Job {
    JOB.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn set_job(next: Job) {
    *JOB.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
}

/// Everything the map app shows about the choice.
pub(crate) fn status(journal: &Path) -> Value {
    let offered = install::offered();
    let installed = read_installed(journal);
    let packs = installed.as_ref().and_then(|_| installed_packs(journal));
    let job = match job() {
        Job::Idle => json!({"state": "idle"}),
        Job::Downloading { received, total } => {
            json!({"state": "downloading", "received": received, "total": total})
        }
        Job::Failed { reason_code } => json!({"state": "failed", "reason_code": reason_code}),
    };
    json!({
        "offered": offered.as_ref().map(|offered| json!({
            "version": offered.version,
            "size_bytes": offered.size_bytes(),
            "files": offered.files.iter().map(|file| file.file.clone()).collect::<Vec<_>>(),
        })),
        "installed": installed.as_ref().map(|installed| json!({
            "version": installed.version,
            "size_bytes": installed.size_bytes(),
            "venues": packs.as_ref().is_some_and(|packs| packs.has_venues()),
            "containment": packs.as_ref().is_some_and(|packs| packs.has_containment()),
        })),
        "current": match (&offered, &installed) {
            (Some(offered), Some(installed)) => offered.is_installed(installed),
            _ => false,
        },
        "attributions": packs.as_ref().map(|packs| packs.attributions()).unwrap_or_default(),
        "notices": packs.as_ref().map(|packs| packs.notices()).unwrap_or_default(),
        "job": job,
    })
}

pub(crate) async fn get(State(state): State<Arc<AppState>>) -> Response {
    let journal_root = state.journal_root.clone();
    spawn_blocking_response(OwnerReadRole::MapAccuracy, move || {
        Json(status(&journal_root)).into_response()
    })
    .await
}

fn busy() -> Response {
    error_envelope(
        "accuracy_busy",
        "location accuracy is already downloading.",
        "A download is running",
        StatusCode::CONFLICT,
    )
    .into_response()
}

fn run_download(journal_root: PathBuf) {
    let result = install::install(&journal_root, |received, total| {
        set_job(Job::Downloading { received, total });
    });
    match result {
        Ok(_) => set_job(Job::Idle),
        Err(error) => {
            log::warn!("location accuracy download: {error}");
            set_job(Job::Failed {
                reason_code: error.reason_code(),
            });
        }
    }
}

pub(crate) async fn download(State(state): State<Arc<AppState>>) -> Response {
    {
        let mut current = JOB
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*current, Job::Downloading { .. }) {
            return busy();
        }
        if install::offered().is_none() {
            return error_envelope(
                "accuracy_not_offered",
                "this version of your journal has no location accuracy download.",
                "Nothing offered",
                StatusCode::NOT_FOUND,
            )
            .into_response();
        }
        *current = Job::Downloading {
            received: 0,
            total: install::offered().map_or(0, |offered| offered.size_bytes()),
        };
    }
    let journal_root = state.journal_root.clone();
    let spawned = std::thread::Builder::new()
        .name("location-accuracy".to_owned())
        .spawn(move || run_download(journal_root));
    if spawned.is_err() {
        set_job(Job::Failed {
            reason_code: "download_failed",
        });
    }
    (StatusCode::ACCEPTED, Json(status(&state.journal_root))).into_response()
}

pub(crate) async fn remove(State(state): State<Arc<AppState>>) -> Response {
    if matches!(job(), Job::Downloading { .. }) {
        return busy();
    }
    // Removing is two unlinks and a directory; it runs inline.
    match install::remove(&state.journal_root) {
        Ok(()) => {
            set_job(Job::Idle);
            Json(status(&state.journal_root)).into_response()
        }
        Err(error) => error_envelope(
            "accuracy_remove_failed",
            "location accuracy couldn't be removed.",
            error.to_string(),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response(),
    }
}
