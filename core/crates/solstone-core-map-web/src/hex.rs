// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A month of stays and visits, gathered into H3 hexagons.
//!
//! Cells are worked out here, on the journal, each time the page asks; nothing
//! is stored. A stay counts only where the owner can see it on the map, and
//! only at a cell size no finer than how well its position is known.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as RoutePath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use h3o::{CellIndex, LatLng, Resolution};
use serde_json::{Value, json};
use solstone_core_convey_http::envelope::error_envelope;
use solstone_core_convey_http::owner_read::{OwnerReadRole, spawn_blocking_response};
use solstone_core_location::Entry;
use solstone_core_location::accuracy::MARGIN_FLOOR_M;
use solstone_core_location::day::{LocalTime, Point};
use solstone_core_location::places::PlaceStore;

use solstone_core_location::Bounds;

use crate::{
    AppState, MAP_TOWNS, bounds_json, context_bounds, day_counts, invalid_month, round, town_json,
    valid_month,
};

/// The cell sizes offered: edges of about 530 m, 200 m and 76 m, so about
/// 920 m, 350 m and 130 m across.
const RESOLUTIONS: [Resolution; 3] = [Resolution::Eight, Resolution::Nine, Resolution::Ten];

#[derive(Default)]
struct Cell {
    /// Each stay's time as (start, end) in Unix seconds. Two phones often
    /// report the same stay, so a cell's time is the union of these, not the sum.
    spans: Vec<(i64, i64)>,
    stays: u32,
    days: BTreeSet<String>,
}

impl Cell {
    /// Seconds covered by at least one stay, overlaps counted once.
    fn covered_seconds(&self) -> i64 {
        let mut spans = self.spans.clone();
        spans.sort_unstable();
        let mut total = 0;
        let mut current: Option<(i64, i64)> = None;
        for (start, end) in spans {
            current = match current {
                Some((open, close)) if start <= close => Some((open, close.max(end))),
                Some((open, close)) => {
                    total += close - open;
                    Some((start, end))
                }
                None => Some((start, end)),
            };
        }
        total + current.map_or(0, |(open, close)| close - open)
    }
}

#[derive(Default)]
struct LeftOut {
    hidden: u32,
    coarse: u32,
}

fn resolution(text: &str) -> Option<Resolution> {
    let number: u8 = text.parse().ok()?;
    RESOLUTIONS.into_iter().find(|res| u8::from(*res) == number)
}

fn invalid_resolution() -> Response {
    error_envelope(
        "invalid_resolution",
        "that cell size couldn't be used.",
        "Resolution must be 8, 9 or 10",
        StatusCode::BAD_REQUEST,
    )
    .into_response()
}

/// A stay's time, or `None` when the phone never said when it ended.
fn span(start: Option<&LocalTime>, end: Option<&LocalTime>) -> Option<(i64, i64)> {
    match (start, end) {
        (Some(start), Some(end)) if end.utc > start.utc => {
            Some((start.utc.timestamp(), end.utc.timestamp()))
        }
        _ => None,
    }
}

/// Add one stay or visit to its cell, or count why it was left out.
fn gather(
    cells: &mut BTreeMap<CellIndex, Cell>,
    left_out: &mut LeftOut,
    store: &PlaceStore,
    res: Resolution,
    day: &str,
    point: &Point,
    time: Option<(i64, i64)>,
) {
    if store.hides(point.latitude, point.longitude) {
        left_out.hidden += 1;
        return;
    }
    // A position known no better than the cell is wide would pick a cell at
    // random. Unlike venue words, a coarse cell can still use a wide margin, so
    // this takes the reported accuracy rather than the capped margin.
    let margin = point
        .accuracy_m
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.max(MARGIN_FLOOR_M));
    match margin {
        Some(margin) if margin <= res.edge_length_m() => {}
        _ => {
            left_out.coarse += 1;
            return;
        }
    }
    let Ok(at) = LatLng::new(point.latitude, point.longitude) else {
        left_out.coarse += 1;
        return;
    };
    let cell = cells.entry(at.to_cell(res)).or_default();
    cell.spans.extend(time);
    cell.stays += 1;
    cell.days.insert(day.to_owned());
}

fn cell_json(index: &CellIndex, cell: &Cell) -> Value {
    json!({
        "cell": index.to_string(),
        "hours": (cell.covered_seconds() as f64 / 36.0).round() / 100.0,
        "stays": cell.stays,
        "days": cell.days.len(),
        "outline": index
            .boundary()
            .iter()
            .map(|vertex| json!([round(vertex.lat()), round(vertex.lng())]))
            .collect::<Vec<_>>(),
    })
}

/// The box around every cell drawn, with the towns around it, or null.
fn map_json(cells: &BTreeMap<CellIndex, Cell>) -> Value {
    let mut bounds: Option<Bounds> = None;
    for vertex in cells
        .keys()
        .flat_map(|index| index.boundary().iter().copied().collect::<Vec<_>>())
    {
        let (lat, lon) = (vertex.lat(), vertex.lng());
        bounds = Some(match bounds {
            None => Bounds {
                west: lon,
                south: lat,
                east: lon,
                north: lat,
            },
            Some(b) => Bounds {
                west: b.west.min(lon),
                south: b.south.min(lat),
                east: b.east.max(lon),
                north: b.north.max(lat),
            },
        });
    }
    let Some(bounds) = bounds else {
        return Value::Null;
    };
    let towns = solstone_core_location::area_tier()
        .map(|tier| {
            tier.towns_in(context_bounds(bounds), MAP_TOWNS)
                .iter()
                .map(town_json)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({"bounds": bounds_json(&bounds), "towns": towns})
}

/// `/app/map/api/hex/{month}/{res}`: every cell the month's stays fall in.
pub(crate) async fn month(
    State(state): State<Arc<AppState>>,
    RoutePath((month, res)): RoutePath<(String, String)>,
) -> Response {
    if !valid_month(&month) {
        return invalid_month();
    }
    let Some(res) = resolution(&res) else {
        return invalid_resolution();
    };
    let journal_root = state.journal_root.clone();
    spawn_blocking_response(OwnerReadRole::MapHex, move || {
        let store = PlaceStore::read(&journal_root);
        let mut cells = BTreeMap::new();
        let mut left_out = LeftOut::default();
        let days: Vec<String> = day_counts(&journal_root)
            .into_keys()
            .filter(|day| day.starts_with(&month))
            .collect();
        for day in &days {
            let view = match solstone_core_location::read_journal_day(&journal_root, day) {
                Ok(view) => view,
                Err(error) => {
                    return error_envelope(
                        "area_names_unavailable",
                        "the place names that come with your journal couldn't be read.",
                        error.to_string(),
                        StatusCode::SERVICE_UNAVAILABLE,
                    )
                    .into_response();
                }
            };
            for entry in &view.entries {
                let (point, time) = match entry {
                    Entry::Stay {
                        point, first, last, ..
                    } => (point, span(Some(first), Some(last))),
                    Entry::Visit {
                        point,
                        arrival,
                        departure,
                        ..
                    } => (point, span(arrival.as_ref(), departure.as_ref())),
                    Entry::Readings { .. } | Entry::Unreadable { .. } => continue,
                };
                gather(&mut cells, &mut left_out, &store, res, day, point, time);
            }
        }
        Json(json!({
            "month": month,
            "res": u8::from(res),
            "edge_m": res.edge_length_m().round(),
            "cells": cells.iter().map(|(index, cell)| cell_json(index, cell)).collect::<Vec<_>>(),
            "map": map_json(&cells),
            "left_out": {"hidden": left_out.hidden, "coarse": left_out.coarse},
        }))
        .into_response()
    })
    .await
}
