// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;
use solstone_core_backup::archive_readback::{OwnedSettledDirectRead, SettledDirectRead};

#[must_use]
pub struct SettledLegacyRestoreReport {
    inner: SettledRestoreReport,
    evidence: LegacyEvidence,
}
#[must_use = "retain the entire native hold and its original review/binding receipts"]
pub struct HeldLegacyRestoreFinish<'clock> {
    inner: Box<HeldRestoreFinish<'clock>>,
    evidence: LegacyEvidence,
}
#[must_use = "retain closed legacy custody alongside the outside Runtime owner"]
pub struct OwnedLegacyRestoreFinishHold {
    inner: OwnedRestoreFinishHold,
    evidence: LegacyEvidence,
}

impl<'clock> LegacyRestoreOutcome<'clock> {
    pub fn settle_direct(
        self,
    ) -> Result<
        (SettledDirectRead<'clock>, SettledLegacyRestoreReport),
        Box<HeldLegacyRestoreFinish<'clock>>,
    > {
        match self.inner.settle_direct() {
            Ok((read, inner)) => Ok((
                read,
                SettledLegacyRestoreReport {
                    inner,
                    evidence: self.evidence,
                },
            )),
            Err(inner) => Err(Box::new(HeldLegacyRestoreFinish {
                inner,
                evidence: self.evidence,
            })),
        }
    }
}
impl<'clock> HeldLegacyRestoreFinish<'clock> {
    pub fn report(&self) -> &RestoreResult {
        self.inner.report()
    }
    pub fn files(&self) -> impl ExactSizeIterator<Item = RestoreFileFacts<'_>> {
        self.inner.files()
    }
    pub fn refusal(&self) -> RestoreFinishRefusal<'_> {
        self.inner.refusal()
    }
    pub fn binding(&self) -> &LegacyRestoreBinding {
        &self.evidence.binding
    }
    pub fn probes(&self) -> impl ExactSizeIterator<Item = LegacyProbeFacts<'_>> {
        self.evidence.probes.iter().map(probe_facts)
    }
    pub fn binding_write(&self) -> LedgerFacts {
        self.evidence.ledger()
    }
    pub fn carrier(&self) -> &Arc<solstone_core_journal_io::media::RootActivityCarrier> {
        self.inner.carrier()
    }
    pub fn into_owned_hold(self) -> Result<OwnedLegacyRestoreFinishHold, Box<Self>> {
        match self.inner.into_owned_hold() {
            Ok(inner) => Ok(OwnedLegacyRestoreFinishHold {
                inner,
                evidence: self.evidence,
            }),
            Err(inner) => Err(Box::new(Self {
                inner,
                evidence: self.evidence,
            })),
        }
    }
}
impl OwnedLegacyRestoreFinishHold {
    pub fn report(&self) -> &RestoreResult {
        self.inner.report()
    }
    pub fn files(&self) -> impl ExactSizeIterator<Item = RestoreFileFacts<'_>> {
        self.inner.files()
    }
    pub fn refusal(&self) -> RestoreFinishRefusal<'_> {
        self.inner.refusal()
    }
    pub fn binding(&self) -> &LegacyRestoreBinding {
        &self.evidence.binding
    }
    pub fn probes(&self) -> impl ExactSizeIterator<Item = LegacyProbeFacts<'_>> {
        self.evidence.probes.iter().map(probe_facts)
    }
    pub fn binding_write(&self) -> LedgerFacts {
        self.evidence.ledger()
    }
    pub fn carrier(&self) -> &Arc<solstone_core_journal_io::media::RootActivityCarrier> {
        self.inner.carrier()
    }
}
impl SettledLegacyRestoreReport {
    pub fn report(&self) -> &RestoreResult {
        self.inner.report()
    }
    pub fn files(&self) -> impl ExactSizeIterator<Item = RestoreFileFacts<'_>> {
        self.inner.files()
    }
    pub fn binding(&self) -> &LegacyRestoreBinding {
        &self.evidence.binding
    }
    pub fn probes(&self) -> impl ExactSizeIterator<Item = LegacyProbeFacts<'_>> {
        self.evidence.probes.iter().map(probe_facts)
    }
    pub fn binding_write(&self) -> LedgerFacts {
        self.evidence.ledger()
    }
}

impl OwnedLegacyRestoreOutcome {
    pub fn settle_direct(
        self,
    ) -> Result<
        (OwnedSettledDirectRead, SettledLegacyRestoreReport),
        Box<OwnedLegacyRestoreFinishHold>,
    > {
        match self.inner.settle_direct() {
            Ok((read, inner)) => Ok((
                read,
                SettledLegacyRestoreReport {
                    inner,
                    evidence: self.evidence,
                },
            )),
            Err(inner) => Err(Box::new(OwnedLegacyRestoreFinishHold {
                inner: *inner,
                evidence: self.evidence,
            })),
        }
    }

    pub fn into_owned_hold(self) -> OwnedLegacyRestoreFinishHold {
        OwnedLegacyRestoreFinishHold {
            inner: self.inner.into_owned_hold(),
            evidence: self.evidence,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_receipts_and_closed_custody_are_send_static() {
        fn require<T: Send + 'static>() {}
        require::<super::SettledLegacyRestoreReport>();
        require::<super::OwnedLegacyRestoreFinishHold>();
        require::<super::OwnedLegacyRestoreReviewHold>();
    }
}
