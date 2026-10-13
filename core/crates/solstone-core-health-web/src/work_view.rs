// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use serde_json::{Value, json};
use solstone_core_system::work_inventory::WorkInventory;
use std::collections::BTreeMap;

/// Non-retryable holds share a day-level action. Send one counted row per
/// day/reason instead of repeating that action for every recording. Explicit
/// retry units stay separate, with their exact identities and evidence.
pub(crate) fn summarize(mut inventory: WorkInventory) -> Value {
    let mut groups: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut holds = Vec::new();
    for hold in std::mem::take(&mut inventory.holds) {
        if hold.retry_unit.is_some() {
            holds.push(serde_json::to_value(hold).expect("serializable work hold"));
        } else {
            *groups.entry((hold.day, hold.reason)).or_default() += 1;
        }
    }
    holds.extend(groups.into_iter().map(|((day, reason), count)| {
        json!({
            "day": day, "reason": reason, "count": count,
        })
    }));
    let mut view = serde_json::to_value(inventory).expect("serializable inventory");
    view["holds"] = Value::Array(holds);
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use solstone_core_system::work_inventory::WorkHold;
    #[test]
    fn counts_repeated_holds_without_collapsing_retry_identities() {
        let hold: WorkHold = serde_json::from_value(json!({
            "day":"20261001", "band":"now", "reason":"spending_confirmation"
        }))
        .unwrap();
        let mut inventory = WorkInventory {
            holds: vec![hold.clone(); 1000],
            ..WorkInventory::default()
        };
        for id in ["one", "two"] {
            let mut retry = hold.clone();
            retry.retry_unit = Some(id.into());
            inventory.holds.push(retry);
        }
        let view = summarize(inventory);
        let holds = view["holds"].as_array().unwrap();
        assert_eq!(holds.len(), 3);
        assert_eq!(holds[0]["retry_unit"], "one");
        assert_eq!(holds[1]["retry_unit"], "two");
        assert_eq!(holds[2]["count"], 1000);
    }
}
