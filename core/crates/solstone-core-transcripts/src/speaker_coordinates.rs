// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Sentence coordinates shared by the owner view and the bounded agent projection.

use std::fs;
use std::path::Path;

use serde_json::Value;

/// Count statements from the start of each source, including non-rendered starts.
/// Stored sentence_id fields are not the coordinates used by local speaker labels.
pub fn next_statement_ordinal(line: &str, ordinal: &mut u64) -> Option<i64> {
    match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Object(value)) if value.contains_key("start") => {
            *ordinal += 1;
            i64::try_from(*ordinal).ok()
        }
        _ => None,
    }
}

fn audio_sources(dir: &Path) -> Vec<String> {
    let mut values = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.ends_with("audio.jsonl")
                .then(|| name.trim_end_matches(".jsonl").into())
        })
        .collect::<Vec<String>>();
    values.sort();
    values
}
/// Select the source whose sentence ordinals the segment-local labels address.
pub fn speaker_label_source(dir: &Path) -> (Option<String>, bool) {
    let audio = audio_sources(dir);
    let mut npz = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".npz")?;
            (stem == "audio" || stem.ends_with("_audio")).then(|| stem.into())
        })
        .collect::<Vec<String>>();
    npz.sort();
    if let Some(value) = npz.into_iter().next() {
        (Some(value), false)
    } else if audio.len() == 1 {
        (Some(audio[0].clone()), false)
    } else {
        (None, audio.len() > 1)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_start_bearing_objects_advance_the_statement_ordinal() {
        let mut ordinal = 0;
        for line in ["", "broken", "[]", "{\"raw\":\"audio.flac\"}"] {
            assert_eq!(super::next_statement_ordinal(line, &mut ordinal), None);
        }
        assert_eq!(
            super::next_statement_ordinal("{\"start\":null}", &mut ordinal),
            Some(1)
        );
        assert_eq!(
            super::next_statement_ordinal("{\"start\":1}", &mut ordinal),
            Some(2)
        );
        assert_eq!(
            super::next_statement_ordinal(
                "{\"start\":\"00:00:01\",\"sentence_id\":99}",
                &mut ordinal
            ),
            Some(3)
        );
    }
}
