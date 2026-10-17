// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::error::Error;
use std::fmt;

use serde_json::{Map, Value};

pub const SPEAKER_TURN_EVIDENCE_VERSION: &str = "window-local-turns-v1";
pub const SENTENCE_TURN_INSET_S: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeakerTurnRange {
    pub start_s: f64,
    pub end_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerTurnEvidence {
    pub version: String,
    pub audio_duration_s: f64,
    pub ranges: Vec<SpeakerTurnRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeakerTurnError {
    Malformed { detail: String },
}

impl fmt::Display for SpeakerTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { detail } => formatter.write_str(detail),
        }
    }
}

impl Error for SpeakerTurnError {}

impl SpeakerTurnEvidence {
    pub fn new(audio_duration_s: f64, ranges: Vec<SpeakerTurnRange>) -> Self {
        Self {
            version: SPEAKER_TURN_EVIDENCE_VERSION.to_owned(),
            audio_duration_s,
            ranges,
        }
    }

    pub fn flags_sentence(&self, start_s: Option<f64>, end_s: Option<f64>) -> bool {
        let (Some(start), Some(end)) = (start_s, end_s) else {
            return false;
        };
        if !start.is_finite() || !end.is_finite() {
            return false;
        }
        self.ranges.iter().any(|range| {
            range.start_s >= start + SENTENCE_TURN_INSET_S
                && range.end_s <= end - SENTENCE_TURN_INSET_S
        })
    }

    pub fn to_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("version".to_owned(), Value::String(self.version.clone()));
        map.insert(
            "audio_duration_s".to_owned(),
            serde_json::Number::from_f64(self.audio_duration_s)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        );
        let ranges = self
            .ranges
            .iter()
            .map(|range| {
                let mut range_map = Map::new();
                range_map.insert(
                    "start_s".to_owned(),
                    serde_json::Number::from_f64(range.start_s)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                );
                range_map.insert(
                    "end_s".to_owned(),
                    serde_json::Number::from_f64(range.end_s)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                );
                Value::Object(range_map)
            })
            .collect();
        map.insert("ranges".to_owned(), Value::Array(ranges));
        map
    }

    pub fn to_value(&self) -> Value {
        Value::Object(self.to_map())
    }

    pub fn from_value(value: &Value) -> Result<Self, SpeakerTurnError> {
        let object = value
            .as_object()
            .ok_or_else(|| malformed("speaker turns evidence must be an object"))?;
        if object.len() != 3 {
            return Err(malformed("speaker turns evidence has unexpected keys"));
        }

        let version = object
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("version must be a string"))?;
        if version != SPEAKER_TURN_EVIDENCE_VERSION {
            return Err(malformed(format!(
                "expected version {SPEAKER_TURN_EVIDENCE_VERSION}, got {version}"
            )));
        }

        let audio_duration_s =
            parse_finite_f64(object.get("audio_duration_s"), "audio_duration_s")?;
        if audio_duration_s < 0.0 {
            return Err(malformed("audio_duration_s must be non-negative"));
        }

        let ranges_array = object
            .get("ranges")
            .and_then(Value::as_array)
            .ok_or_else(|| malformed("ranges must be an array"))?;

        let mut ranges: Vec<SpeakerTurnRange> = Vec::with_capacity(ranges_array.len());
        for (index, item) in ranges_array.iter().enumerate() {
            let range_obj = item
                .as_object()
                .ok_or_else(|| malformed(format!("range {index} must be an object")))?;
            if range_obj.len() != 2 {
                return Err(malformed(format!("range {index} has unexpected keys")));
            }

            let start_s = parse_finite_f64(range_obj.get("start_s"), "start_s")?;
            let end_s = parse_finite_f64(range_obj.get("end_s"), "end_s")?;

            if start_s < 0.0 {
                return Err(malformed(format!(
                    "range {index} start_s must be non-negative"
                )));
            }
            if start_s > end_s {
                return Err(malformed(format!(
                    "range {index} is inverted: start_s {start_s} > end_s {end_s}"
                )));
            }
            if end_s > audio_duration_s {
                return Err(malformed(format!(
                    "range {index} end_s {end_s} exceeds audio_duration_s {audio_duration_s}"
                )));
            }

            let range = SpeakerTurnRange { start_s, end_s };
            if let Some(prev) = ranges.last() {
                let cmp = prev
                    .start_s
                    .total_cmp(&range.start_s)
                    .then_with(|| prev.end_s.total_cmp(&range.end_s));
                if cmp == std::cmp::Ordering::Equal {
                    return Err(malformed(format!("range {index} is an exact duplicate")));
                }
                if cmp == std::cmp::Ordering::Greater {
                    return Err(malformed(format!(
                        "ranges must be sorted: range {index} is out of order"
                    )));
                }
            }
            ranges.push(range);
        }

        Ok(Self {
            version: version.to_owned(),
            audio_duration_s,
            ranges,
        })
    }
}

fn parse_finite_f64(value: Option<&Value>, field: &str) -> Result<f64, SpeakerTurnError> {
    let value = value.ok_or_else(|| malformed(format!("{field} is missing")))?;
    let number = value
        .as_f64()
        .ok_or_else(|| malformed(format!("{field} must be a finite number")))?;
    if !number.is_finite() {
        return Err(malformed(format!("{field} must be finite")));
    }
    Ok(number)
}

fn malformed(detail: impl Into<String>) -> SpeakerTurnError {
    SpeakerTurnError::Malformed {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_roundtrip_and_key_order() {
        let evidence = SpeakerTurnEvidence::new(
            15.5,
            vec![
                SpeakerTurnRange {
                    start_s: 1.0,
                    end_s: 2.0,
                },
                SpeakerTurnRange {
                    start_s: 3.5,
                    end_s: 3.5,
                },
            ],
        );

        let map = evidence.to_map();
        let keys: Vec<_> = map.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["version", "audio_duration_s", "ranges"]);

        let range0 = map["ranges"][0].as_object().unwrap();
        let r0_keys: Vec<_> = range0.keys().map(String::as_str).collect();
        assert_eq!(r0_keys, vec!["start_s", "end_s"]);

        let parsed = SpeakerTurnEvidence::from_value(&Value::Object(map)).unwrap();
        assert_eq!(parsed, evidence);
    }

    #[test]
    fn parser_rejects_unknown_top_level_key() {
        let mut val = SpeakerTurnEvidence::new(10.0, vec![]).to_value();
        val["extra"] = json!(123);
        assert!(SpeakerTurnEvidence::from_value(&val).is_err());
    }

    #[test]
    fn parser_rejects_unknown_range_key() {
        let val = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": 10.0,
            "ranges": [
                {"start_s": 1.0, "end_s": 2.0, "extra": true}
            ]
        });
        assert!(SpeakerTurnEvidence::from_value(&val).is_err());
    }

    #[test]
    fn parser_rejects_wrong_version() {
        let val = json!({
            "version": "windowed-slots-v1",
            "audio_duration_s": 10.0,
            "ranges": []
        });
        assert!(SpeakerTurnEvidence::from_value(&val).is_err());
    }

    #[test]
    fn parser_rejects_nulls_and_missing_keys() {
        let val = json!({
            "version": null,
            "audio_duration_s": 10.0,
            "ranges": []
        });
        assert!(SpeakerTurnEvidence::from_value(&val).is_err());

        let val = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "ranges": []
        });
        assert!(SpeakerTurnEvidence::from_value(&val).is_err());
    }

    #[test]
    fn parser_rejects_non_finite_number() {
        // String "1e309" or non-f64
        let val = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": "1e309",
            "ranges": []
        });
        assert!(SpeakerTurnEvidence::from_value(&val).is_err());
    }

    #[test]
    fn parser_rejects_negative_and_inverted_and_out_of_bounds() {
        let neg_dur = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": -1.0,
            "ranges": []
        });
        assert!(SpeakerTurnEvidence::from_value(&neg_dur).is_err());

        let neg_start = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": 10.0,
            "ranges": [{"start_s": -0.1, "end_s": 1.0}]
        });
        assert!(SpeakerTurnEvidence::from_value(&neg_start).is_err());

        let inverted = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": 10.0,
            "ranges": [{"start_s": 2.0, "end_s": 1.0}]
        });
        assert!(SpeakerTurnEvidence::from_value(&inverted).is_err());

        let out_of_bounds = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": 10.0,
            "ranges": [{"start_s": 2.0, "end_s": 10.1}]
        });
        assert!(SpeakerTurnEvidence::from_value(&out_of_bounds).is_err());
    }

    #[test]
    fn parser_rejects_unsorted_and_duplicate_ranges() {
        let unsorted = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": 10.0,
            "ranges": [
                {"start_s": 3.0, "end_s": 4.0},
                {"start_s": 1.0, "end_s": 2.0}
            ]
        });
        assert!(SpeakerTurnEvidence::from_value(&unsorted).is_err());

        let duplicate = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": 10.0,
            "ranges": [
                {"start_s": 1.0, "end_s": 2.0},
                {"start_s": 1.0, "end_s": 2.0}
            ]
        });
        assert!(SpeakerTurnEvidence::from_value(&duplicate).is_err());
    }

    #[test]
    fn parser_accepts_zero_width_and_overlapping_sorted() {
        let valid = json!({
            "version": SPEAKER_TURN_EVIDENCE_VERSION,
            "audio_duration_s": 10.0,
            "ranges": [
                {"start_s": 1.0, "end_s": 3.0},
                {"start_s": 2.0, "end_s": 2.0},
                {"start_s": 2.0, "end_s": 4.0}
            ]
        });
        assert!(SpeakerTurnEvidence::from_value(&valid).is_ok());
    }

    #[test]
    fn flags_sentence_inset_behavior() {
        let evidence = SpeakerTurnEvidence::new(
            20.0,
            vec![SpeakerTurnRange {
                start_s: 5.5,
                end_s: 6.5,
            }],
        );

        // Inset is 0.5: sentence must span at least [start, end] where start <= 5.0 and end >= 7.0
        assert!(evidence.flags_sentence(Some(5.0), Some(7.0)));
        assert!(evidence.flags_sentence(Some(0.0), Some(10.0)));

        // Violates start inset (5.5 < 5.1 + 0.5)
        assert!(!evidence.flags_sentence(Some(5.1), Some(7.0)));
        // Violates end inset (6.5 > 6.9 - 0.5)
        assert!(!evidence.flags_sentence(Some(5.0), Some(6.9)));

        // Missing or non-finite bounds
        assert!(!evidence.flags_sentence(None, Some(7.0)));
        assert!(!evidence.flags_sentence(Some(5.0), None));
        assert!(!evidence.flags_sentence(Some(f64::NAN), Some(7.0)));
        assert!(!evidence.flags_sentence(Some(5.0), Some(f64::INFINITY)));
    }
}
