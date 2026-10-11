// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit restore-only review of an entire historical manifest in the current repository.

use super::*;
use crate::ledger::writer::{
    LedgerConfirmation, LedgerSync, LedgerWriteEffect, LedgerWriteReceipt, LedgerWriteRefusal,
    append_legacy_restore_binding,
};
use solstone_core_backup::archive_readback::{HistoricalArchiveFile, ReadbackError};
use solstone_core_journal_io::activity_obligations::observe_with_carrier;
use solstone_core_journal_io::media::{
    MediaDigest, MediaStage, OwnedMediaFailure, OwnedMediaTarget, PreparedMedia,
};
use solstone_core_journal_io::paths::SegmentLayout;
use solstone_core_retention::archive_catalog::{
    ArchiveManifest, JournalCatalog, LegacyRestoreBinding, LegacyRestoreFileBinding,
};
use solstone_core_retention::{Mark, RemovalClass, Target, review_context};
use std::collections::BTreeSet;
use std::io;

mod preview;
pub use preview::{
    prepare_legacy_restore, prepare_owned_legacy_restore,
    prepare_owned_legacy_restore_with_recovery,
};
mod finish;
pub use finish::{
    HeldLegacyRestoreFinish, OwnedLegacyRestoreFinishHold, SettledLegacyRestoreReport,
};

/// Selectors only; fresh catalog and native admission determine the selected values.
pub struct LegacyRestoreSelection {
    pub target: Target,
    pub layout: SegmentLayout,
    pub manifest_id: String,
    pub names: Vec<String>,
}

pub struct LegacyRestoreRefusal {
    reason: &'static str,
    readback: Option<ReadbackError>,
}
impl LegacyRestoreRefusal {
    pub fn reason(&self) -> &'static str {
        self.reason
    }
    pub fn readback(&self) -> Option<ReadbackError> {
        self.readback
    }
}
fn refuse(reason: &'static str) -> LegacyRestoreRefusal {
    LegacyRestoreRefusal {
        reason,
        readback: None,
    }
}
fn read_refusal(error: ReadbackError) -> LegacyRestoreRefusal {
    LegacyRestoreRefusal {
        reason: "legacy_read_refused",
        readback: Some(error),
    }
}

struct ProbeRecord {
    name: String,
    capacity: Option<io::Result<u64>>,
    digest: Option<MediaDigest>,
    cleanup: Option<Cleanup>,
    parent_sync: Option<io::Result<()>>,
}
pub struct LegacyProbeFacts<'a> {
    pub name: &'a str,
    pub capacity: Option<&'a io::Result<u64>>,
    pub digest: Option<&'a MediaDigest>,
    pub cleanup: Option<Cleanup>,
    pub parent_sync: Option<&'a io::Result<()>>,
}
fn probe_facts(record: &ProbeRecord) -> LegacyProbeFacts<'_> {
    LegacyProbeFacts {
        name: &record.name,
        capacity: record.capacity.as_ref(),
        digest: record.digest.as_ref(),
        cleanup: record.cleanup,
        parent_sync: record.parent_sync.as_ref(),
    }
}
enum ProbeOwner {
    Creation(OwnedMediaFailure<Option<MediaStage>>),
    Stage(MediaStage),
    Target(OwnedMediaFailure<MediaStage>),
    Readback(ReadbackFailure),
    Prepared(PreparedMedia),
    ReadyTarget(OwnedMediaTarget),
}
impl ProbeOwner {
    fn source(&self) -> Option<&io::Error> {
        match self {
            Self::Creation(error) => Some(&error.error),
            Self::Target(error) => Some(&error.error),
            Self::Stage(stage) => {
                let _ = stage.diagnostic_path();
                None
            }
            Self::ReadyTarget(target) => {
                let _ = target.journal_root();
                None
            }
            Self::Prepared(media) => {
                let _ = media.identity();
                None
            }
            Self::Readback(_) => None,
        }
    }
}

struct LegacyEvidence {
    binding: LegacyRestoreBinding,
    probes: Vec<ProbeRecord>,
    write: LedgerWriteReceipt,
}
impl LegacyEvidence {
    fn ledger(&self) -> LedgerFacts {
        ledger_facts(&self.write)
    }
}
fn ledger_facts(write: &LedgerWriteReceipt) -> LedgerFacts {
    LedgerFacts {
        effect: Some(match write.effect() {
            LedgerWriteEffect::AlreadyRecorded => LedgerEffect::AlreadyRecorded,
            LedgerWriteEffect::Appended { bytes } => LedgerEffect::Appended { bytes },
            LedgerWriteEffect::Attempted { written } => LedgerEffect::Attempted { written },
        }),
        durability: write.durability().map(|sync| sync == LedgerSync::Confirmed),
        confirmed: write.confirmation() == LedgerConfirmation::Confirmed,
        failures: write.failures().to_vec(),
    }
}
fn confirmed(write: &LedgerWriteReceipt) -> bool {
    write.confirmation() == LedgerConfirmation::Confirmed
        && write.durability() == [LedgerSync::Confirmed; 4]
        && write.failures().is_empty()
        && !matches!(write.effect(), LedgerWriteEffect::Attempted { .. })
}

#[must_use = "retain the original read, immutable review and all uncertain probe/write owners"]
pub struct LegacyRestoreReview<R> {
    operation: R,
    recovery_binding: Option<QualifiedArchiveRecoveryBinding>,
    selection: LegacyRestoreSelection,
    destination: Option<PreparedRestore>,
    manifest: Option<ArchiveManifest>,
    selected: Vec<ArchiveWitness>,
    expected_marks: Vec<Option<Mark>>,
    required_capacity: u64,
    candidate: Option<LegacyRestoreBinding>,
    probes: Vec<ProbeRecord>,
    probe_owner: Option<ProbeOwner>,
    binding_write: Option<Result<LedgerWriteReceipt, LedgerWriteRefusal>>,
    refusal: Option<LegacyRestoreRefusal>,
}

pub type LegacyRestoreReviewOutcome<'clock> = LegacyRestoreReview<ReadOperation<'clock>>;
pub type OwnedLegacyRestoreReviewOutcome = LegacyRestoreReview<OwnedReadOperation>;
pub type OwnedLegacyRestoreReviewHold =
    LegacyRestoreReview<solstone_core_backup::archive_readback::OwnedDirectReadHold>;

impl<R> LegacyRestoreReview<R> {
    pub fn candidate(&self) -> Option<&LegacyRestoreBinding> {
        self.refusal
            .is_none()
            .then_some(self.candidate.as_ref())
            .flatten()
    }
    pub fn selected(&self) -> &[ArchiveWitness] {
        &self.selected
    }
    pub fn refusal(&self) -> Option<&LegacyRestoreRefusal> {
        self.refusal.as_ref()
    }
    pub fn probes(&self) -> impl ExactSizeIterator<Item = LegacyProbeFacts<'_>> {
        self.probes.iter().map(probe_facts)
    }
    pub fn retained_probe(&self) -> bool {
        self.probe_owner.is_some()
    }
    pub fn probe_source(&self) -> Option<&io::Error> {
        self.probe_owner.as_ref().and_then(ProbeOwner::source)
    }
    pub fn probe_readback(&self) -> Option<&ReadbackFailure> {
        match &self.probe_owner {
            Some(ProbeOwner::Readback(error)) => Some(error),
            _ => None,
        }
    }
    pub fn binding_write(&self) -> Option<LedgerFacts> {
        self.binding_write
            .as_ref()
            .and_then(|write| write.as_ref().ok())
            .map(ledger_facts)
    }
    pub fn binding_refused(&self) -> bool {
        self.binding_write.as_ref().is_some_and(Result::is_err)
    }
    /// The original subset is immutable; approval cannot supply a replacement selection.
    fn approve_inner<'clock>(
        mut self,
        adoption_id: &str,
        revision: &str,
        time: u64,
    ) -> Result<LegacyRestoreExecution<R>, Box<Self>>
    where
        R: RestoreRead<'clock>,
    {
        let result = (|| {
            if self.refusal.is_some() {
                return Err(refuse("legacy_review_held"));
            }
            let binding = self
                .candidate
                .as_ref()
                .ok_or_else(|| refuse("legacy_review_incomplete"))?;
            if binding.adoption_id != adoption_id || binding.revision != revision {
                return Err(refuse("legacy_review_changed"));
            }
            self.fresh(&self.selected)?;
            self.qualification()?;
            self.binding_write = Some(append_legacy_restore_binding(
                self.operation.as_read().journal_root(),
                binding,
                time,
            ));
            if !self
                .binding_write
                .as_ref()
                .and_then(|write| write.as_ref().ok())
                .is_some_and(confirmed)
            {
                return Err(refuse("legacy_binding_unconfirmed"));
            }
            self.fresh(&self.selected)?;
            let catalog = self.catalog()?;
            if catalog.restore_binding(adoption_id) != Some(binding) {
                return Err(refuse("legacy_binding_changed"));
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.refusal = Some(error);
            return Err(Box::new(self));
        }
        // Only the confirmed, still-owned review reaches execution. No decoded binding does.
        let segment = match &self.destination {
            Some(destination) => destination.locator().segment.clone(),
            None => {
                self.refusal = Some(refuse("legacy_destination_unavailable"));
                return Err(Box::new(self));
            }
        };
        let (binding, write) = match (self.candidate.take(), self.binding_write.take()) {
            (Some(binding), Some(Ok(write))) => (binding, write),
            (binding, write) => {
                self.candidate = binding;
                self.binding_write = write;
                self.refusal = Some(refuse("legacy_review_incomplete"));
                return Err(Box::new(self));
            }
        };
        Ok(self.execute(time, segment, binding, write))
    }

    fn catalog<'clock>(&self) -> Result<JournalCatalog, LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        let catalog = read_complete_catalog(self.operation.as_read().journal_root())
            .map_err(|_| refuse("legacy_catalog_unavailable"))?;
        if catalog.degraded() {
            return Err(refuse("legacy_catalog_degraded"));
        }
        Ok(catalog)
    }

    fn qualification<'clock>(&self) -> Result<(), LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        check_recovery(&self.operation, self.recovery_binding.as_ref())
            .map_err(|_| refuse("legacy_recovery_binding_changed"))?;
        self.operation
            .as_read()
            .revalidate_admission()
            .map_err(read_refusal)
    }

    fn census<'clock>(&self) -> Result<(), LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        self.qualification()?;
        let coverage = observe_with_carrier(
            self.operation.as_read().journal_root(),
            self.operation.as_read().carrier().observation(),
        )
        .map_err(|_| refuse("legacy_obligations_unknown"))?;
        if coverage.unresolved_entries().next().is_some() {
            return Err(refuse("legacy_obligations_held"));
        }
        self.qualification()
    }

    fn fresh<'clock>(&self, remaining: &[ArchiveWitness]) -> Result<(), LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        self.census()?;
        let destination = self
            .destination
            .as_ref()
            .ok_or_else(|| refuse("legacy_destination_unavailable"))?;
        destination
            .revalidate()
            .map_err(|_| refuse("legacy_destination_changed"))?;
        let catalog = self.catalog()?;
        let manifest = self
            .manifest
            .as_ref()
            .ok_or_else(|| refuse("legacy_manifest_unavailable"))?;
        let segment = exact_manifest(&catalog, &self.selection, manifest)?;
        if remaining
            .iter()
            .any(|witness| !segment.active.contains(witness))
        {
            return Err(refuse("legacy_selection_changed"));
        }
        let register = review_context::load_at_root(self.operation.as_read().journal_root())
            .map_err(|_| refuse("legacy_register_unavailable"))?;
        for (witness, expected) in self.selected.iter().zip(&self.expected_marks) {
            let current = selected_mark(&register, &self.selection.target, witness)?;
            if current != expected.as_ref() {
                return Err(refuse("legacy_mark_changed"));
            }
        }
        self.qualification()?;
        destination
            .revalidate()
            .map_err(|_| refuse("legacy_destination_changed"))
    }

    fn admit_again<'clock>(&self) -> Result<PreparedRestore, LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        let segments = discover(
            self.operation.as_read().journal_root(),
            &self.selection.target.day,
        )
        .map_err(|_| refuse("legacy_destination_unavailable"))?;
        let mut matching = segments.iter().filter(|segment| {
            segment.locator_identity().is_ok_and(|id| {
                id.layout == self.selection.layout
                    && id.name == self.selection.target.dir
                    && id.stream == self.selection.target.stream
            })
        });
        let segment = matching
            .next()
            .ok_or_else(|| refuse("legacy_destination_unavailable"))?;
        if matching.next().is_some() {
            return Err(refuse("legacy_destination_ambiguous"));
        }
        let admitted = PreparedRestore::admit(
            Arc::clone(self.operation.as_read().journal_root()),
            &self.selection.target.day,
            segment,
        )
        .map_err(|_| refuse("legacy_destination_changed"))?;
        if self
            .destination
            .as_ref()
            .is_some_and(|old| old.destination() != admitted.destination())
        {
            return Err(refuse("legacy_destination_changed"));
        }
        Ok(admitted)
    }

    fn execute<'clock>(
        mut self,
        time: u64,
        segment: String,
        binding: LegacyRestoreBinding,
        write: LedgerWriteReceipt,
    ) -> LegacyRestoreExecution<R>
    where
        R: RestoreRead<'clock>,
    {
        let mut files: Vec<_> = self
            .selected
            .iter()
            .map(|witness| FileRecord {
                detail: 0,
                day: self.selection.target.day.clone(),
                stream: self.selection.target.stream.clone(),
                segment: segment.clone(),
                witness: witness.clone(),
                disposition: RestoreDisposition::Unattempted,
                reason: None,
                files: 0,
                bytes: 0,
                owner: FileOwner::None,
            })
            .collect();
        let mut report = base(
            "refused",
            Some("restore_held"),
            "legacy",
            Some(&self.selection.target.day),
        );
        let bytes = self.selected.iter().map(|witness| witness.file.bytes).sum();
        report.segments_selected = 1;
        report.files_expected = files.len() as u64;
        report.bytes_expected = bytes;
        report.details.push(super::super::RestoreSegmentResult {
            status: "refused".into(),
            reason: Some("restore_held".into()),
            day: self.selection.target.day.clone(),
            stream: self.selection.target.stream.clone(),
            segment,
            snapshot_id: Some(binding.snapshot_id.clone()),
            files_expected: report.files_expected,
            files_restored: 0,
            bytes_expected: bytes,
            bytes_restored: 0,
        });
        let mut stopped = false;
        for (index, file) in files.iter_mut().enumerate() {
            if stopped {
                file.reason = Some("restore_held");
                continue;
            }
            let destination = self
                .fresh(&self.selected[index..])
                .and_then(|()| self.admit_again());
            let destination = match destination {
                Ok(destination) => destination,
                Err(error) => {
                    file.reason = Some(error.reason);
                    stopped = true;
                    continue;
                }
            };
            match StagedRestore::read_legacy(
                destination,
                &mut self.operation,
                file.witness.clone(),
                self.required_capacity,
                binding.clone(),
                self.recovery_binding.as_ref(),
            ) {
                Err(failure) => {
                    file.reason = Some("restore_held");
                    file.disposition = failure.disposition();
                    file.owner = FileOwner::Staging(failure);
                    stopped = true;
                }
                Ok(staged) => {
                    stopped = file.accept(
                        staged
                            .commit_preserving_mark(
                                time,
                                options(),
                                self.expected_marks[index].clone(),
                            )
                            .retire_intent(options())
                            .into_owned(),
                    )
                }
            }
        }
        summarize(&mut report, &files);
        LegacyRestoreExecution {
            inner: RestoreExecution {
                report,
                _operation: self.operation,
                _binding: self.recovery_binding,
                files,
            },
            evidence: LegacyEvidence {
                binding,
                probes: self.probes,
                write,
            },
        }
    }
}

fn exact_manifest<'a>(
    catalog: &'a JournalCatalog,
    selection: &LegacyRestoreSelection,
    expected: &ArchiveManifest,
) -> Result<&'a SegmentCatalog, LegacyRestoreRefusal> {
    let mut matching = catalog
        .days
        .iter()
        .flat_map(|day| &day.segments)
        .flat_map(|segment| {
            segment
                .history
                .iter()
                .map(move |manifest| (segment, manifest))
        })
        .filter(|(_, manifest)| manifest.manifest_id == expected.manifest_id);
    let (segment, manifest) = matching
        .next()
        .ok_or_else(|| refuse("legacy_manifest_changed"))?;
    if matching.next().is_some()
        || manifest != expected
        || segment.day != selection.target.day
        || segment.stream != selection.target.stream
        || solstone_core_format::segment::segment_key(&selection.target.dir).as_ref()
            != Some(&segment.segment)
    {
        return Err(refuse("legacy_manifest_changed"));
    }
    for witness in &expected.files {
        let mut matches = catalog
            .days
            .iter()
            .flat_map(|day| &day.segments)
            .flat_map(|segment| &segment.history)
            .flat_map(|manifest| &manifest.files)
            .filter(|file| file.witness_id == witness.witness_id);
        if matches.next() != Some(witness) || matches.next().is_some() {
            return Err(refuse("legacy_witness_ambiguous"));
        }
    }
    Ok(segment)
}

fn selected_mark<'a>(
    register: &'a solstone_core_retention::Register,
    target: &Target,
    witness: &ArchiveWitness,
) -> Result<Option<&'a Mark>, LegacyRestoreRefusal> {
    let mut matches = register.marks.values().filter(|mark| {
        mark.class == RemovalClass::OffloadRawRelease
            && mark.target == *target
            && mark.proposal.names.contains(&witness.file.name)
    });
    let mark = matches.next();
    if matches.next().is_some() {
        return Err(refuse("legacy_mark_ambiguous"));
    }
    Ok(mark)
}

#[must_use = "retain legacy review receipts alongside the complete native outcome"]
pub struct LegacyRestoreExecution<R> {
    inner: RestoreExecution<R>,
    evidence: LegacyEvidence,
}
pub type LegacyRestoreOutcome<'clock> = LegacyRestoreExecution<ReadOperation<'clock>>;
pub type OwnedLegacyRestoreOutcome = LegacyRestoreExecution<OwnedReadOperation>;

impl<R> LegacyRestoreExecution<R> {
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

impl<'clock> LegacyRestoreReviewOutcome<'clock> {
    pub fn carrier(&self) -> &Arc<solstone_core_journal_io::media::RootActivityCarrier> {
        self.operation.carrier()
    }
    pub fn approve(
        self,
        adoption_id: &str,
        revision: &str,
        time: u64,
    ) -> Result<LegacyRestoreOutcome<'clock>, Box<Self>> {
        self.approve_inner(adoption_id, revision, time)
    }
}
impl OwnedLegacyRestoreReviewOutcome {
    pub fn carrier(&self) -> &Arc<solstone_core_journal_io::media::RootActivityCarrier> {
        self.operation.carrier()
    }
    pub fn approve(
        self,
        adoption_id: &str,
        revision: &str,
        time: u64,
    ) -> Result<OwnedLegacyRestoreOutcome, Box<Self>> {
        self.approve_inner(adoption_id, revision, time)
    }
    pub fn into_owned_hold(self) -> OwnedLegacyRestoreReviewHold {
        let Self {
            operation,
            recovery_binding,
            selection,
            destination,
            manifest,
            selected,
            expected_marks,
            required_capacity,
            candidate,
            probes,
            probe_owner,
            binding_write,
            refusal,
        } = self;
        LegacyRestoreReview {
            operation: operation.into_owned_hold(),
            recovery_binding,
            selection,
            destination,
            manifest,
            selected,
            expected_marks,
            required_capacity,
            candidate,
            probes,
            probe_owner,
            binding_write,
            refusal,
        }
    }
}
impl OwnedLegacyRestoreReviewHold {
    pub fn carrier(&self) -> &Arc<solstone_core_journal_io::media::RootActivityCarrier> {
        self.operation.carrier()
    }
}

#[cfg(all(test, feature = "full-tests", target_os = "linux"))]
mod tests;
