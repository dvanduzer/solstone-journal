// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Stays the journal works out from readings.
//!
//! A phone that reports visits already says where it stopped. Between those
//! reports, a run of precise readings that stays put is a stay too, and this
//! module finds it with one bounded, inspectable rule:
//!
//! - only readings with a reported accuracy of 100 m or better count, so
//!   approximate location (Android, a Watch, an iPhone with precise location
//!   off) never becomes a stay;
//! - a stay starts at a reading (its anchor) and takes every following reading
//!   within [`STAY_RADIUS_M`] of it, as long as no two readings are more than
//!   [`STAY_MAX_GAP_SECONDS`] apart;
//! - it needs at least [`STAY_MIN_READINGS`] readings across at least
//!   [`STAY_MIN_SECONDS`];
//! - it ends at its last reading, never at the next reading somewhere else.
//!
//! The iPhone's per-segment "interrupted" flag is not used here: it also
//! fires when accuracy is limited, so it says nothing about coverage.

use crate::pack::distance_m;

/// How far from its first reading a stay may wander.
pub const STAY_RADIUS_M: f64 = 100.0;
/// The longest silence inside a stay.
pub const STAY_MAX_GAP_SECONDS: i64 = 5 * 60;
/// The shortest stay.
pub const STAY_MIN_SECONDS: i64 = 5 * 60;
/// The fewest readings a stay rests on.
pub const STAY_MIN_READINGS: usize = 3;
/// Readings less accurate than this never support a stay.
pub const STAY_MAX_ACCURACY_M: f64 = 100.0;

/// One reading offered to the detector, in time order per device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub seconds: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy_m: Option<f64>,
}

/// A stay: which samples it rests on, its centre and its margin.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Indexes into the samples given, in order.
    pub members: Vec<usize>,
    pub latitude: f64,
    pub longitude: f64,
    /// The circle that holds every supporting reading with its own margin,
    /// so a name for the stay is checked against all of them.
    pub margin_m: f64,
}

fn qualifies(sample: &Sample) -> bool {
    sample.accuracy_m.is_some_and(|accuracy| {
        accuracy.is_finite() && (0.0..=STAY_MAX_ACCURACY_M).contains(&accuracy)
    })
}

/// Find the stays in one device's readings, which must be in time order.
pub fn find_stays(samples: &[Sample]) -> Vec<Found> {
    let mut stays = Vec::new();
    let mut start = 0;
    while start < samples.len() {
        if !qualifies(&samples[start]) {
            start += 1;
            continue;
        }
        let anchor = samples[start];
        let mut members = vec![start];
        let mut last_time = anchor.seconds;
        let mut next = start + 1;
        while next < samples.len() {
            let sample = &samples[next];
            if sample.seconds - last_time > STAY_MAX_GAP_SECONDS {
                break;
            }
            if !qualifies(sample) {
                // A coarse reading neither extends nor breaks a stay.
                next += 1;
                continue;
            }
            if distance_m(
                anchor.latitude,
                anchor.longitude,
                sample.latitude,
                sample.longitude,
            ) > STAY_RADIUS_M
            {
                break;
            }
            members.push(next);
            last_time = sample.seconds;
            next += 1;
        }
        let span = samples[*members.last().unwrap_or(&start)].seconds - anchor.seconds;
        if members.len() >= STAY_MIN_READINGS && span >= STAY_MIN_SECONDS {
            let count = members.len() as f64;
            let latitude = members.iter().map(|&i| samples[i].latitude).sum::<f64>() / count;
            let longitude = members.iter().map(|&i| samples[i].longitude).sum::<f64>() / count;
            let margin_m = members
                .iter()
                .map(|&i| {
                    let sample = &samples[i];
                    distance_m(latitude, longitude, sample.latitude, sample.longitude)
                        + sample.accuracy_m.unwrap_or(0.0)
                })
                .fold(0.0, f64::max);
            start = members.last().copied().unwrap_or(start) + 1;
            stays.push(Found {
                members,
                latitude,
                longitude,
                margin_m,
            });
        } else {
            start += 1;
        }
    }
    stays
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAT: f64 = 39.7527;
    const LON: f64 = -105.0003;

    fn at(seconds: i64, east_m: f64, accuracy: f64) -> Sample {
        Sample {
            seconds,
            latitude: LAT,
            longitude: LON + east_m / (111_195.0 * LAT.to_radians().cos()),
            accuracy_m: Some(accuracy),
        }
    }

    #[test]
    fn readings_that_stay_put_for_five_minutes_are_a_stay() {
        let samples: Vec<Sample> = (0..8)
            .map(|i| at(i * 60, (i % 3) as f64 * 5.0, 10.0))
            .collect();
        let stays = find_stays(&samples);
        assert_eq!(stays.len(), 1);
        assert_eq!(stays[0].members.len(), 8);
        assert!(stays[0].margin_m >= 10.0 && stays[0].margin_m < 25.0);
    }

    #[test]
    fn moving_readings_are_not_a_stay() {
        let samples: Vec<Sample> = (0..20).map(|i| at(i * 30, i as f64 * 40.0, 8.0)).collect();
        assert!(find_stays(&samples).is_empty());
    }

    #[test]
    fn a_short_pause_is_not_a_stay() {
        let samples: Vec<Sample> = (0..5).map(|i| at(i * 50, 0.0, 8.0)).collect();
        assert!(find_stays(&samples).is_empty(), "four minutes");
    }

    #[test]
    fn approximate_readings_never_become_a_stay() {
        let samples: Vec<Sample> = (0..30).map(|i| at(i * 60, 0.0, 2000.0)).collect();
        assert!(find_stays(&samples).is_empty());
        let unknown: Vec<Sample> = (0..30)
            .map(|i| Sample {
                accuracy_m: None,
                ..at(i * 60, 0.0, 0.0)
            })
            .collect();
        assert!(find_stays(&unknown).is_empty());
    }

    #[test]
    fn a_long_silence_ends_a_stay_at_its_last_reading() {
        let mut samples: Vec<Sample> = (0..6).map(|i| at(i * 60, 0.0, 8.0)).collect();
        samples.push(at(3600, 0.0, 8.0));
        let stays = find_stays(&samples);
        assert_eq!(stays.len(), 1);
        assert_eq!(stays[0].members, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn two_places_in_a_day_are_two_stays() {
        let mut samples: Vec<Sample> = (0..6).map(|i| at(i * 60, 0.0, 8.0)).collect();
        samples.extend((0..6).map(|i| at(600 + i * 60, 800.0, 8.0)));
        assert_eq!(find_stays(&samples).len(), 2);
    }
}
