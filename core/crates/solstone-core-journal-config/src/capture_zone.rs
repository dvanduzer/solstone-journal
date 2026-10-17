// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Capture zone for a segment, measured against the journal owner's clock.
//!
//! A segment's wall clock reflects the recording device's own zone. When
//! reported, this module calculates the capture zone against the owner's
//! configured timezone.

use chrono::{NaiveDateTime, Offset, TimeDelta, TimeZone};
use chrono_tz::Tz;

use crate::zone::zone_label;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureZone {
    /// Canonical IANA name after it parses. `None` when the device sent no
    /// name or a name that did not parse.
    pub tz: Option<String>,
    pub utc_offset_seconds: i32,
    pub label: String,
    pub differs: bool,
}

pub fn capture_zone_view(
    day: &str,
    key: &str,
    reported_tz: Option<&str>,
    reported_offset_seconds: Option<i32>,
    owner: &Tz,
) -> Option<CaptureZone> {
    if reported_tz.is_none() && reported_offset_seconds.is_none() {
        return None;
    }
    let wall = segment_wall_start(day, key)?;
    let parsed_zone = reported_tz
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|name| name.parse::<Tz>().ok());
    let offset = reported_offset_seconds.or_else(|| {
        parsed_zone
            .and_then(|zone| zone.offset_from_local_datetime(&wall).earliest())
            .map(|offset| offset.fix().local_minus_utc())
    })?;
    let start = wall - TimeDelta::seconds(i64::from(offset));
    let host_offset = owner
        .offset_from_utc_datetime(&start)
        .fix()
        .local_minus_utc();
    Some(CaptureZone {
        tz: parsed_zone.map(|zone| zone.name().to_owned()),
        utc_offset_seconds: offset,
        label: zone_label(parsed_zone, offset),
        differs: host_offset != offset,
    })
}

fn segment_wall_start(day: &str, key: &str) -> Option<NaiveDateTime> {
    let hhmmss = key.get(..6)?;
    let digits =
        |text: &str, len: usize| text.len() == len && text.bytes().all(|b| b.is_ascii_digit());
    if !digits(day, 8) || !digits(hhmmss, 6) {
        return None;
    }
    NaiveDateTime::parse_from_str(&format!("{day}{hhmmss}"), "%Y%m%d%H%M%S").ok()
}

#[cfg(test)]
mod tests {
    use chrono_tz::Tz;

    use super::{CaptureZone, capture_zone_view};

    #[test]
    fn a_phone_in_tokyo_differs_from_a_journal_in_denver() {
        assert_eq!(
            capture_zone_view(
                "20260929",
                "211400_300",
                Some("Asia/Tokyo"),
                Some(32400),
                &Tz::America__Denver,
            ),
            Some(CaptureZone {
                tz: Some("Asia/Tokyo".to_owned()),
                utc_offset_seconds: 32400,
                label: "Tokyo".to_owned(),
                differs: true,
            })
        );
    }

    #[test]
    fn the_same_zone_as_the_journal_does_not_differ() {
        let view = capture_zone_view(
            "20260929",
            "090000_300",
            Some("America/Denver"),
            Some(-21600),
            &Tz::America__Denver,
        )
        .expect("view is present");
        assert!(!view.differs);
        assert_eq!(view.label, "Denver");
    }

    #[test]
    fn the_offset_alone_is_enough_and_is_compared_at_the_segment_start() {
        let summer = capture_zone_view(
            "20260715",
            "120000_300",
            None,
            Some(-21600),
            &Tz::America__Denver,
        )
        .expect("summer view is present");
        assert!(!summer.differs);
        assert_eq!(summer.label, "UTC-6");

        let winter = capture_zone_view(
            "20260115",
            "120000_300",
            None,
            Some(-21600),
            &Tz::America__Denver,
        )
        .expect("winter view is present");
        assert!(winter.differs);
    }

    #[test]
    fn a_zone_name_alone_fills_in_the_offset_for_that_day() {
        let view = capture_zone_view(
            "20260115",
            "083000_300",
            Some("Asia/Kolkata"),
            None,
            &Tz::America__Denver,
        )
        .expect("view is present");
        assert_eq!(view.utc_offset_seconds, 19800);
        assert_eq!(view.label, "Kolkata");
        assert!(view.differs);
    }

    #[test]
    fn places_read_as_places_and_placeless_zones_as_offsets() {
        let label = |tz: &str, offset: i32| {
            capture_zone_view("20260929", "120000_300", Some(tz), Some(offset), &Tz::UTC)
                .expect("view is present")
                .label
        };
        assert_eq!(
            label("America/Argentina/Buenos_Aires", -10800),
            "Buenos Aires"
        );
        assert_eq!(label("UTC", 0), "UTC");
        assert_eq!(label("Etc/GMT-9", 32400), "UTC+9");

        let unparsed = capture_zone_view(
            "20260929",
            "120000_300",
            Some("Not/A_Zone"),
            Some(-12600),
            &Tz::UTC,
        )
        .expect("view is present");
        assert_eq!(unparsed.tz, None);
        assert_eq!(unparsed.label, "UTC-3:30");
    }

    #[test]
    fn unusable_is_none() {
        assert_eq!(
            capture_zone_view("20260929", "120000_300", None, None, &Tz::UTC),
            None
        );
        assert_eq!(
            capture_zone_view("20260929", "120000_300", Some("Not/A_Zone"), None, &Tz::UTC),
            None
        );
        assert_eq!(
            capture_zone_view("2026092", "120000_300", None, Some(0), &Tz::UTC),
            None
        );
    }

    #[test]
    fn place_name_is_kept_when_explicit_offset_disagrees_with_zone() {
        let view = capture_zone_view(
            "20260929",
            "120000_300",
            Some("Asia/Tokyo"),
            Some(-12600),
            &Tz::UTC,
        )
        .expect("view is present");
        assert_eq!(view.tz, Some("Asia/Tokyo".to_owned()));
        assert_eq!(view.utc_offset_seconds, -12600);
        assert_eq!(view.label, "Tokyo");
    }
}
