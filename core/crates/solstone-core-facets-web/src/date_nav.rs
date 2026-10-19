// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeMap;

use serde_json::{Value, json};

// Deliberate local copy: sibling ports keep date-nav helpers private. The grid helper
// accepts an optional coverage override so ports can share its three payload keys.
pub fn date_nav_index(counts: &BTreeMap<String, usize>) -> Value {
    let mut months = BTreeMap::<String, usize>::new();
    let days = counts
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(day, count)| {
            *months.entry(day[..6].to_owned()).or_default() += count;
            day.clone()
        })
        .collect::<Vec<_>>();
    let coverage = days
        .first()
        .zip(days.last())
        .map(|(start, end)| json!({"start": start, "end": end}));
    json!({"coverage": coverage, "months": months})
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::date_nav_index;

    #[test]
    fn nav_drops_zeroes() {
        let index = date_nav_index(&BTreeMap::from([
            ("20260509".to_owned(), 0),
            ("20260510".to_owned(), 3),
        ]));
        assert_eq!(
            index,
            json!({"coverage": {"start": "20260510", "end": "20260510"}, "months": {"202605": 3}})
        );
    }
}
