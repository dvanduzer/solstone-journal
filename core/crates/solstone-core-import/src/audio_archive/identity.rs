// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Canonical occurrence resolution and member identity.

use chrono::{DateTime, Datelike, LocalResult, NaiveDateTime, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::plan::LeftOutReason;

/// Starting time evidence provided for an audio archive member.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberStartEvidence {
    /// RFC3339 timestamp with explicit offset ('Z' or '±HH:MM').
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instant: Option<String>,
    /// Civil local time without offset (e.g. 'YYYY-MM-DDTHH:MM:SS' or 'YYYY-MM-DDTHH:MM:SS.mmm').
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_civil: Option<String>,
    /// IANA time zone identifier for `local_civil` (e.g. 'America/New_York').
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_zone: Option<String>,
}

/// Resolved occurrence of a member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedOccurrence {
    /// Occurrence settled to a single UTC instant.
    UniqueInstant {
        instant: DateTime<Utc>,
        canonical: String,
    },
    /// Occurrence settled across an ambiguous fold to a stable civil time.
    SettledFold {
        candidate_instants: Vec<DateTime<Utc>>,
        canonical: String,
    },
}

impl ResolvedOccurrence {
    pub fn canonical(&self) -> &str {
        match self {
            Self::UniqueInstant { canonical, .. } => canonical,
            Self::SettledFold { canonical, .. } => canonical,
        }
    }

    pub fn original_time_ms(&self) -> Option<i64> {
        match self {
            Self::UniqueInstant { instant, .. } => Some(instant.timestamp_millis()),
            Self::SettledFold { .. } => None,
        }
    }
}

/// Member identity used for claim-index lookup and deduplication.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MemberIdentity {
    pub byte_len: u64,
    pub sha256: String,
    pub occurrence: String,
}

impl MemberIdentity {
    pub fn new(byte_len: u64, sha256: impl Into<String>, occurrence: impl Into<String>) -> Self {
        Self {
            byte_len,
            sha256: sha256.into().to_ascii_lowercase(),
            occurrence: occurrence.into(),
        }
    }
}

/// Attempt to resolve member start evidence into a unique instant or candidate instants.
pub fn resolve_start_evidence(
    start: &MemberStartEvidence,
    owner_zone: Tz,
    duration_ms: i64,
) -> Result<ResolvedOccurrence, LeftOutReason> {
    let parsed_instant = match &start.instant {
        Some(s) => Some(parse_rfc3339_offset_instant(s)?),
        None => None,
    };

    let (parsed_civil, parsed_zone) = match (&start.local_civil, &start.source_zone) {
        (Some(civil_str), Some(zone_str)) => {
            let zone: Tz = zone_str.parse().map_err(|_| LeftOutReason::UnsettledTime)?;
            let civil = parse_civil_datetime(civil_str, zone)?;
            (Some(civil), Some(zone))
        }
        (Some(_), None) => return Err(LeftOutReason::UnsettledTime),
        (None, Some(_)) => return Err(LeftOutReason::UnsettledTime),
        (None, None) => (None, None),
    };

    match (parsed_instant, parsed_civil, parsed_zone) {
        (None, None, None) => Err(LeftOutReason::UnsettledTime),
        (Some(instant), None, None) => {
            let canonical = format!("utc|{}", instant.timestamp_millis());
            Ok(ResolvedOccurrence::UniqueInstant { instant, canonical })
        }
        (None, Some(civil), Some(zone)) => {
            resolve_civil_in_zone(civil, zone, owner_zone, duration_ms)
        }
        (Some(instant), Some(civil), Some(zone)) => {
            let local_in_source = instant.with_timezone(&zone);
            let matches_civil = local_in_source.year() == civil.year()
                && local_in_source.month() == civil.month()
                && local_in_source.day() == civil.day()
                && local_in_source.hour() == civil.hour()
                && local_in_source.minute() == civil.minute()
                && local_in_source.second() == civil.second()
                && local_in_source.timestamp_subsec_millis()
                    == civil.and_utc().timestamp_subsec_millis();

            if !matches_civil {
                return Err(LeftOutReason::ConflictingEvidence);
            }

            match zone.from_local_datetime(&civil) {
                LocalResult::None => Err(LeftOutReason::NonexistentLocalTime),
                LocalResult::Single(_) => {
                    let canonical = format!("utc|{}", instant.timestamp_millis());
                    Ok(ResolvedOccurrence::UniqueInstant { instant, canonical })
                }
                LocalResult::Ambiguous(inst1, inst2) => {
                    let c1 = inst1.with_timezone(&Utc);
                    let c2 = inst2.with_timezone(&Utc);
                    if instant == c1 || instant == c2 {
                        let canonical = format!("utc|{}", instant.timestamp_millis());
                        Ok(ResolvedOccurrence::UniqueInstant { instant, canonical })
                    } else {
                        Err(LeftOutReason::ConflictingEvidence)
                    }
                }
            }
        }
        _ => Err(LeftOutReason::UnsettledTime),
    }
}

fn parse_rfc3339_offset_instant(s: &str) -> Result<DateTime<Utc>, LeftOutReason> {
    let trimmed = s.trim();
    let Some((_date, time_part)) = trimmed.split_once(['T', 't', ' ']) else {
        return Err(LeftOutReason::UnsettledTime);
    };
    if !time_part.contains('Z')
        && !time_part.contains('z')
        && !time_part.contains('+')
        && !time_part.contains('-')
    {
        return Err(LeftOutReason::UnsettledTime);
    }
    DateTime::parse_from_rfc3339(trimmed)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| LeftOutReason::UnsettledTime)
}

fn parse_civil_datetime(s: &str, zone: Tz) -> Result<NaiveDateTime, LeftOutReason> {
    let trimmed = s.trim();
    // If the string carries its own offset, it must not disagree with the source zone
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        let expected_offset = zone.offset_from_utc_datetime(&dt.naive_utc());
        if dt.offset().fix().local_minus_utc() != expected_offset.fix().local_minus_utc() {
            return Err(LeftOutReason::ConflictingEvidence);
        }
        return Ok(dt.naive_local());
    }

    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f") {
        return Ok(dt);
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f") {
        return Ok(dt);
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt);
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Ok(dt);
    }
    Err(LeftOutReason::UnsettledTime)
}

fn resolve_civil_in_zone(
    civil: NaiveDateTime,
    zone: Tz,
    owner_zone: Tz,
    duration_ms: i64,
) -> Result<ResolvedOccurrence, LeftOutReason> {
    match zone.from_local_datetime(&civil) {
        LocalResult::None => Err(LeftOutReason::NonexistentLocalTime),
        LocalResult::Single(inst) => {
            let instant = inst.with_timezone(&Utc);
            let canonical = format!("utc|{}", instant.timestamp_millis());
            Ok(ResolvedOccurrence::UniqueInstant { instant, canonical })
        }
        LocalResult::Ambiguous(inst1, inst2) => {
            let cand1 = inst1.with_timezone(&Utc);
            let cand2 = inst2.with_timezone(&Utc);

            let parts1 = super::plan::simulate_parts(cand1, owner_zone, duration_ms);
            let parts2 = super::plan::simulate_parts(cand2, owner_zone, duration_ms);

            match (parts1, parts2) {
                (Ok(p1), Ok(p2)) if p1 == p2 => {
                    let sub_ms = civil.and_utc().timestamp_subsec_millis();
                    let formatted = if sub_ms == 0 {
                        civil.format("%Y-%m-%dT%H:%M:%S.000").to_string()
                    } else {
                        format!("{}.{:03}", civil.format("%Y-%m-%dT%H:%M:%S"), sub_ms)
                    };
                    let canonical = format!("local|{}|{}", zone.name(), formatted);
                    Ok(ResolvedOccurrence::SettledFold {
                        candidate_instants: vec![cand1, cand2],
                        canonical,
                    })
                }
                _ => Err(LeftOutReason::UnsettledTime),
            }
        }
    }
}
