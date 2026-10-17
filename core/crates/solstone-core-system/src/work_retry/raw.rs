// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Native analysis retry scope. A queued permission is rechecked and consumed
//! only by the matching native writer under its existing OutputClaims.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Attempt, DECLARED_FIXES, Unit};

pub const JOURNAL_ENV: &str = "SOLSTONE_RETRY_JOURNAL";
pub const UNIT_ENV: &str = "SOLSTONE_RETRY_UNIT";
pub const CAUSE_ENV: &str = "SOLSTONE_RETRY_CAUSE";
pub const ATTEMPT_ENV: &str = "SOLSTONE_RETRY_ATTEMPT";

#[derive(Clone, Debug, PartialEq)]
pub struct RetryContext {
    pub unit_id: String,
    pub attempt: Attempt,
}

pub struct Evidence {
    pub unit: Unit,
    pub reason: String,
    pub input: String,
    pub baseline: String,
    pub input_path: PathBuf,
}

fn component(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', '\0'])
}

pub fn unit_for_output(journal: &Path, output: &Path, handler: &str) -> Result<Unit, String> {
    let relative = output
        .strip_prefix(journal.join("chronicle"))
        .map_err(|e| e.to_string())?;
    let parts = relative
        .iter()
        .map(|p| p.to_str().ok_or("non-UTF8 retry coordinate"))
        .collect::<Result<Vec<_>, _>>()?;
    let (day, stream, segment, output, direct) = match parts.as_slice() {
        [day, segment, output] => (
            *day,
            solstone_core_journal_io::DEFAULT_STREAM,
            *segment,
            *output,
            true,
        ),
        [day, stream, segment, output] => (*day, *stream, *segment, *output, false),
        _ => return Err("invalid native retry coordinate".into()),
    };
    let unit = Unit::Raw {
        day: day.into(),
        stream: stream.into(),
        segment: segment.into(),
        handler: handler.into(),
        output: output.into(),
        direct,
    };
    unit.validate()?;
    Ok(unit)
}

/// The header stays bounded and read errors remain errors, never an absent
/// processing record that could manufacture a fresh attempt.
fn header(output: &Path) -> Result<Value, String> {
    bounded_header(output)?.ok_or_else(|| "native processing header is incomplete".into())
}

/// `None` when the first row fills the bound: like any sidecar, that file has no
/// readable record. A shorter row without its newline is still an error.
fn bounded_header(output: &Path) -> Result<Option<Value>, String> {
    let bound = solstone_core_processing_record::vocab::MAX_FIRST_ROW_BYTES;
    let file = std::fs::File::open(output).map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::new(file.take(bound as u64));
    let mut row = Vec::new();
    reader
        .read_until(b'\n', &mut row)
        .map_err(|e| e.to_string())?;
    if row.last() != Some(&b'\n') {
        if row.len() == bound {
            return Ok(None);
        }
        return Err("native processing header is incomplete".into());
    }
    let header: Value = serde_json::from_slice(&row).map_err(|e| e.to_string())?;
    if !header.is_object() {
        return Err("native processing header is not an object".into());
    }
    Ok(Some(header))
}

pub fn inspect_output(journal: &Path, output: &Path) -> Result<Option<Evidence>, String> {
    let Some(header) = bounded_header(output)? else {
        return Ok(None);
    };
    let Some(record) = header.get("_solstone_processing") else {
        return Ok(None);
    };
    if !record.is_object() {
        return Err("native processing record is malformed".into());
    }
    if record.get("schema").and_then(Value::as_str)
        != Some(solstone_core_processing_record::vocab::SCHEMA)
    {
        return Err("native processing schema is unrecognized".into());
    }
    if record.get("state").and_then(Value::as_str) != Some("failed")
        || !solstone_core_processing_record::is_failure_exhausted(record)
    {
        return Ok(None);
    }
    let handler = record
        .get("handler")
        .and_then(Value::as_str)
        .ok_or("native failure has no handler")?;
    let unit = unit_for_output(journal, output, handler)?;
    let raw = header
        .get("raw")
        .and_then(Value::as_str)
        .filter(|s| component(s))
        .ok_or("native failure has no safe source filename")?;
    let input_path = output
        .parent()
        .ok_or("native output has no parent")?
        .join(raw);
    if input_path.with_extension("jsonl") != output {
        return Err("native failure source does not match its output".into());
    }
    let input = super::file_fingerprint(&input_path)?;
    let baseline = match record.get("input_sha256") {
        None => input.clone(),
        Some(Value::String(value))
            if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            value.clone()
        }
        Some(_) => return Err("native failure input fingerprint is malformed".into()),
    };
    let reason = record
        .get("reason_code")
        .and_then(Value::as_str)
        .unwrap_or("analysis_failed")
        .to_owned();
    Ok(Some(Evidence {
        unit,
        reason,
        input,
        baseline,
        input_path,
    }))
}

pub fn observe(journal: &Path, evidence: &Evidence) -> Result<(), String> {
    super::observe_held(
        journal,
        &evidence.unit,
        &evidence.reason,
        &evidence.baseline,
    )
}

fn selected(
    journal: &Path,
    output: &Path,
    handler: &str,
) -> Result<Option<(Unit, Attempt)>, String> {
    let Some(id) = std::env::var_os(UNIT_ENV) else {
        return Ok(None);
    };
    if std::env::var_os(JOURNAL_ENV).as_deref() != Some(journal.as_os_str()) {
        return Err("native retry journal does not match".into());
    }
    let id = id.to_str().ok_or("native retry identity is not UTF-8")?;
    let cause = std::env::var(CAUSE_ENV).map_err(|e| e.to_string())?;
    let selected: Attempt =
        serde_json::from_str(&std::env::var(ATTEMPT_ENV).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let unit = unit_for_output(journal, output, handler)?;
    if unit.id()? != id {
        return Ok(None);
    }
    let evidence = inspect_output(journal, output)?.ok_or("native output is no longer held")?;
    if evidence.unit != unit {
        return Err("native failure handler changed".into());
    }
    let record = super::read(journal, id)?.ok_or("native retry permission is missing")?;
    let attempt = record
        .permission(&evidence.input, DECLARED_FIXES)
        .filter(|attempt| {
            attempt == &selected && attempt.cause == cause && attempt.reason == evidence.reason
        })
        .ok_or("native retry permission changed")?;
    Ok(Some((unit, attempt)))
}

/// Read-only eligibility for an output. None when not an exhausted held failure or permission() is None.
pub fn context_for_output(
    journal: &Path,
    output: &Path,
    handler: &str,
) -> Result<Option<RetryContext>, String> {
    let unit = unit_for_output(journal, output, handler)?;
    let evidence = match inspect_output(journal, output)? {
        Some(e) => e,
        None => return Ok(None),
    };
    if evidence.unit != unit {
        return Ok(None);
    }
    let record = match super::read(journal, &unit.id()?)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let attempt = match record.permission(&evidence.input, DECLARED_FIXES) {
        Some(a) => a,
        None => return Ok(None),
    };
    Ok(Some(RetryContext {
        unit_id: unit.id()?,
        attempt,
    }))
}

/// Preflight with explicit context. True only when unit id matches and permission is still current.
pub fn waiting_context(
    journal: &Path,
    output: &Path,
    handler: &str,
    context: &RetryContext,
) -> bool {
    let Ok(unit) = unit_for_output(journal, output, handler) else {
        return false;
    };
    let Ok(id) = unit.id() else {
        return false;
    };
    if id != context.unit_id {
        return false;
    }
    context_for_output(journal, output, handler)
        .is_ok_and(|c| c.as_ref().map(|c| &c.attempt) == Some(&context.attempt))
}

/// Preflight only. The native writer repeats this under OutputClaims.
pub fn waiting_from_environment(output: &Path, handler: &str) -> bool {
    let Some(journal) = std::env::var_os(JOURNAL_ENV) else {
        return false;
    };
    selected(Path::new(&journal), output, handler).is_ok_and(|grant| grant.is_some())
}

/// Never call before acquiring the native output execution claim.
pub fn consume_context(
    journal: &Path,
    output: &Path,
    handler: &str,
    context: &RetryContext,
) -> Result<Option<Attempt>, String> {
    let unit = unit_for_output(journal, output, handler)?;
    if unit.id()? != context.unit_id {
        return Ok(None);
    }
    let evidence = inspect_output(journal, output)?.ok_or("native output is no longer held")?;
    if evidence.unit != unit {
        return Err("native failure handler changed".into());
    }
    super::consume(
        journal,
        &unit,
        &context.attempt,
        &context.attempt.reason,
        &evidence.input,
        DECLARED_FIXES,
    )?;
    Ok(Some(context.attempt.clone()))
}

/// Never call before acquiring the native output execution claim.
pub fn consume_from_environment(
    journal: &Path,
    output: &Path,
    handler: &str,
) -> Result<bool, String> {
    let Some((unit, attempt)) = selected(journal, output, handler)? else {
        return Ok(false);
    };
    super::consume(
        journal,
        &unit,
        &attempt,
        &attempt.reason,
        &attempt.input,
        DECLARED_FIXES,
    )?;
    Ok(true)
}

pub fn consume_for_output(output: &Path, handler: &str) -> Result<bool, String> {
    let Some(journal) = std::env::var_os(JOURNAL_ENV) else {
        return Ok(false);
    };
    consume_from_environment(Path::new(&journal), output, handler)
}

/// Called by the owning native publication writer before saving its failed
/// processing record, while its existing input observation still protects it.
pub fn stamp_failed_input(record: &mut Value, input: &Path) -> Result<(), String> {
    stamp_attempt(record)?;
    if record.get("state").and_then(Value::as_str) == Some("failed") {
        let hash = super::file_fingerprint(input)?;
        record
            .as_object_mut()
            .ok_or("processing record is not an object")?
            .insert("input_sha256".into(), Value::String(hash));
    }
    Ok(())
}

pub fn stamp_failed_input_from(
    record: &mut Value,
    input: &Path,
    attempt: &Attempt,
) -> Result<(), String> {
    stamp_attempt_from(record, attempt)?;
    if record.get("state").and_then(Value::as_str) == Some("failed") {
        let hash = super::file_fingerprint(input)?;
        record
            .as_object_mut()
            .ok_or("processing record is not an object")?
            .insert("input_sha256".into(), Value::String(hash));
    }
    Ok(())
}

/// Bind terminal publication to the selected attempt, including successful output.
pub fn stamp_attempt(record: &mut Value) -> Result<(), String> {
    if let Ok(value) = std::env::var(ATTEMPT_ENV) {
        let attempt: Attempt = serde_json::from_str(&value).map_err(|e| e.to_string())?;
        record["retry_attempt"] = serde_json::to_value(attempt).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn stamp_attempt_from(record: &mut Value, attempt: &Attempt) -> Result<(), String> {
    record["retry_attempt"] = serde_json::to_value(attempt).map_err(|e| e.to_string())?;
    Ok(())
}

/// A child exiting successfully is not a terminal receipt. Require a consumed
/// matching attempt and a recognized processing disposition from the writer.
pub fn finish_output(journal: &Path, unit: &Unit, attempt: &Attempt) -> Result<(), String> {
    let Unit::Raw {
        output, handler, ..
    } = unit
    else {
        return Err("not native analysis".into());
    };
    let record = super::read(journal, &unit.id()?)?.ok_or("retry evidence is missing")?;
    if record.attempt.as_ref() != Some(attempt) {
        return Err("native writer did not consume the retry permission".into());
    }
    let output = unit.segment_directory(journal)?.join(output);
    let header = header(&output)?;
    let processing = header
        .get("_solstone_processing")
        .ok_or("native terminal receipt is missing")?;
    if processing.get("schema").and_then(Value::as_str)
        != Some(solstone_core_processing_record::vocab::SCHEMA)
        || processing.get("handler").and_then(Value::as_str) != Some(handler.as_str())
    {
        return Err("native terminal receipt changed".into());
    }
    if processing.get("retry_attempt")
        != Some(&serde_json::to_value(attempt).map_err(|e| e.to_string())?)
    {
        return Err("native writer has not published this retry's terminal receipt".into());
    }
    let remaining = match processing.get("state").and_then(Value::as_str) {
        Some("failed") => Some(
            processing
                .get("reason_code")
                .and_then(Value::as_str)
                .unwrap_or("analysis_failed"),
        ),
        Some("analyzed" | "empty") => None,
        _ => return Err("native terminal receipt is unreadable".into()),
    };
    super::finish(journal, unit, attempt, remaining)
}
