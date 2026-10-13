// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Pure dismissal replay over provenance admitted by each caller.

use serde_json::{Value, json};
use std::collections::BTreeSet;

// Layout is part of identity even when the stream name is the same.
pub(super) type DiscoveryMember = (String, String, String, String, String, i64);

/// Fold connected components using half-of-the-smaller-event overlap.
pub(super) fn fold_dismissal_members(
    events: &[BTreeSet<DiscoveryMember>],
) -> Vec<BTreeSet<DiscoveryMember>> {
    let mut folded = Vec::new();
    let mut seen = vec![false; events.len()];
    for start in 0..events.len() {
        if seen[start] {
            continue;
        }
        let mut stack = vec![start];
        let mut component = BTreeSet::new();
        seen[start] = true;
        while let Some(current) = stack.pop() {
            component.extend(events[current].iter().cloned());
            for next in 0..events.len() {
                if !seen[next] && overlap_ratio_at_least_half(&events[current], &events[next]) {
                    seen[next] = true;
                    stack.push(next);
                }
            }
        }
        folded.push(component);
    }
    folded
}

pub(super) fn dismissal_suppressed(
    candidate: &BTreeSet<DiscoveryMember>,
    dismissals: &[BTreeSet<DiscoveryMember>],
) -> bool {
    !candidate.is_empty()
        && dismissals
            .iter()
            .any(|dismissal| candidate.intersection(dismissal).count() * 2 >= candidate.len())
}

fn overlap_ratio_at_least_half(
    left: &BTreeSet<DiscoveryMember>,
    right: &BTreeSet<DiscoveryMember>,
) -> bool {
    let denominator = left.len().min(right.len());
    denominator != 0 && left.intersection(right).count() * 2 >= denominator
}

pub(super) fn member_set(members: &[Value]) -> Result<BTreeSet<DiscoveryMember>, String> {
    canonical_members(members).map(|members| {
        members
            .into_iter()
            .map(|member| {
                (
                    member["day"].as_str().expect("canonical day").to_owned(),
                    member["stream_layout"]
                        .as_str()
                        .expect("canonical stream layout")
                        .to_owned(),
                    member["stream"]
                        .as_str()
                        .expect("canonical stream")
                        .to_owned(),
                    member["segment_key"]
                        .as_str()
                        .expect("canonical segment key")
                        .to_owned(),
                    member["source"]
                        .as_str()
                        .expect("canonical source")
                        .to_owned(),
                    member["sentence_id"]
                        .as_i64()
                        .expect("canonical sentence id"),
                )
            })
            .collect()
    })
}

pub(super) fn canonical_members(members: &[Value]) -> Result<Vec<Value>, String> {
    let mut tuples = BTreeSet::new();
    for member in members {
        let object = member
            .as_object()
            .ok_or_else(|| "invalid cluster member provenance".to_owned())?;
        let field = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "invalid cluster member provenance".to_owned())
        };
        let sentence_id = object
            .get("sentence_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| "invalid cluster member provenance".to_owned())?;
        let stream_layout = object
            .get("stream_layout")
            .map(|_| field("stream_layout"))
            .transpose()?
            .unwrap_or_else(|| "named".to_owned());
        if !matches!(stream_layout.as_str(), "direct" | "named") {
            return Err("invalid cluster member provenance".to_owned());
        }
        tuples.insert((
            field("day")?,
            stream_layout,
            field("stream")?,
            field("segment_key")?,
            field("source")?,
            sentence_id,
        ));
    }
    Ok(tuples.into_iter().map(|(day, stream_layout, stream, segment_key, source, sentence_id)| json!({"day":day,"stream_layout":stream_layout,"stream":stream,"segment_key":segment_key,"source":source,"sentence_id":sentence_id})).collect())
}

#[cfg(test)]
mod tests {
    use super::{DiscoveryMember, dismissal_suppressed, fold_dismissal_members, member_set};
    use serde_json::json;
    use std::collections::BTreeSet;

    fn members(ids: &[i64], layout: &str) -> BTreeSet<DiscoveryMember> {
        member_set(
            &ids.iter()
                .map(|id| json!({"day":"20260101","stream_layout":layout,"stream":"_default","segment_key":"090000_1","source":"audio","sentence_id":id}))
                .collect::<Vec<_>>(),
        )
        .expect("provenance")
    }

    #[test]
    fn connected_dismissals_suppress_at_half_of_unique_candidate_provenance() {
        let events = [
            members(&[3, 4], "named"),
            members(&[1, 2, 2], "named"),
            members(&[], "named"),
            members(&[2, 3], "named"),
        ];
        let candidate = members(&[1, 4, 5, 6, 6], "named");
        assert!(!dismissal_suppressed(&candidate, &events));
        let folded = fold_dismissal_members(&events);
        assert!(folded.contains(&members(&[1, 2, 3, 4], "named")));
        assert!(dismissal_suppressed(&candidate, &folded));
        assert!(!dismissal_suppressed(
            &members(&[1, 4, 5, 6, 7], "named"),
            &folded
        ));
        assert!(!dismissal_suppressed(&BTreeSet::new(), &folded));
        assert!(!dismissal_suppressed(&candidate, &[]));
    }

    #[test]
    fn layout_is_part_of_dismissal_identity() {
        let folded =
            fold_dismissal_members(&[members(&[1, 2], "direct"), members(&[2, 3], "named")]);
        assert_eq!(folded.len(), 2);
        assert!(!dismissal_suppressed(&members(&[3], "direct"), &folded));
        assert!(!dismissal_suppressed(&members(&[1], "named"), &folded));
    }

    #[test]
    fn components_use_event_overlap_rather_than_growing_union_overlap() {
        let events = [
            members(&[1, 2, 3, 4], "named"),
            members(&[3, 4, 5, 6], "named"),
            members(&[1, 5, 7, 8], "named"),
        ];
        let folded = fold_dismissal_members(&events);
        assert_eq!(folded.len(), 2);
        assert!(!dismissal_suppressed(
            &members(&[2, 6, 7, 8, 9], "named"),
            &folded
        ));
    }
}
