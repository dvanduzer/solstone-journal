// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Consuming settlement for the synchronous local checked executor. Local return
//! settles only this operation's writers. Unqualified stage-namespace cooperation
//! holds production invocation, as required by native carrier retirement.

use super::*;
use solstone_core_journal_io::media::ActivityCarrierRetirementReceipt;

/// Independent local finish failure; this supplies no cleanup authority.
pub enum ReviewedRemovalFinishRefusal {
    IncompleteSelection,
    Native(ReviewedArchiveFinishRefusal),
    Retirement,
}

/// Every original owner survives refusal, without another execution entrance.
#[must_use]
pub struct OwnedReviewedRemoval {
    removal: ReviewedRemoval,
    finish_refusal: Option<ReviewedRemovalFinishRefusal>,
    retirement: Option<ActivityCarrierRetirementReceipt>,
}

impl OwnedReviewedRemoval {
    pub fn removal(&self) -> &ReviewedRemoval {
        &self.removal
    }
    pub fn finish_refusal(&self) -> Option<&ReviewedRemovalFinishRefusal> {
        self.finish_refusal.as_ref()
    }
    pub fn retirement(&self) -> Option<&ActivityCarrierRetirementReceipt> {
        self.retirement.as_ref()
    }
}

/// Actual completed local facts, without retained native or activity handles.
#[must_use]
pub struct ReviewedRemovalReport {
    outcome: Outcome,
    selected: Vec<Mark>,
    attempts: Vec<ReviewedRemovalFileReport>,
    mutation: Option<
        Result<
            crate::mark_mutation::BoundMutationOutcome,
            crate::mark_mutation::BoundMutationRefusal,
        >,
    >,
    retirement: ActivityCarrierRetirementReceipt,
}

impl ReviewedRemovalReport {
    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }
    pub fn selected_marks(&self) -> &[Mark] {
        &self.selected
    }
    pub fn attempts(&self) -> &[ReviewedRemovalFileReport] {
        &self.attempts
    }
    pub fn mutation(
        &self,
    ) -> Option<
        &Result<
            crate::mark_mutation::BoundMutationOutcome,
            crate::mark_mutation::BoundMutationRefusal,
        >,
    > {
        self.mutation.as_ref()
    }
    pub fn retirement(&self) -> &ActivityCarrierRetirementReceipt {
        &self.retirement
    }
}

#[must_use]
pub enum ReviewedRemovalFinish {
    Settled(Box<ReviewedRemovalReport>),
    Held(Box<OwnedReviewedRemoval>),
}

impl ReviewedRemoval {
    pub fn into_owned_hold(self) -> OwnedReviewedRemoval {
        OwnedReviewedRemoval {
            removal: self,
            finish_refusal: None,
            retirement: None,
        }
    }

    fn hold_local(self, reason: ReviewedRemovalFinishRefusal) -> ReviewedRemovalFinish {
        let mut held = self.into_owned_hold();
        held.finish_refusal = Some(reason);
        ReviewedRemovalFinish::Held(Box::new(held))
    }

    /// Finish actual local effects under the original activity. This operation
    /// launches no child/HTTP work; native namespace cooperation remains required.
    pub fn finish(self) -> ReviewedRemovalFinish {
        #[cfg(unix)]
        {
            self.finish_local()
        }
        #[cfg(not(unix))]
        {
            self.hold_local(ReviewedRemovalFinishRefusal::IncompleteSelection)
        }
    }

    #[cfg(unix)]
    fn finish_local(mut self) -> ReviewedRemovalFinish {
        use solstone_core_journal_io::media::{
            ActivityCarrierRetirementEffect, Confirmation, Durability,
        };

        if !archive_finish::complete_selected_files(&self.checked, &self.execution) {
            return self.hold_local(ReviewedRemovalFinishRefusal::IncompleteSelection);
        }
        let Some(carrier) = self.execution.carrier.take() else {
            return self.hold_local(ReviewedRemovalFinishRefusal::IncompleteSelection);
        };
        if let Err(reason) = archive_finish::finish_native(&mut self.execution, &carrier) {
            self.execution.carrier = Some(carrier);
            return self.hold_local(ReviewedRemovalFinishRefusal::Native(reason));
        }
        // This caller's local writes have returned. The owning invocation must
        // also qualify namespace cooperation; a census or lease does not do so.
        let retired = carrier.retire_after_settlement();
        let confirmed = retired.receipt.effect() == ActivityCarrierRetirementEffect::Removed
            && retired.receipt.root_durability() == Durability::Synced
            && retired.receipt.confirmation() == Confirmation::Confirmed
            && retired.receipt.failures().is_empty();
        if !confirmed {
            self.execution.carrier = Some(retired.owned);
            let mut held = self.into_owned_hold();
            held.finish_refusal = Some(ReviewedRemovalFinishRefusal::Retirement);
            held.retirement = Some(retired.receipt);
            return ReviewedRemovalFinish::Held(Box::new(held));
        }
        let selected = self.checked.marks().to_vec();
        let attempts = self
            .execution
            .attempts
            .into_iter()
            .map(ReviewedRemovalFileReport::from_actual)
            .collect();
        ReviewedRemovalFinish::Settled(Box::new(ReviewedRemovalReport {
            outcome: self.execution.outcome,
            selected,
            attempts,
            mutation: self.execution.mutation,
            retirement: retired.receipt,
        }))
    }
}
