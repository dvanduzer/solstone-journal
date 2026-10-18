// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bounded audit chunks and reusable final bytes for abandoned closure.
use super::*;
use solstone_core_journal_io::{AtomicWriteOptions, atomic_replace, write_bytes_exclusive};

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

fn inventory(
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

fn inputs_match(ledger: &ParentLossLedger, proof: &Publication) -> bool {
    proof.inputs.iter().all(|(seq, batch, ids)| {
        fs::read(ledger.closure_recovery_batch_path(proof.generation, *seq))
            .is_ok_and(|b| digest_bytes(&b) == *batch)
            && fs::read(ledger.closure_recovery_ids_path(proof.generation, *seq))
                .is_ok_and(|b| digest_bytes(&b) == *ids)
    })
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

fn finish(
    ledger: &ParentLossLedger,
    proof: &Publication,
) -> Result<ClosureOutcome, ParentLossLedgerError> {
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
    Ok(ClosureOutcome::Closed)
}

fn seam(_name: &str) {
    #[cfg(feature = "test-hooks")]
    if std::env::var("SOLSTONE_PARENT_LOSS_CLOSURE_SEAM").as_deref() == Ok(_name) {
        loop {
            thread::sleep(Duration::from_millis(100));
        }
    }
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
) -> Result<ClosureOutcome, ParentLossLedgerError> {
    let dir = phase(ledger, generation);
    if let Some(proof) = read_publication(ledger, generation)?
        && proof.schema == 1
        && proof.generation == generation
        && proof.inventory == all_ids
        && proof
            .inputs
            .iter()
            .map(|(seq, _, _)| *seq)
            .collect::<Vec<_>>()
            == seqs
        && proof.provisional_sources.len() == provisional.len()
        && provisional
            .keys()
            .all(|id| proof.provisional_sources.contains_key(id))
        && inputs_match(ledger, &proof)
        && provisional_matches(ledger, &proof)
    {
        if record
            .as_ref()
            .and_then(|r| r.sealed_ledger_digest.as_ref())
            .is_some_and(|digest| *digest != proof.ledger_digest)
        {
            return Err(unverifiable_admission());
        }
        return finish(ledger, &proof);
    }
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
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        }
        provisional_sources.insert(id.clone(), derived.sources);
    }
    let mut rows = HashMap::new();
    let mut inputs = Vec::new();
    for seq in seqs {
        if Instant::now() >= authority.deadline {
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        }
        let batch_path = ledger.closure_recovery_batch_path(generation, seq);
        let ids_path = ledger.closure_recovery_ids_path(generation, seq);
        let batch_bytes = fs::read(&batch_path)?;
        let ids_bytes = fs::read(&ids_path)?;
        let batch_digest = digest_bytes(&batch_bytes);
        let ids_digest = digest_bytes(&ids_bytes);
        let prepared_path = dir.join(format!("audited-{seq:06}.json"));
        let stamp_path = dir.join(format!("audited-{seq:06}.sha256"));
        let batch: RecoveryBatchFile = serde_json::from_slice(&batch_bytes)?;
        let index: RecoveryIdsFile = serde_json::from_slice(&ids_bytes)?;
        let roster: Vec<_> = batch.entries.iter().map(|r| r.launch_id.clone()).collect();
        if batch.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
            || batch.generation != generation
            || batch.sealed_phase != ParentLossPhase::Sealed
            || batch.kind != RecoveryArtifactKind::Batch
            || batch.seq != seq
            || index.seq != seq
            || index.generation != generation
            || index.schema != PARENT_LOSS_RECOVERY_SCHEMA_V1
            || index.kind != RecoveryArtifactKind::Ids
            || index.sealed_phase != ParentLossPhase::Sealed
            || roster != index.ids
            || !batch
                .entries
                .iter()
                .all(|row| completed_row_is_authorized(ledger, generation, row))
        {
            // These are derived caches; preserve them and revisit original evidence next time.
            for path in [
                &batch_path,
                &ids_path,
                &ledger.closure_recovery_receipt_path(generation, seq),
            ] {
                if path.exists() {
                    set_aside(path)?;
                }
            }
            return Ok(ClosureOutcome::Refused(
                BootstrapRecoveryReason::CoordinatorNotLive,
            ));
        }
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
        let retained = fs::read(&prepared_path).ok().and_then(|bytes| {
            let stamp = fs::read_to_string(&stamp_path).ok()?;
            if digest_bytes(&bytes) != stamp {
                return None;
            }
            let chunk: PreparedChunk = serde_json::from_slice(&bytes).ok()?;
            (chunk.schema == 1
                && chunk.generation == generation
                && chunk.seq == seq
                && chunk.batch_digest == batch_digest
                && chunk.ids_digest == ids_digest)
                .then_some(chunk)
        });
        let chunk = if let Some(chunk) = retained.filter(|chunk| chunk.rows == prepared) {
            chunk
        } else {
            let chunk = PreparedChunk {
                schema: 1,
                generation,
                seq,
                batch_digest: batch_digest.clone(),
                ids_digest: ids_digest.clone(),
                rows: prepared,
            };
            // Preserve reusable bytes, but always compare them with authorized source projections.
            let bytes = serde_json::to_vec(&chunk)?;
            atomic_replace(
                &prepared_path,
                &bytes,
                AtomicWriteOptions { mode: Some(0o600) },
            )?;
            atomic_replace(
                &stamp_path,
                digest_bytes(&bytes).as_bytes(),
                AtomicWriteOptions { mode: Some(0o600) },
            )?;
            chunk
        };
        inputs.push((seq, batch_digest, ids_digest));
        for row in chunk.rows {
            if rows.insert(row.launch_id.clone(), row).is_some() {
                return Err(unverifiable_admission());
            }
        }
        seam("during-finalization");
    }
    for prov in provisional.values() {
        let closure = AdmissionClosure {
            launch_id: prov.launch_id.clone(),
            service: prov.service,
            instance: None,
            uid: None,
            finding: prov.finding,
        };
        if rows
            .insert(
                prov.launch_id.clone(),
                PreparedAdmission {
                    launch_id: prov.launch_id.clone(),
                    closure: serde_json::to_string(&closure)?,
                    sealed: None,
                },
            )
            .is_some()
        {
            return Err(unverifiable_admission());
        }
    }
    if rows.len() != all_ids.len() || inventory(ledger, generation)? != all_ids {
        return Err(unverifiable_admission());
    }
    if Instant::now() >= authority.deadline {
        return Ok(ClosureOutcome::Refused(
            BootstrapRecoveryReason::CoordinatorNotLive,
        ));
    }
    let mut closures = Vec::new();
    let mut sealed = Vec::new();
    for id in &all_ids {
        let row = rows.remove(id).ok_or_else(unverifiable_admission)?;
        closures.push(row.closure);
        if let Some(identity) = row.sealed {
            sealed.push(identity);
        }
    }
    let mut ledger_bytes = format!("[{}]\n", sealed.join(",")).into_bytes();
    let ledger_path = ledger.sealed_ledger_path(generation);
    if ledger_path.exists() {
        let existing_bytes = fs::read(&ledger_path)?;
        let existing: serde_json::Value = serde_json::from_slice(&existing_bytes)?;
        let expected: serde_json::Value = serde_json::from_slice(&ledger_bytes)?;
        if existing == expected {
            ledger_bytes = existing_bytes;
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
    let ledger_digest = digest_bytes(&ledger_bytes);
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
    let record_bytes = header
        .replacen(
            needle,
            &format!("\"admissions\":[{}]", closures.join(",")),
            1,
        )
        .into_bytes();
    // Establish exact expected record and ledger before publishing either one.
    atomic_replace(
        dir.join("ledger.prepared"),
        &ledger_bytes,
        AtomicWriteOptions { mode: Some(0o600) },
    )?;
    atomic_replace(
        dir.join("record.prepared"),
        &record_bytes,
        AtomicWriteOptions { mode: Some(0o600) },
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
        record_digest: digest_bytes(&record_bytes),
    };
    write_json(dir.join("publication.json"), &proof, json_options())?;
    #[cfg(test)]
    if PUBLICATION_STOP_FAULT.with(|fault| fault.get()) {
        return Ok(ClosureOutcome::Refused(
            BootstrapRecoveryReason::CoordinatorNotLive,
        ));
    }
    seam("before-publication");
    if Instant::now() >= authority.deadline {
        return Ok(ClosureOutcome::Refused(
            BootstrapRecoveryReason::CoordinatorNotLive,
        ));
    }
    finish(ledger, &proof)
}
