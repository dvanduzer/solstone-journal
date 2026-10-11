// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;

pub fn prepare_legacy_restore<'clock>(
    operation: ReadOperation<'clock>,
    selection: LegacyRestoreSelection,
) -> LegacyRestoreReviewOutcome<'clock> {
    prepare(operation, None, selection)
}

pub fn prepare_owned_legacy_restore(
    operation: OwnedReadOperation,
    selection: LegacyRestoreSelection,
) -> OwnedLegacyRestoreReviewOutcome {
    prepare(operation, None, selection)
}

pub fn prepare_owned_legacy_restore_with_recovery(
    operation: OwnedReadOperation,
    binding: QualifiedArchiveRecoveryBinding,
    selection: LegacyRestoreSelection,
) -> OwnedLegacyRestoreReviewOutcome {
    prepare(operation, Some(binding), selection)
}

pub(super) fn prepare<'clock, R: RestoreRead<'clock>>(
    operation: R,
    recovery_binding: Option<QualifiedArchiveRecoveryBinding>,
    selection: LegacyRestoreSelection,
) -> LegacyRestoreReview<R> {
    let mut outcome = LegacyRestoreReview {
        operation,
        recovery_binding,
        selection,
        destination: None,
        manifest: None,
        selected: Vec::new(),
        expected_marks: Vec::new(),
        required_capacity: 0,
        candidate: None,
        probes: Vec::new(),
        probe_owner: None,
        binding_write: None,
        refusal: None,
    };
    if let Err(error) = outcome.prepare() {
        outcome.refusal = Some(error);
        release_probe(
            &mut outcome.probe_owner,
            &mut outcome.probes,
            outcome.destination.as_ref(),
        );
    }
    outcome
}

impl<R> LegacyRestoreReview<R> {
    fn prepare<'clock>(&mut self) -> Result<(), LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        self.census()?;
        let catalog = self.catalog()?;
        let mut manifests = catalog
            .days
            .iter()
            .flat_map(|day| &day.segments)
            .flat_map(|segment| &segment.history)
            .filter(|manifest| manifest.manifest_id == self.selection.manifest_id);
        let manifest = manifests
            .next()
            .ok_or_else(|| refuse("legacy_manifest_unavailable"))?
            .clone();
        if manifests.next().is_some() {
            return Err(refuse("legacy_manifest_ambiguous"));
        }
        let segment = exact_manifest(&catalog, &self.selection, &manifest)?;
        let mut names = BTreeSet::new();
        if self.selection.names.is_empty()
            || self.selection.names.iter().any(|name| !names.insert(name))
        {
            return Err(refuse("legacy_selection_invalid"));
        }
        if manifest.files.is_empty()
            || manifest.files.iter().any(|witness| {
                witness.repository_id.is_some()
                    || witness.archive_path.is_some()
                    || witness.journal_id.is_some()
                    || witness.processing.is_some()
                    || witness.segment_dir.is_some()
                    || witness.layout.is_some()
            })
        {
            return Err(refuse("legacy_manifest_bound"));
        }
        let snapshot = &manifest.files[0].snapshot_id;
        if manifest
            .files
            .iter()
            .any(|file| file.snapshot_id != *snapshot)
        {
            return Err(refuse("legacy_manifest_mixed"));
        }
        // Keep publication in the explicitly selected order. Verification below keeps full manifest order.
        for name in &self.selection.names {
            let mut witnesses = manifest
                .files
                .iter()
                .filter(|witness| witness.file.name == *name);
            let witness = witnesses
                .next()
                .ok_or_else(|| refuse("legacy_selection_invalid"))?;
            if witnesses.next().is_some() || !segment.active.contains(witness) {
                return Err(refuse("legacy_selection_changed"));
            }
            self.selected.push(witness.clone());
        }
        self.selected.iter().try_fold(0_u64, |bytes, witness| {
            bytes
                .checked_add(witness.file.bytes)
                .ok_or_else(|| refuse("legacy_selection_overflow"))
        })?;
        self.destination = Some(self.admit_again()?);
        let register = review_context::load_at_root(self.operation.as_read().journal_root())
            .map_err(|_| refuse("legacy_register_unavailable"))?;
        self.expected_marks = self
            .selected
            .iter()
            .map(|witness| {
                selected_mark(&register, &self.selection.target, witness).map(|mark| mark.cloned())
            })
            .collect::<Result<_, _>>()?;
        let largest = manifest
            .files
            .iter()
            .map(|file| file.file.bytes)
            .max()
            .ok_or_else(|| refuse("legacy_manifest_unavailable"))?;
        self.required_capacity = self
            .operation
            .as_read()
            .offload_required_bytes(
                largest,
                metadata_allowance().ok_or_else(|| refuse("legacy_capacity_overflow"))?,
            )
            .map_err(read_refusal)?;
        self.manifest = Some(manifest.clone());
        self.fresh(&self.selected)?;

        // The actual retained stage capacity is checked before even the resolver's WAN query.
        self.create_probe(&manifest.files[0].file.name)?;
        let files: Vec<_> = manifest
            .files
            .iter()
            .map(|witness| HistoricalArchiveFile {
                name: &witness.file.name,
                bytes: witness.file.bytes,
                sha256: &witness.file.sha256,
            })
            .collect();
        self.qualification()?;
        let resolved = self
            .operation
            .discover_historical_snapshot(snapshot, &files)
            .map_err(read_refusal)?;
        self.qualification()?;
        if resolved.files.len() != manifest.files.len() {
            return Err(refuse("legacy_resolution_changed"));
        }
        for (index, (original, exact)) in manifest.files.iter().zip(&resolved.files).enumerate() {
            if exact.repository_id != self.operation.as_read().repository_id()
                || exact.snapshot_id != resolved.snapshot_id
                || exact.name != original.file.name
                || exact.bytes != original.file.bytes
                || exact.sha256 != original.file.sha256
            {
                return Err(refuse("legacy_resolution_changed"));
            }
            if index > 0 {
                self.fresh(&self.selected)?;
                self.create_probe(&original.file.name)?;
            }
            self.verify_probe(exact)?;
        }
        self.fresh(&self.selected)?;
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| refuse("legacy_entropy_unavailable"))?;
        let mut binding = LegacyRestoreBinding {
            adoption_id: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
            revision: String::new(),
            target: self.selection.target.clone(),
            layout: self.selection.layout,
            repository_id: self.operation.as_read().repository_id().to_owned(),
            snapshot_id: resolved.snapshot_id,
            files: manifest
                .files
                .iter()
                .zip(resolved.files)
                .map(|(witness, file)| LegacyRestoreFileBinding {
                    witness_id: witness.witness_id.clone(),
                    name: file.name,
                    bytes: file.bytes,
                    sha256: file.sha256,
                    archive_path: file.archive_path,
                })
                .collect(),
            legacy_manifest: manifest,
        };
        binding.revision = binding
            .canonical_revision()
            .map_err(|_| refuse("legacy_binding_invalid"))?;
        binding
            .validate()
            .map_err(|_| refuse("legacy_binding_invalid"))?;
        if self
            .catalog()?
            .restore_binding(&binding.adoption_id)
            .is_some()
        {
            return Err(refuse("legacy_adoption_collision"));
        }
        self.candidate = Some(binding);
        Ok(())
    }

    fn create_probe<'clock>(&mut self, name: &str) -> Result<(), LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        let destination = self
            .destination
            .as_ref()
            .ok_or_else(|| refuse("legacy_destination_unavailable"))?;
        destination
            .revalidate()
            .map_err(|_| refuse("legacy_destination_changed"))?;
        self.qualification()?;
        self.probes.push(ProbeRecord {
            name: name.into(),
            capacity: None,
            digest: None,
            cleanup: None,
            parent_sync: None,
        });
        let stage = match MediaStage::create(
            destination.stage_parent(),
            Arc::clone(self.operation.as_read().activity()),
        ) {
            Ok(stage) => stage,
            Err(error) => {
                self.probe_owner = Some(ProbeOwner::Creation(error));
                return Err(refuse("legacy_stage_unavailable"));
            }
        };
        let capacity = stage.available_bytes();
        self.probe_owner = Some(ProbeOwner::Stage(stage));
        let record = self
            .probes
            .last_mut()
            .ok_or_else(|| refuse("legacy_probe_unavailable"))?;
        record.capacity = Some(capacity);
        match record.capacity.as_ref() {
            Some(Ok(available)) if *available >= self.required_capacity => {}
            Some(Ok(_)) => return Err(refuse("legacy_insufficient_free_space")),
            Some(Err(_)) | None => return Err(refuse("legacy_capacity_unavailable")),
        }
        destination
            .revalidate()
            .map_err(|_| refuse("legacy_destination_changed"))?;
        self.qualification()
    }

    fn verify_probe<'clock>(
        &mut self,
        exact: &solstone_core_backup::archive_readback::ExactArchiveFile,
    ) -> Result<(), LegacyRestoreRefusal>
    where
        R: RestoreRead<'clock>,
    {
        self.qualification()?;
        let stage = match self.probe_owner.take() {
            Some(ProbeOwner::Stage(stage)) => stage,
            owner => {
                self.probe_owner = owner;
                return Err(refuse("legacy_stage_unavailable"));
            }
        };
        // Probes hold only backup bytes; the restore download leaf marks them so.
        let target = match stage.create_named_target(std::ffi::OsStr::new(
            solstone_core_retention::restore_intent::RESTORE_DOWNLOAD_LEAF,
        )) {
            Ok(target) => target,
            Err(error) => {
                self.probe_owner = Some(ProbeOwner::Target(error));
                return Err(refuse("legacy_target_unavailable"));
            }
        };
        if let Err(error) = self.qualification() {
            self.probe_owner = Some(ProbeOwner::ReadyTarget(target));
            return Err(error);
        }
        let media = match self.operation.read_exact(exact, target) {
            Ok(media) => media,
            Err(error) => {
                self.probe_owner = Some(ProbeOwner::Readback(error));
                return Err(refuse("legacy_verification_failed"));
            }
        };
        self.probe_owner = Some(ProbeOwner::Prepared(media));
        let Some(ProbeOwner::Prepared(media)) = self.probe_owner.as_ref() else {
            return Err(refuse("legacy_probe_unavailable"));
        };
        self.probes
            .last_mut()
            .ok_or_else(|| refuse("legacy_probe_unavailable"))?
            .digest = Some(media.digest().clone());
        // Qualification after dump is required BEFORE cleanup, even for a verified file.
        self.qualification()?;
        let cleanup = match self.probe_owner.as_mut() {
            Some(ProbeOwner::Prepared(media)) => media.cleanup(),
            _ => return Err(refuse("legacy_probe_unavailable")),
        };
        self.probes
            .last_mut()
            .ok_or_else(|| refuse("legacy_probe_unavailable"))?
            .cleanup = Some(cleanup);
        self.qualification()?;
        if cleanup != Cleanup::Complete {
            return Err(refuse("legacy_probe_cleanup_held"));
        }
        let destination = self
            .destination
            .as_ref()
            .ok_or_else(|| refuse("legacy_destination_unavailable"))?;
        let sync = destination.stage_parent().sync();
        let confirmed = sync.is_ok();
        self.probes
            .last_mut()
            .ok_or_else(|| refuse("legacy_probe_unavailable"))?
            .parent_sync = Some(sync);
        if !confirmed {
            return Err(refuse("legacy_probe_sync_uncertain"));
        }
        destination
            .revalidate()
            .map_err(|_| refuse("legacy_destination_changed"))?;
        self.qualification()?;
        // Only confirmed cleanup/sync and the same live token allow releasing this probe.
        self.probe_owner = None;
        Ok(())
    }
}
