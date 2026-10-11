// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;
use std::path::{Path, PathBuf};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use super::*;

const IPHONE_HEADER: &str = r#"{"accuracy":"full","fix_count":2,"gap":false,"kind":"location","platform":"ios","schema":"solstone.location.segment/1","source":"location","tier":"full"}"#;

fn fix(t: &str, lat: f64, lon: f64) -> String {
    format!(
        r#"{{"alt":null,"course":null,"h_acc":10,"lat":{lat},"lon":{lon},"schema":"solstone.location.fix/1","speed":null,"stationary":false,"t":"{t}","v_acc":null}}"#
    )
}

fn write(root: &Path, day: &str, stream: &str, segment: &str, text: &str) {
    let dir = root.join("chronicle").join(day).join(stream).join(segment);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("location.jsonl"), text).unwrap();
}

/// A journal whose only capture is location: two days from an iPhone near
/// Golden and Denver, nothing else at all.
fn location_only_journal() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "20261001",
        "phone_location",
        "140000_300",
        &format!(
            "{IPHONE_HEADER}\n{}\n{}\n",
            fix("2026-10-01T20:00:10Z", 39.7555, -105.2211),
            fix("2026-10-01T20:04:10Z", 39.7556, -105.2212)
        ),
    );
    write(
        root.path(),
        "20261003",
        "phone_location",
        "090000_300",
        &format!(
            "{IPHONE_HEADER}\n{}\n{}\n",
            r#"{"arrival":"2026-10-03T14:58:00Z","departure":null,"h_acc":40,"lat":39.7527,"lon":-105.0001,"schema":"solstone.location.visit/1"}"#,
            fix("2026-10-03T15:00:00Z", 39.7527, -105.0001)
        ),
    );
    let mut reconciler = Reconciler::default();
    let report = solstone_core_location::reconcile_journal(root.path(), &mut reconciler).unwrap();
    assert_eq!(report.written, 2, "{report:?}");
    root
}

fn app(root: PathBuf) -> Router {
    router(root, || Response::new(Body::from("shell")))
}

async fn get(root: &Path, uri: &str) -> (StatusCode, Option<String>, Value) {
    let response = app(root.to_path_buf())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        location,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn every_location_day_is_reachable_in_a_location_only_journal() {
    let journal = location_only_journal();
    let (status, _, index) = get(journal.path(), "/app/map/api/index").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(index["coverage"]["start"], "20261001");
    assert_eq!(index["coverage"]["end"], "20261003");
    let (_, _, month) = get(journal.path(), "/app/map/api/stats/202610").await;
    let days: Vec<&String> = month.as_object().unwrap().keys().collect();
    assert_eq!(days, ["20261001", "20261003"]);
    let (status, location, _) = get(journal.path(), "/app/map/").await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(location.as_deref(), Some("/app/map/20261003"));
}

#[tokio::test]
async fn a_day_lists_its_records_and_draws_the_same_ones() {
    let journal = location_only_journal();
    let (status, _, day) = get(journal.path(), "/app/map/api/day/20261003").await;
    assert_eq!(status, StatusCode::OK);
    let entries = day["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["kind"], "visit");
    assert_eq!(entries[0]["area"]["label"], "near Denver, Colorado");
    assert_eq!(entries[0]["departure"], Value::Null);
    assert_eq!(entries[1]["kind"], "readings");
    assert_eq!(
        entries[1]["segments"][0]["url"],
        "/app/transcripts/20261003?stream=phone_location#090000_300"
    );
    let map = &day["map"];
    assert!(
        map["towns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|town| town["name"] == "Denver")
    );
    // Denver sits well inside Colorado, so no state line crosses this view.
    assert!(map["regions"].as_array().unwrap().is_empty());
    assert_eq!(map["bounds"].as_array().unwrap().len(), 4);
    assert_eq!(day["nearby"]["previous"], "20261001");
    assert_eq!(day["nearby"]["next"], Value::Null);
}

#[tokio::test]
async fn an_empty_day_says_so_and_points_at_the_nearest_days() {
    let journal = location_only_journal();
    let (status, _, day) = get(journal.path(), "/app/map/api/day/20261002").await;
    assert_eq!(status, StatusCode::OK);
    assert!(day["entries"].as_array().unwrap().is_empty());
    assert_eq!(day["map"], Value::Null);
    assert_eq!(day["journal_has_location"], true);
    assert_eq!(day["nearby"]["previous"], "20261001");
    assert_eq!(day["nearby"]["next"], "20261003");
}

#[tokio::test]
async fn a_journal_with_no_location_serves_the_app_and_an_empty_index() {
    let root = tempfile::tempdir().unwrap();
    let (status, _, index) = get(root.path(), "/app/map/api/index").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(index["coverage"], Value::Null);
    let (status, location, _) = get(root.path(), "/app/map/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(location, None);
}

#[tokio::test]
async fn bad_days_and_months_are_refused() {
    let root = tempfile::tempdir().unwrap();
    for uri in [
        "/app/map/api/day/2026",
        "/app/map/api/day/20261341",
        "/app/map/20261341",
    ] {
        let (status, _, body) = get(root.path(), uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body["reason_code"], "invalid_day");
    }
    let (status, _, body) = get(root.path(), "/app/map/api/stats/202613").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason_code"], "invalid_month");
}

#[tokio::test]
async fn a_deleted_segment_leaves_the_day_view_at_once() {
    let journal = location_only_journal();
    fs::remove_dir_all(
        journal
            .path()
            .join("chronicle/20261001/phone_location/140000_300"),
    )
    .unwrap();
    let (_, _, day) = get(journal.path(), "/app/map/api/day/20261001").await;
    assert!(day["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_accuracy_choice_is_offered_and_removal_is_safe_when_nothing_is_installed() {
    let journal = location_only_journal();
    let (status, _, accuracy) = get(journal.path(), "/app/map/api/accuracy").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accuracy["installed"], Value::Null);
    assert_eq!(accuracy["current"], false);
    assert!(accuracy["offered"]["size_bytes"].as_u64().unwrap() > 0);
    assert!(
        accuracy["offered"]["files"]
            .as_array()
            .unwrap()
            .contains(&Value::from("venues.slpk"))
    );
    let (_, _, day) = get(journal.path(), "/app/map/api/day/20261003").await;
    assert_eq!(day["accuracy"]["offered"], accuracy["offered"]);
    let response = app(journal.path().to_path_buf())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/app/map/api/accuracy/remove")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
