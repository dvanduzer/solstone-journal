// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use solstone_core_retention::Register;
use solstone_core_retention::mark_mutation::{
    ConditionalResult, RegisterUncertainty, RestoredMarkRefusal,
};

fn applied() -> BoundMutationOutcome {
    BoundMutationOutcome::Published(ConditionalResult {
        status: MutationStatus::Applied,
        register: Register::empty(),
    })
}

#[test]
fn only_confirmed_register_success_qualifies_and_late_publication_stays_whole() {
    for status in [MutationStatus::Applied, MutationStatus::AlreadyApplied] {
        assert!(register_confirmed(&Ok(BoundMutationOutcome::Unchanged(
            ConditionalResult {
                status,
                register: Register::empty(),
            }
        ))));
    }
    for status in [
        MutationStatus::Missing,
        MutationStatus::Stale,
        MutationStatus::Collision,
    ] {
        assert!(!register_confirmed(&Ok(BoundMutationOutcome::Unchanged(
            ConditionalResult {
                status,
                register: Register::empty(),
            }
        ))));
    }
    let uncertain = Ok(BoundMutationOutcome::PublishedUncertain {
        candidate: ConditionalResult {
            status: MutationStatus::Applied,
            register: Register::empty(),
        },
        uncertainty: RegisterUncertainty {
            durability: Some(io::Error::other("uncertain durability")),
            ..RegisterUncertainty::default()
        },
    });
    assert!(!register_confirmed(&uncertain));
    let late = Err(RecoveredRestoredMarkRefusal::PostMutation {
        outcome: Box::new(applied()),
        refusal: RestoredMarkRefusal::Changed,
    });
    assert!(!register_confirmed(&late));
    assert!(
        matches!(&late, Err(RecoveredRestoredMarkRefusal::PostMutation { outcome, refusal: RestoredMarkRefusal::Changed }) if matches!(outcome.as_ref(), BoundMutationOutcome::Published(result) if result.status == MutationStatus::Applied))
    );
}

#[test]
fn recovered_outcome_is_closed_send_static() {
    fn closed<T: Send + 'static>() {}
    closed::<RecoveredRestoreOutcome>();
}
