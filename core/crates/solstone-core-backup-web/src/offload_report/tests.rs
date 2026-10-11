// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;

#[test]
fn offload_report_keeps_uncertain_effects_and_unavailable_totals() {
    let failures = vec!["private-path credential-bearing diagnostic".into()];
    let value = ledger(
        [OffloadLedgerWrite::Recorded {
            effect: OffloadLedgerEffect::Attempted { written: Some(7) },
            durability: [
                OffloadLedgerSync::Confirmed,
                OffloadLedgerSync::Uncertain,
                OffloadLedgerSync::Confirmed,
                OffloadLedgerSync::Uncertain,
            ],
            confirmation: OffloadLedgerSync::Uncertain,
            failures: &failures,
        }]
        .into_iter(),
    );
    assert_eq!(value[0]["effect"], "attempted");
    assert_eq!(value[0]["bytes"], 7);
    assert_eq!(value[0]["confirmation"], "uncertain");
    assert_eq!(value[0]["durability"][0], "confirmed");
    assert_eq!(value[0]["durability"][1], "uncertain");
    assert_eq!(value[0]["failures"][0]["detail_available"], true);
    assert!(!value.to_string().contains("credential-bearing"));
    let planning = planning_fields(
        OffloadPlanningPhase::OffersPublished,
        None,
        None,
        None,
        None,
        None,
    );
    assert_eq!(planning["phase"], "offers_published");
    for field in ["budget_bytes", "local", "decision", "initial_projection"] {
        assert!(planning[field].is_null(), "{field}");
    }
    assert!(planning.get("budget_met").is_none());
}
