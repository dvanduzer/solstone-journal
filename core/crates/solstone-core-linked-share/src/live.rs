// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Whether a captured message timestamp falls inside an activity.

/// Minutes allowed before the first segment starts.
const LEAD_SECONDS: u32 = 10 * 60;
/// Minutes allowed after the last segment ends.
const TRAIL_SECONDS: u32 = 5 * 60;
/// Extra minutes a relative stamp ("12m") may reach past the activity length.
const RELATIVE_SLACK_MINUTES: u32 = 10;

/// An activity's span in seconds of its day, from its segment names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityWindow {
    start: u32,
    end: u32,
}

impl ActivityWindow {
    /// The span from the earliest segment start to the latest segment end.
    pub fn from_segments<S: AsRef<str>>(segments: &[S]) -> Option<Self> {
        let mut window: Option<Self> = None;
        for segment in segments {
            let Some((times, end)) =
                solstone_core_format::segment::segment_start_and_end_seconds(segment.as_ref())
            else {
                continue;
            };
            let start = u32::from(times.hour) * 3_600
                + u32::from(times.minute) * 60
                + u32::from(times.second);
            let end = u32::try_from(end).unwrap_or(u32::MAX);
            window = Some(match window {
                Some(existing) => Self {
                    start: existing.start.min(start),
                    end: existing.end.max(end),
                },
                None => Self { start, end },
            });
        }
        window
    }

    /// Whether the visible stamp places the message inside the activity:
    /// "now"; a relative "Nm" that, counted back from when the frame was
    /// captured, lands inside the padded window (without a capture time, no
    /// longer ago than the activity lasted, plus slack); or a clock time within
    /// the padded window. Weekdays, "Yesterday" and dates are history.
    pub fn is_live(&self, timestamp: &str, captured_at: Option<u32>) -> bool {
        let stamp = timestamp.trim().to_lowercase();
        if stamp == "now" || stamp == "just now" {
            return true;
        }
        if let Some(minutes) = relative_minutes(&stamp) {
            return match captured_at {
                Some(captured) => {
                    let sent = captured.saturating_sub(minutes * 60);
                    sent + LEAD_SECONDS >= self.start && sent <= self.end + TRAIL_SECONDS
                }
                None => {
                    let length = (self.end - self.start).div_ceil(60);
                    minutes <= length + RELATIVE_SLACK_MINUTES
                }
            };
        }
        let Some(minute) = clock_minute(&stamp) else {
            return false;
        };
        let first = self.start.saturating_sub(LEAD_SECONDS) / 60;
        let last = (self.end + TRAIL_SECONDS).div_ceil(60);
        (first..=last).contains(&minute)
    }
}

fn relative_minutes(stamp: &str) -> Option<u32> {
    let stamp = stamp.strip_suffix(" ago").unwrap_or(stamp);
    let digits = stamp
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let unit = stamp[digits.len()..].trim_start();
    matches!(unit, "m" | "min" | "mins")
        .then(|| digits.parse().ok())
        .flatten()
}

/// Minute of the day for "H:MM", "HH:MM" or "H:MM AM/PM".
fn clock_minute(stamp: &str) -> Option<u32> {
    let (clock, meridiem) = match stamp.strip_suffix("am") {
        Some(clock) => (clock.trim_end(), Some(false)),
        None => match stamp.strip_suffix("pm") {
            Some(clock) => (clock.trim_end(), Some(true)),
            None => (stamp, None),
        },
    };
    let (hour, minute) = clock.split_once(':')?;
    if hour.is_empty()
        || hour.len() > 2
        || minute.len() != 2
        || !hour
            .bytes()
            .chain(minute.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut hour = hour.parse::<u32>().ok()?;
    let minute = minute.parse::<u32>().ok()?;
    if minute > 59 {
        return None;
    }
    match meridiem {
        Some(pm) => {
            if !(1..=12).contains(&hour) {
                return None;
            }
            hour = match (pm, hour) {
                (false, 12) => 0,
                (true, 12) => 12,
                (true, hour) => hour + 12,
                (false, hour) => hour,
            };
        }
        None if hour > 23 => return None,
        None => {}
    }
    Some(hour * 60 + minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 15:00:00 to 15:10:00.
    fn window() -> ActivityWindow {
        ActivityWindow::from_segments(&["150500_300", "150000_300"]).expect("window")
    }

    #[test]
    fn window_spans_every_segment() {
        assert_eq!(
            window(),
            ActivityWindow {
                start: 15 * 3_600,
                end: 15 * 3_600 + 600
            }
        );
        assert_eq!(ActivityWindow::from_segments(&["nonsense"]), None);
    }

    #[test]
    fn now_and_recent_relative_stamps_are_live() {
        let window = window();
        assert!(window.is_live("Now", None));
        assert!(window.is_live("now", None));
        assert!(window.is_live("12m", None));
        assert!(window.is_live("20 min", None));
        assert!(!window.is_live("21m", None));
        assert!(!window.is_live("3h", None));
    }

    #[test]
    fn clock_times_inside_the_padded_window_are_live() {
        let window = window();
        assert!(window.is_live("3:06 PM", None));
        assert!(window.is_live("15:06", None));
        assert!(window.is_live("2:50 pm", None));
        assert!(window.is_live("15:15", None));
        assert!(!window.is_live("2:49 PM", None));
        assert!(!window.is_live("15:16", None));
        assert!(!window.is_live("3:06 AM", None));
        assert!(!window.is_live("3:06", None));
    }

    #[test]
    fn weekdays_and_dates_are_history() {
        let window = window();
        for stamp in ["Tuesday", "Yesterday", "Oct 12", "10/12/26", "", "25:00"] {
            assert!(!window.is_live(stamp, None), "{stamp}");
        }
    }

    #[test]
    fn relative_stamps_are_counted_back_from_the_capture() {
        // 15:00:00 to 15:05:00, captured at 15:04.
        let window = ActivityWindow::from_segments(&["150000_300"]).unwrap();
        let captured = Some(15 * 3_600 + 4 * 60);
        assert!(window.is_live("12m", captured));
        // Sent at 14:40, before the window and its ten-minute lead.
        assert!(!window.is_live("24m", captured));
        // Without a capture time the activity length bounds it.
        assert!(!window.is_live("24m", None));
        assert!(window.is_live("Now", captured));
    }
}
