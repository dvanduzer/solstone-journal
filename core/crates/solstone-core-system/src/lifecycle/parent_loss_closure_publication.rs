// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bounded audit chunks and reusable final bytes for abandoned closure.
use super::*;
use solstone_core_journal_io::{AtomicWriteOptions, atomic_replace, write_bytes_exclusive};
#[path = "parent_loss_closure_assembly.rs"]
mod assembly;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PreparedAdmission {
    launch_id: String,
    closure: String,
    sealed: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PreparedChunk {
    schema: u32,
    generation: ParentLossGeneration,
    seq: u64,
    batch_digest: String,
    ids_digest: String,
    rows: Vec<PreparedAdmission>,
}

#[derive(Serialize, Deserialize)]
struct AuditedInput {
    batch_digest: String,
    ids_digest: String,
    chunk_digest: String,
}

/// Writer-owned commit of completed source validation. A prepared chunk and
/// its adjacent checksum cannot authorize themselves. This commit is replaced
/// only after its exact inputs have passed source validation and its canonical
/// output has been durably written.
#[derive(Serialize, Deserialize)]
struct AuditProgress {
    schema: u32,
    generation: ParentLossGeneration,
    sealed_phase: ParentLossPhase,
    inputs: HashMap<u64, AuditedInput>,
}

#[derive(Serialize, Deserialize)]
struct Publication {
    schema: u32,
    generation: ParentLossGeneration,
    supervisor: Option<ProcessInstance>,
    coordinator: Option<ProcessInstance>,
    inventory: Vec<String>,
    // Inputs are immutable completed observations, not scheduling IDs alone.
    inputs: Vec<(u64, String, String)>,
    provisional_sources: HashMap<String, AdmissionSourceSnapshot>,
    ledger_digest: String,
    record_digest: String,
}

fn phase(ledger: &ParentLossLedger, generation: ParentLossGeneration) -> std::path::PathBuf {
    ledger.closure_recovery_phase_dir(generation)
}

fn read_publication(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
) -> Result<Option<Publication>, ParentLossLedgerError> {
    let path = phase(ledger, generation).join("publication.json");
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn inventory(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
) -> Result<Vec<String>, ParentLossLedgerError> {
    let mut ids = Vec::new();
    match fs::read_dir(admission_directory(ledger, generation)) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| unverifiable_admission())?;
                validate_launch_id(&name).map_err(|_| unverifiable_admission())?;
                ids.push(name);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    ids.sort();
    Ok(ids)
}

fn provisional_matches(ledger: &ParentLossLedger, proof: &Publication) -> bool {
    proof.provisional_sources.iter().all(|(id, snapshot)| {
        read_derived_admission_drops(ledger, proof.generation, id)
            .is_ok_and(|derived| derived.sources == *snapshot)
    })
}

fn matches_digest(path: &Path, expected: &str) -> bool {
    fs::read(path).is_ok_and(|bytes| digest_bytes(&bytes) == expected)
}

/// Called before the ordinary reader deserializes a possibly huge closed record.
/// The lease and active lock are already held. Observations remain vetoes.
pub(super) fn completed(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    authority: &ClosingAuthority<'_>,
) -> Result<bool, ParentLossLedgerError> {
    let Some(proof) = read_publication(ledger, generation)? else {
        return Ok(false);
    };
    if proof.schema != 1 || proof.generation != generation {
        return Ok(false);
    }
    for (instance, reason) in [
        (proof.supervisor, BootstrapRecoveryReason::SupervisorLive),
        (
            proof.coordinator,
            BootstrapRecoveryReason::ActiveCoordinator,
        ),
    ] {
        if instance.is_some_and(|i| {
            matches!(
                authority.source.observe(&i),
                InstanceVerdict::SameLive { .. }
            )
        }) {
            return Err(ParentLossLedgerError::RecoveryRequired(reason));
        }
    }
    if !matches_digest(&ledger.record_path(generation), &proof.record_digest) {
        return Ok(false);
    }
    if !read_retirement_census(ledger, generation)?
        .obligations
        .is_empty()
        || inventory(ledger, generation)? != proof.inventory
        || !provisional_matches(ledger, &proof)
    {
        return Ok(false);
    }
    Ok(
        matches_digest(&ledger.sealed_ledger_path(generation), &proof.ledger_digest)
            && matches_digest(&ledger.record_path(generation), &proof.record_digest),
    )
}

/// A publication overtaken before successor allocation is preserved and audited again.
/// Move the exact old record first: a crash then resumes with missing state, while
/// changed provisional sources prevent the old manifest from authorizing reuse.
pub(super) fn supersede_overtaken_publication(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
) -> Result<Option<std::path::PathBuf>, ParentLossLedgerError> {
    let Some(proof) = read_publication(ledger, generation)? else {
        return Ok(None);
    };
    if proof.schema != 1
        || proof.generation != generation
        || !matches_digest(&ledger.record_path(generation), &proof.record_digest)
        || !matches_digest(&ledger.sealed_ledger_path(generation), &proof.ledger_digest)
    {
        return Ok(None);
    }
    let changed = inventory(ledger, generation)? != proof.inventory
        || proof.provisional_sources.iter().any(|(id, snapshot)| {
            read_derived_admission_drops(ledger, generation, id)
                .is_ok_and(|derived| derived.sources != *snapshot)
        });
    if !changed {
        return Ok(None);
    }
    let aside = set_aside(&ledger.record_path(generation))?;
    fs::File::open(ledger.generation_path(generation))?.sync_all()?;
    Ok(Some(aside))
}

pub(super) enum PublicationOutcome {
    Closed,
    BudgetExpired,
    RebuildRequired,
    InputsChanged,
    #[cfg(test)]
    Paused,
}

impl PublicationOutcome {
    pub(super) fn into_closure(self) -> ClosureOutcome {
        match self {
            Self::Closed => ClosureOutcome::Closed,
            _ => ClosureOutcome::Refused(BootstrapRecoveryReason::CoordinatorNotLive),
        }
    }
}

fn finish(ledger: &ParentLossLedger, proof: &Publication) -> Result<(), ParentLossLedgerError> {
    let dir = phase(ledger, proof.generation);
    let final_ledger = ledger.sealed_ledger_path(proof.generation);
    if !matches_digest(&final_ledger, &proof.ledger_digest) {
        // A manifest only authorizes its fully audited exact byte sequence.
        if final_ledger.exists() {
            return Err(unverifiable_admission());
        }
        let bytes = fs::read(dir.join("ledger.prepared"))?;
        if digest_bytes(&bytes) != proof.ledger_digest {
            return Err(unverifiable_admission());
        }
        write_bytes_exclusive(
            &final_ledger,
            &bytes,
            AtomicWriteOptions { mode: Some(0o600) },
        )?;
    }
    seam("after-ledger");
    let final_record = ledger.record_path(proof.generation);
    if !matches_digest(&final_record, &proof.record_digest) {
        let bytes = fs::read(dir.join("record.prepared"))?;
        if digest_bytes(&bytes) != proof.record_digest {
            return Err(unverifiable_admission());
        }
        atomic_replace(
            &final_record,
            &bytes,
            AtomicWriteOptions { mode: Some(0o600) },
        )?;
    }
    seam("after-record");
    Ok(())
}

fn proof_still_current(
    ledger: &ParentLossLedger,
    proof: &Publication,
) -> Result<bool, ParentLossLedgerError> {
    Ok(inventory(ledger, proof.generation)? == proof.inventory
        && provisional_matches(ledger, proof)
        && read_retirement_census(ledger, proof.generation)?
            .obligations
            .is_empty())
}

fn finish_current(
    ledger: &ParentLossLedger,
    proof: &Publication,
) -> Result<PublicationOutcome, ParentLossLedgerError> {
    // Both callers checked the exact current sources immediately before this
    // stage. Recheck after writes as acknowledgement writers may run meanwhile.
    finish(ledger, proof)?;
    #[cfg(test)]
    inject_finalization_ack(ledger, &FINISH_ACK_FAULT);
    if !proof_still_current(ledger, proof)? {
        return Ok(PublicationOutcome::InputsChanged);
    }
    Ok(PublicationOutcome::Closed)
}

/// The final writer-owned commit authorizes exact prepared bytes independently
/// of historical scheduling caches, including the scan-complete hint.
pub(super) fn resume_prepared(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    authority: &ClosingAuthority<'_>,
    record: Option<&ParentLossGenerationRecord>,
) -> Result<Option<PublicationOutcome>, ParentLossLedgerError> {
    let Some(proof) = read_publication(ledger, generation)? else {
        return Ok(None);
    };
    if proof.schema != 1
        || proof.generation != generation
        || !read_retirement_census(ledger, generation)?
            .obligations
            .is_empty()
        || inventory(ledger, generation)? != proof.inventory
        || !provisional_matches(ledger, &proof)
    {
        return Ok(None);
    }
    if record
        .and_then(|r| r.sealed_ledger_digest.as_ref())
        .is_some_and(|digest| *digest != proof.ledger_digest)
    {
        return Err(unverifiable_admission());
    }
    if Instant::now() >= authority.deadline {
        return Ok(Some(PublicationOutcome::BudgetExpired));
    }
    finish_current(ledger, &proof).map(Some)
}

fn seam(_name: &str) {
    #[cfg(feature = "test-hooks")]
    if std::env::var("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM").as_deref() == Ok(_name) {
        loop {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

pub(super) fn current_provisional_sources(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    provisional: &HashMap<String, ProvisionalFile>,
) -> Result<Option<HashMap<String, AdmissionSourceSnapshot>>, ParentLossLedgerError> {
    let mut provisional_sources = HashMap::new();
    for (id, prov) in provisional {
        let derived = read_derived_admission_drops(ledger, generation, id)?;
        if derived.instance.is_some()
            || derived.finding != prov.finding
            || derived.service != prov.service
            || derived.digests.intent != prov.intent_digest
            || derived.digests.result != prov.result_digest
            || derived.digests.ack != prov.ack_digest
            || derived.digests.spawn_identity != prov.spawn_identity_digest
        {
            return Ok(None);
        }
        provisional_sources.insert(id.clone(), derived.sources);
    }
    Ok(Some(provisional_sources))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn publish(
    ledger: &ParentLossLedger,
    generation: ParentLossGeneration,
    successor_generation: ParentLossGeneration,
    authority: &ClosingAuthority<'_>,
    supervisor: Option<ProcessInstance>,
    coordinator: Option<ProcessInstance>,
    record: Option<ParentLossGenerationRecord>,
    supervisor_observation: AuthorityObservation,
    coordinator_observation: AuthorityObservation,
    mut set_aside_files: Vec<String>,
    mut notes: Vec<String>,
    all_ids: Vec<String>,
    seqs: Vec<u64>,
    provisional: &HashMap<String, ProvisionalFile>,
) -> Result<PublicationOutcome, ParentLossLedgerError> {
    let dir = phase(ledger, generation);
    if let Some(outcome) = resume_prepared(ledger, generation, authority, record.as_ref())? {
        return Ok(outcome);
    }
    let Some(provisional_sources) = current_provisional_sources(ledger, generation, provisional)?
    else {
        return Ok(PublicationOutcome::RebuildRequired);
    };
    let progress_path = dir.join("audit-progress.json");
    let mut progress = match read_json_durable_validated::<AuditProgress>(
        ArtifactId::ParentLossClosureRecovery,
        &progress_path,
        |p| {
            if p.schema != 1
                || p.generation != generation
                || p.sealed_phase != ParentLossPhase::Sealed
            {
                return Err("invalid audit progress envelope".to_owned());
            }
            Ok(())
        },
    )? {
        DurableRead::Present(p) => p,
        _ => AuditProgress {
            schema: 1,
            generation,
            sealed_phase: ParentLossPhase::Sealed,
            inputs: HashMap::new(),
        },
    };
    let mut runs = Vec::new();
    let mut inputs = Vec::new();
    for seq in seqs {
        if Instant::now() >= authority.deadline {
            return Ok(PublicationOutcome::BudgetExpired);
        }
        let batch_path = ledger.closure_recovery_batch_path(generation, seq);
        let ids_path = ledger.closure_recovery_ids_path(generation, seq);
        let receipt_path = ledger.closure_recovery_receipt_path(generation, seq);
        let preserve_invalid = || -> Result<PublicationOutcome, ParentLossLedgerError> {
            // No derived payload can make a scheduling hint authoritative. Drop
            // the hint with the damaged payload so the next scan reads originals.
            for path in [&batch_path, &ids_path, &receipt_path] {
                if path.exists() {
                    set_aside(path)?;
                }
            }
            Ok(PublicationOutcome::RebuildRequired)
        };
        let Ok(ids_bytes) = fs::read(&ids_path) else {
            return preserve_invalid();
        };
        let ids_digest = digest_bytes(&ids_bytes);
        if let Some(committed) = progress.inputs.get(&seq)
            && committed.ids_digest == ids_digest
            && let Some(run) = assembly::cached_run(
                &dir,
                generation,
                seq,
                &committed.chunk_digest,
                &ids_digest,
                &ids_bytes,
            )?
        {
            inputs.push((seq, committed.batch_digest.clone(), ids_digest));
            runs.push(run);
            continue;
        }
        let prepared_path = dir.join(format!("audited-{seq:06}.json"));
        let stamp_path = dir.join(format!("audited-{seq:06}.sha256"));
        let retained = progress.inputs.get(&seq).and_then(|committed| {
            if committed.ids_digest != ids_digest {
                return None;
            }
            let bytes = fs::read(&prepared_path).ok()?;
            if digest_bytes(&bytes) != committed.chunk_digest {
                return None;
            }
            let chunk: PreparedChunk = serde_json::from_slice(&bytes).ok()?;
            (chunk.schema == 1
                && chunk.generation == generation
                && chunk.seq == seq
                && chunk.batch_digest == committed.batch_digest
                && chunk.ids_digest == ids_digest)
                .then_some(chunk)
        });
        let (batch_digest, chunk) = if let Some(chunk) = retained {
            // This writer's independent commit authorizes the exact canonical
            // output. The raw batch is historical provenance after that commit;
            // rereading it would spend every retry on the completed prefix.
            (chunk.batch_digest.clone(), chunk)
        } else {
            #[cfg(test)]
            AUDIT_BATCH_READS.with(|count| count.set(count.get() + 1));
            let Ok(batch_bytes) = fs::read(&batch_path) else {
                return preserve_invalid();
            };
            let batch_digest = digest_bytes(&batch_bytes);
            let parsed = (|| {
                let batch: RecoveryBatchFile = serde_json::from_slice(&batch_bytes).ok()?;
                let index: RecoveryIdsFile = serde_json::from_slice(&ids_bytes).ok()?;
                let receipt: RecoveryBatchReceipt =
                    serde_json::from_slice(&fs::read(&receipt_path).ok()?).ok()?;
                let roster: Vec<_> = batch.entries.iter().map(|r| r.launch_id.clone()).collect();
                (batch.schema == PARENT_LOSS_RECOVERY_SCHEMA_V1
                    && batch.generation == generation
                    && batch.sealed_phase == ParentLossPhase::Sealed
                    && batch.kind == RecoveryArtifactKind::Batch
                    && batch.seq == seq
                    && index.seq == seq
                    && index.generation == generation
                    && index.schema == PARENT_LOSS_RECOVERY_SCHEMA_V1
                    && index.kind == RecoveryArtifactKind::Ids
                    && index.sealed_phase == ParentLossPhase::Sealed
                    && receipt.schema == PARENT_LOSS_RECOVERY_SCHEMA_V1
                    && receipt.generation == generation
                    && receipt.seq == seq
                    && receipt.sealed_phase == ParentLossPhase::Sealed
                    && receipt.kind == RecoveryArtifactKind::Receipt
                    && receipt.batch_bytes_digest == batch_digest
                    && receipt.ids == index.ids
                    && roster == index.ids
                    && batch
                        .entries
                        .iter()
                        .all(|row| completed_row_is_authorized(ledger, generation, row)))
                .then_some(batch)
            })();
            let Some(batch) = parsed else {
                return preserve_invalid();
            };
            #[cfg(test)]
            AUDIT_VALIDATED_ROWS.with(|count| count.set(count.get() + batch.entries.len()));
            let mut prepared = Vec::with_capacity(batch.entries.len());
            for row in batch.entries {
                let closure = AdmissionClosure {
                    launch_id: row.launch_id.clone(),
                    service: row.service,
                    instance: row.instance,
                    uid: row.uid,
                    finding: row.finding,
                };
                let sealed = match (row.instance, row.uid) {
                    (Some(instance), Some(uid)) => Some(serde_json::to_string(&SealedAdmission {
                        launch_id: row.launch_id.clone(),
                        service: row.service,
                        identity: AdmissionIdentity {
                            generation,
                            launch_id: row.launch_id.clone(),
                            instance,
                            uid,
                            parent_launch_id: row.parent_launch_id,
                        },
                    })?),
                    _ => None,
                };
                prepared.push(PreparedAdmission {
                    launch_id: row.launch_id,
                    closure: serde_json::to_string(&closure)?,
                    sealed,
                });
            }
            let chunk = PreparedChunk {
                schema: 1,
                generation,
                seq,
                batch_digest: batch_digest.clone(),
                ids_digest: ids_digest.clone(),
                rows: prepared,
            };
            let bytes = serde_json::to_vec(&chunk)?;
            let chunk_digest = digest_bytes(&bytes);
            atomic_replace(
                &prepared_path,
                &bytes,
                AtomicWriteOptions { mode: Some(0o600) },
            )?;
            atomic_replace(
                &stamp_path,
                chunk_digest.as_bytes(),
                AtomicWriteOptions { mode: Some(0o600) },
            )?;
            progress.inputs.insert(
                seq,
                AuditedInput {
                    batch_digest: batch_digest.clone(),
                    ids_digest: ids_digest.clone(),
                    chunk_digest,
                },
            );
            // The committed expected digest is independent of the weak chunk
            // checksum, and is never regenerated from unvalidated retained data.
            write_json(&progress_path, &progress, json_options())?;
            #[cfg(test)]
            if AUDIT_STOP_AFTER_BATCH.with(|remaining| {
                let value = remaining.get();
                if value == 0 {
                    return false;
                }
                remaining.set(value - 1);
                value == 1
            }) {
                return Ok(PublicationOutcome::Paused);
            }
            (batch_digest, chunk)
        };
        let committed = progress
            .inputs
            .get(&seq)
            .ok_or_else(unverifiable_admission)?;
        runs.push(assembly::commit_run(
            &dir,
            generation,
            seq,
            &committed.chunk_digest,
            &ids_digest,
            &ids_bytes,
            chunk.rows,
        )?);
        inputs.push((seq, batch_digest, ids_digest));
        seam("during-finalization");
    }
    let mut provisional_rows = Vec::new();
    for prov in provisional.values() {
        let closure = AdmissionClosure {
            launch_id: prov.launch_id.clone(),
            service: prov.service,
            instance: None,
            uid: None,
            finding: prov.finding,
        };
        provisional_rows.push(PreparedAdmission {
            launch_id: prov.launch_id.clone(),
            closure: serde_json::to_string(&closure)?,
            sealed: None,
        });
    }
    runs.extend(assembly::provisional_runs(&dir, provisional_rows)?);
    // The caller established this roster. The post-copy publication veto checks
    // the fresh inventory again before it can authorize a successor.
    let Some(spools) = assembly::merge(
        &dir,
        generation,
        authority,
        &runs,
        &all_ids,
        &provisional_sources,
    )?
    else {
        return Ok(PublicationOutcome::BudgetExpired);
    };
    if Instant::now() >= authority.deadline {
        return Ok(PublicationOutcome::BudgetExpired);
    }
    let mut existing_ledger = None;
    let ledger_path = ledger.sealed_ledger_path(generation);
    if ledger_path.exists() {
        let existing_bytes = fs::read(&ledger_path)?;
        if spools.existing_ledger_matches(&existing_bytes)? {
            #[cfg(test)]
            if LEGACY_LEDGER_MUTATION_FAULT.with(|fault| fault.get()) {
                fs::write(&ledger_path, b"[]")?;
            }
            existing_ledger = Some(existing_bytes);
        } else if record
            .as_ref()
            .and_then(|r| r.sealed_ledger_digest.as_ref())
            .is_some()
        {
            return Err(unverifiable_admission());
        } else {
            let aside = set_aside(&ledger_path)?;
            fs::File::open(ledger.generation_path(generation))?.sync_all()?;
            if let Some(name) = aside.file_name() {
                set_aside_files.push(name.to_string_lossy().into_owned());
            }
        }
    }
    let ledger_digest = if let Some(bytes) = existing_ledger {
        let expected = digest_bytes(&bytes);
        atomic_replace(
            dir.join("ledger.prepared"),
            &bytes,
            AtomicWriteOptions { mode: Some(0o600) },
        )?;
        expected
    } else {
        spools.write_array(&dir.join("ledger.prepared"), true, b"[", b"]\n")?
    };
    let mut record = record.unwrap_or_else(|| ParentLossGenerationRecord {
        schema: PARENT_LOSS_LEDGER_SCHEMA_V1,
        generation,
        coordinator,
        supervisor: supervisor.unwrap_or(authority.closed_by),
        sealed_ledger_digest: None,
        terminal: None,
        supervisor_heartbeat: None,
        closure: None,
    });
    if record
        .sealed_ledger_digest
        .as_ref()
        .is_some_and(|d| *d != ledger_digest)
    {
        return Err(unverifiable_admission());
    }
    record.sealed_ledger_digest = Some(ledger_digest.clone());
    if record.terminal.is_none() {
        record.terminal = Some(ParentLossTerminalDisposition::Unresolved {
            reason: ParentLossUnresolvedReason::AuthoritiesLost,
        });
    }
    if supervisor.is_none() {
        notes.push("no supervisor identity was recorded; the record names the closer".to_owned());
    }
    let heartbeat = match (record.supervisor_heartbeat.as_deref(), supervisor) {
        (Some(filename), Some(instance)) => clear_supervisor_heartbeat(ledger, filename, instance),
        _ => HeartbeatRetirement::NotRecorded,
    };
    record.closure = Some(AbandonedGenerationClosure {
        schema: PARENT_LOSS_CLOSURE_SCHEMA_V1,
        closed_by: authority.closed_by,
        successor_generation,
        closed_at: unix_seconds(),
        supervisor: supervisor_observation,
        coordinator: coordinator_observation,
        heartbeat,
        set_aside: set_aside_files,
        notes,
        admissions: Vec::new(),
    });
    let header = serde_json::to_string(&record)?;
    let needle = "\"admissions\":[]";
    if !header.contains(needle) {
        return Err(unverifiable_admission());
    }
    let (before, after) = header
        .split_once(needle)
        .ok_or_else(unverifiable_admission)?;
    let prefix = format!("{before}\"admissions\":[");
    let suffix = format!("]{after}");
    // Expected digests come from authenticated input spans and writer literals.
    let record_digest = spools.write_array(
        &dir.join("record.prepared"),
        false,
        prefix.as_bytes(),
        suffix.as_bytes(),
    )?;
    let proof = Publication {
        schema: 1,
        generation,
        supervisor,
        coordinator,
        inventory: all_ids,
        inputs,
        provisional_sources,
        ledger_digest,
        record_digest,
    };
    #[cfg(test)]
    inject_finalization_ack(ledger, &FINALIZATION_ACK_FAULT);
    if !proof_still_current(ledger, &proof)? {
        return Ok(PublicationOutcome::InputsChanged);
    }
    write_json(dir.join("publication.json"), &proof, json_options())?;
    #[cfg(test)]
    if PUBLICATION_STOP_FAULT.with(|fault| fault.get()) {
        return Ok(PublicationOutcome::Paused);
    }
    seam("before-publication");
    if Instant::now() >= authority.deadline {
        return Ok(PublicationOutcome::BudgetExpired);
    }
    finish_current(ledger, &proof)
}
