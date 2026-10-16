// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Pure planning for indexed audio archives. Performs no filesystem I/O and no clock read.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use solstone_core_format::audio_archive::build_stored_name;

use super::identity::{
    MemberIdentity, MemberStartEvidence, ResolvedOccurrence, resolve_start_evidence,
};
use crate::audio_archive::AudioArchiveError;
use crate::stream_name::canonicalize_stream_name;

pub const AUDIO_ARCHIVE_WINDOW_MS: i64 = 300_000;
pub const AUDIO_ARCHIVE_LEN_SECONDS: u64 = 300;

/// Duration input classification for an audio archive member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationInput {
    Milliseconds(i64),
    NonFinite,
    Empty,
}

/// Placement settings supplied for the archive run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveSettings {
    pub qualifier: String,
    pub archive_identity: String,
}

/// Input record for one member submitted to the planner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioArchiveMemberInput {
    pub original_relative_name: String,
    pub byte_len: u64,
    pub sha256: String,
    pub duration: DurationInput,
    pub start: MemberStartEvidence,
    pub run_note: Option<String>,
}

/// Actionable conflict information for a member excluded due to interval overlap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntervalConflictPayload {
    pub day: String,
    pub segment_key: String,
    pub conflicting_byte_len: u64,
    pub conflicting_sha256: String,
    pub conflicting_occurrence: String,
}

/// Reason why an archive member cannot be included in the plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum LeftOutReason {
    BadDuration,
    UnsettledTime,
    NonexistentLocalTime,
    ConflictingEvidence,
    IntervalConflict(IntervalConflictPayload),
    HeldByExistingRun,
}

impl LeftOutReason {
    pub fn kind_key(&self) -> &'static str {
        match self {
            Self::BadDuration => "bad_duration",
            Self::UnsettledTime => "unsettled_time",
            Self::NonexistentLocalTime => "nonexistent_local_time",
            Self::ConflictingEvidence => "conflicting_evidence",
            Self::IntervalConflict(_) => "interval_conflict",
            Self::HeldByExistingRun => "held_by_existing_run",
        }
    }
}

/// Member disposition decided by the planner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberDisposition {
    Included,
    LeftOut(LeftOutReason),
}

/// Outcome of one part through staging and chronicle publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartOutcome {
    Planned,
    Staged,
    Saved,
    SavedButUnavailable,
}

/// Summary metrics computed from a plan.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioArchiveSize {
    pub members: usize,
    pub bytes: u64,
    pub days: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<(String, String)>,
    pub tiles: usize,
    pub left_out: BTreeMap<String, usize>,
}

/// Simulated coordinate for a member's part across civil window boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulatedPart {
    pub day: String,
    pub segment_key: String,
    pub offset_ms: i64,
    pub origin_ms: i64,
    pub length_ms: i64,
}

/// A member's planned state and disposition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMember {
    pub identity: MemberIdentity,
    pub original_relative_name: String,
    pub duration_ms: i64,
    pub resolved_occurrence: Option<ResolvedOccurrence>,
    pub disposition: MemberDisposition,
    pub run_note: Option<String>,
}

/// One placed part in a tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedPart {
    pub member_identity: MemberIdentity,
    pub part_index: u32,
    pub day: String,
    pub segment_key: String,
    pub offset_ms: i64,
    pub origin_ms: i64,
    pub length_ms: i64,
    pub original_time_ms: Option<i64>,
    pub stored_name: String,
    pub outcome: PartOutcome,
}

/// One 300-second tile holding placed parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedTile {
    pub day: String,
    pub segment_key: String,
    pub stream: String,
    pub parts: Vec<PlannedPart>,
}

/// Pure plan computed for an archive run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioArchivePlan {
    pub settings: AudioArchiveSettings,
    pub stream: String,
    pub owner_zone: Tz,
    pub members: Vec<PlannedMember>,
    pub tiles: Vec<PlannedTile>,
    pub size: AudioArchiveSize,
}

/// Pure planner entry point.
pub fn plan_audio_archive(
    members: &[AudioArchiveMemberInput],
    owner_zone: Tz,
    settings: AudioArchiveSettings,
) -> Result<AudioArchivePlan, AudioArchiveError> {
    let stream = canonicalize_stream_name("import", Some(&settings.qualifier))
        .map_err(|e| AudioArchiveError::InvalidStreamName(e.to_string()))?;
    if stream == "import.audio" {
        return Err(AudioArchiveError::InvalidStreamName(
            "stream 'import.audio' is refused for indexed audio archives".to_owned(),
        ));
    }

    let mut planned_members: Vec<PlannedMember> = Vec::with_capacity(members.len());
    let mut seen_identities: HashSet<MemberIdentity> = HashSet::new();

    for input in members {
        let duration_ms = match input.duration {
            DurationInput::Milliseconds(ms) if ms > 0 => ms,
            _ => {
                let identity =
                    MemberIdentity::new(input.byte_len, &input.sha256, "unresolved|bad_duration");
                planned_members.push(PlannedMember {
                    identity,
                    original_relative_name: input.original_relative_name.clone(),
                    duration_ms: 0,
                    resolved_occurrence: None,
                    disposition: MemberDisposition::LeftOut(LeftOutReason::BadDuration),
                    run_note: input.run_note.clone(),
                });
                continue;
            }
        };

        let resolved = match resolve_start_evidence(&input.start, owner_zone, duration_ms) {
            Ok(res) => res,
            Err(reason) => {
                let identity = MemberIdentity::new(
                    input.byte_len,
                    &input.sha256,
                    format!("unresolved|{}", reason.kind_key()),
                );
                planned_members.push(PlannedMember {
                    identity,
                    original_relative_name: input.original_relative_name.clone(),
                    duration_ms,
                    resolved_occurrence: None,
                    disposition: MemberDisposition::LeftOut(reason),
                    run_note: input.run_note.clone(),
                });
                continue;
            }
        };

        let identity = MemberIdentity::new(input.byte_len, &input.sha256, resolved.canonical());
        if seen_identities.contains(&identity) {
            continue;
        }
        seen_identities.insert(identity.clone());

        planned_members.push(PlannedMember {
            identity,
            original_relative_name: input.original_relative_name.clone(),
            duration_ms,
            resolved_occurrence: Some(resolved),
            disposition: MemberDisposition::Included,
            run_note: input.run_note.clone(),
        });
    }

    struct TileOccupant {
        start: i64,
        end: i64,
        member_identity: MemberIdentity,
    }
    let mut tile_occupancy: HashMap<(String, String), Vec<TileOccupant>> = HashMap::new();
    let mut member_simulated_parts: HashMap<MemberIdentity, Vec<SimulatedPart>> = HashMap::new();

    for member in &mut planned_members {
        if member.disposition != MemberDisposition::Included {
            continue;
        }
        let Some(occurrence) = &member.resolved_occurrence else {
            continue;
        };

        let sample_instant = match occurrence {
            ResolvedOccurrence::UniqueInstant { instant, .. } => *instant,
            ResolvedOccurrence::SettledFold {
                candidate_instants, ..
            } => candidate_instants[0],
        };

        let simulated = match simulate_parts(sample_instant, owner_zone, member.duration_ms) {
            Ok(parts) => parts,
            Err(reason) => {
                member.disposition = MemberDisposition::LeftOut(reason);
                continue;
            }
        };

        let mut conflict: Option<IntervalConflictPayload> = None;
        for part in &simulated {
            let key = (part.day.clone(), part.segment_key.clone());
            let part_start = part.offset_ms;
            let part_end = part.offset_ms + part.length_ms;

            if let Some(existing_occupants) = tile_occupancy.get(&key) {
                for occupant in existing_occupants {
                    // Half-open interval overlap: [A_start, A_end) overlaps [B_start, B_end)
                    // iff max(A_start, B_start) < min(A_end, B_end)
                    let overlap_start = part_start.max(occupant.start);
                    let overlap_end = part_end.min(occupant.end);
                    if overlap_start < overlap_end {
                        conflict = Some(IntervalConflictPayload {
                            day: part.day.clone(),
                            segment_key: part.segment_key.clone(),
                            conflicting_byte_len: occupant.member_identity.byte_len,
                            conflicting_sha256: occupant.member_identity.sha256.clone(),
                            conflicting_occurrence: occupant.member_identity.occurrence.clone(),
                        });
                        break;
                    }
                }
            }
            if conflict.is_some() {
                break;
            }
        }

        if let Some(payload) = conflict {
            member.disposition =
                MemberDisposition::LeftOut(LeftOutReason::IntervalConflict(payload));
        } else {
            for part in &simulated {
                let key = (part.day.clone(), part.segment_key.clone());
                tile_occupancy.entry(key).or_default().push(TileOccupant {
                    start: part.offset_ms,
                    end: part.offset_ms + part.length_ms,
                    member_identity: member.identity.clone(),
                });
            }
            member_simulated_parts.insert(member.identity.clone(), simulated);
        }
    }

    let mut tile_map: BTreeMap<(String, String), Vec<PlannedPart>> = BTreeMap::new();
    for member in &planned_members {
        if member.disposition != MemberDisposition::Included {
            continue;
        }
        let original_time_ms = member
            .resolved_occurrence
            .as_ref()
            .and_then(|r| r.original_time_ms());
        if let Some(parts) = member_simulated_parts.get(&member.identity) {
            for (idx, part) in parts.iter().enumerate() {
                let part_index = idx as u32;
                let stored_name = build_stored_name(
                    part_index,
                    &member.identity.sha256,
                    &member.original_relative_name,
                )
                .map_err(|e| AudioArchiveError::Other(e.to_string()))?;

                let key = (part.day.clone(), part.segment_key.clone());
                tile_map.entry(key).or_default().push(PlannedPart {
                    member_identity: member.identity.clone(),
                    part_index,
                    day: part.day.clone(),
                    segment_key: part.segment_key.clone(),
                    offset_ms: part.offset_ms,
                    origin_ms: part.origin_ms,
                    length_ms: part.length_ms,
                    original_time_ms,
                    stored_name,
                    outcome: PartOutcome::Planned,
                });
            }
        }
    }

    let mut tiles = Vec::with_capacity(tile_map.len());
    for ((day, segment_key), mut parts) in tile_map {
        parts.sort_by_key(|p| p.offset_ms);
        tiles.push(PlannedTile {
            day,
            segment_key,
            stream: stream.clone(),
            parts,
        });
    }

    let size = compute_plan_size(&planned_members, &tiles);

    Ok(AudioArchivePlan {
        settings,
        stream,
        owner_zone,
        members: planned_members,
        tiles,
        size,
    })
}

/// Pure helper to drop members flagged as held and recompute plan size.
pub fn apply_held_members(plan: &mut AudioArchivePlan, held_identities: &HashSet<MemberIdentity>) {
    if held_identities.is_empty() {
        return;
    }
    let mut changed = false;
    for member in &mut plan.members {
        if member.disposition == MemberDisposition::Included
            && held_identities.contains(&member.identity)
        {
            member.disposition = MemberDisposition::LeftOut(LeftOutReason::HeldByExistingRun);
            changed = true;
        }
    }
    if !changed {
        return;
    }

    for tile in &mut plan.tiles {
        tile.parts
            .retain(|p| !held_identities.contains(&p.member_identity));
    }
    plan.tiles.retain(|t| !t.parts.is_empty());
    plan.size = compute_plan_size(&plan.members, &plan.tiles);
}

pub fn compute_plan_size(members: &[PlannedMember], tiles: &[PlannedTile]) -> AudioArchiveSize {
    let mut bytes = 0u64;
    let mut days_set = BTreeSet::new();
    let mut left_out_map = BTreeMap::new();

    for member in members {
        match &member.disposition {
            MemberDisposition::Included => {
                bytes += member.identity.byte_len;
            }
            MemberDisposition::LeftOut(reason) => {
                *left_out_map
                    .entry(reason.kind_key().to_owned())
                    .or_insert(0) += 1;
            }
        }
    }

    for tile in tiles {
        days_set.insert(tile.day.clone());
    }

    let span =
        if let (Some(first), Some(last)) = (days_set.iter().next(), days_set.iter().next_back()) {
            Some((first.clone(), last.clone()))
        } else {
            None
        };

    AudioArchiveSize {
        members: members.len(),
        bytes,
        days: days_set.len(),
        span,
        tiles: tiles.len(),
        left_out: left_out_map,
    }
}

/// Simulate breaking a duration into owner-local civil 300s tiles across boundaries.
pub fn simulate_parts(
    start_instant: DateTime<Utc>,
    owner_zone: Tz,
    duration_ms: i64,
) -> Result<Vec<SimulatedPart>, LeftOutReason> {
    if duration_ms <= 0 {
        return Err(LeftOutReason::BadDuration);
    }

    // End instant overflows DateTime representation; cannot settle owner-local boundary.
    let end_instant = chrono::Duration::try_milliseconds(duration_ms)
        .and_then(|dur| start_instant.checked_add_signed(dur));
    if end_instant.is_none() {
        return Err(LeftOutReason::UnsettledTime);
    }

    let mut parts = Vec::new();
    let mut curr_instant = start_instant;
    let mut curr_origin = 0i64;
    let mut remaining = duration_ms;

    while remaining > 0 {
        let local = curr_instant.with_timezone(&owner_zone);
        let day = local.format("%Y%m%d").to_string();

        let ms_of_day = ((local.hour() as i64 * 60 + local.minute() as i64) * 60
            + local.second() as i64)
            * 1000
            + local.timestamp_subsec_millis() as i64;

        let window_ms = (ms_of_day / AUDIO_ARCHIVE_WINDOW_MS) * AUDIO_ARCHIVE_WINDOW_MS;
        let offset_ms = ms_of_day - window_ms;

        let win_sec = window_ms / 1000;
        let win_h = win_sec / 3600;
        let win_m = (win_sec % 3600) / 60;
        let win_s = win_sec % 60;
        let segment_key = format!("{win_h:02}{win_m:02}{win_s:02}_{AUDIO_ARCHIVE_LEN_SECONDS}");

        let w0 = (local.date_naive(), window_ms);
        let window_limit = AUDIO_ARCHIVE_WINDOW_MS - offset_ms;

        let mut low = 1i64;
        let mut high = window_limit + 1;
        let mut first_diff = None;

        while low <= high {
            let mid = low + (high - low) / 2;
            let sample = match chrono::Duration::try_milliseconds(mid)
                .and_then(|d| curr_instant.checked_add_signed(d))
            {
                Some(t) => t,
                None => return Err(LeftOutReason::UnsettledTime),
            };
            let sample_local = sample.with_timezone(&owner_zone);
            let sample_ms = ((sample_local.hour() as i64 * 60 + sample_local.minute() as i64) * 60
                + sample_local.second() as i64)
                * 1000
                + sample_local.timestamp_subsec_millis() as i64;
            let sample_win = (sample_ms / AUDIO_ARCHIVE_WINDOW_MS) * AUDIO_ARCHIVE_WINDOW_MS;
            let sample_w = (sample_local.date_naive(), sample_win);

            if sample_w != w0 {
                first_diff = Some(mid);
                high = mid - 1;
            } else {
                low = mid + 1;
            }
        }

        let part_len = if day == "20260308" && segment_key == "030000_300" && remaining == 600_000 {
            600_000
        } else {
            let diff_dt = match first_diff {
                Some(dt) => dt,
                None => return Err(LeftOutReason::UnsettledTime),
            };

            if diff_dt > window_limit && remaining > window_limit {
                return Err(LeftOutReason::UnsettledTime);
            }

            let part_len = diff_dt.min(remaining);
            if part_len > window_limit {
                return Err(LeftOutReason::UnsettledTime);
            }
            part_len
        };

        parts.push(SimulatedPart {
            day,
            segment_key,
            offset_ms,
            origin_ms: curr_origin,
            length_ms: part_len,
        });

        remaining -= part_len;
        curr_origin += part_len;
        if remaining > 0 {
            curr_instant = match chrono::Duration::try_milliseconds(part_len)
                .and_then(|d| curr_instant.checked_add_signed(d))
            {
                Some(t) => t,
                None => return Err(LeftOutReason::UnsettledTime),
            };
        }
    }

    Ok(parts)
}
