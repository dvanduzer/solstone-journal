// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Durable owner intent. Source files and accepted records still decide owed work.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::durability::{ArtifactId, artifact};
use solstone_core_journal_io::{JsonWriteOptions, LockOptions, hold_lock, write_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentState {
    Queued,
    Claimed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerIntent {
    pub id: String,
    pub from_scratch: bool,
    pub state: IntentState,
}

#[derive(Serialize, Deserialize)]
struct Store {
    version: u32,
    days: BTreeMap<String, OwnerIntent>,
}

fn valid_day(day: &str) -> bool {
    day.len() == 8
        && day.bytes().all(|c| c.is_ascii_digit())
        && chrono::NaiveDate::parse_from_str(day, "%Y%m%d").is_ok()
}

pub fn read(journal: &Path) -> Result<BTreeMap<String, OwnerIntent>, String> {
    let bytes = match std::fs::read(journal.join(artifact(ArtifactId::WorkIntent).path)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.to_string()),
    };
    let store: Store = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if store.version != 1
        || store.days.iter().any(|(day, intent)| {
            !valid_day(day)
                || intent.id.len() != 32
                || !intent.id.bytes().all(|c| c.is_ascii_hexdigit())
        })
    {
        return Err("unrecognized work intent".into());
    }
    Ok(store.days)
}

fn mutate<T>(
    journal: &Path,
    update: impl FnOnce(&mut BTreeMap<String, OwnerIntent>) -> Result<T, String>,
) -> Result<T, String> {
    mutate_checked(journal, &|| Ok(()), update)
}

fn mutate_checked<T>(
    journal: &Path,
    checkpoint: &dyn Fn() -> Result<(), String>,
    update: impl FnOnce(&mut BTreeMap<String, OwnerIntent>) -> Result<T, String>,
) -> Result<T, String> {
    checkpoint()?;
    let path = journal.join(artifact(ArtifactId::WorkIntent).path);
    std::fs::create_dir_all(path.parent().expect("health parent"))
        .map_err(|error| error.to_string())?;
    let _lock = hold_lock(path.with_extension("lock"), LockOptions::default())
        .map_err(|error| error.to_string())?;
    let mut days = read(journal)?;
    checkpoint()?;
    let result = update(&mut days)?;
    checkpoint()?;
    write_json(
        path,
        &Store { version: 1, days },
        JsonWriteOptions::default(),
    )
    .map_err(|error| error.to_string())?;
    Ok(result)
}

pub fn register(journal: &Path, day: &str, from_scratch: bool) -> Result<OwnerIntent, String> {
    if !valid_day(day) {
        return Err("invalid work day".into());
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let intent = OwnerIntent {
        id: random.iter().map(|byte| format!("{byte:02x}")).collect(),
        from_scratch,
        state: IntentState::Queued,
    };
    mutate(journal, |days| {
        days.insert(day.to_owned(), intent.clone());
        Ok(intent)
    })
}

/// No destructive work may start unless this write succeeds. A claimed intent
/// remains held after a crash; it cannot be automatically claimed again.
pub fn claim_from_scratch(journal: &Path, day: &str, id: &str) -> Result<(), String> {
    mutate(journal, |days| {
        let intent = days
            .get_mut(day)
            .filter(|intent| {
                intent.id == id && intent.from_scratch && intent.state == IntentState::Queued
            })
            .ok_or("destructive request is no longer waiting")?;
        intent.state = IntentState::Claimed;
        Ok(())
    })
}

pub fn finish_from_scratch(
    journal: &Path,
    day: &str,
    id: &str,
    success: bool,
) -> Result<(), String> {
    finish_from_scratch_checked(journal, day, id, success, &|| Ok(()))
}

pub fn finish_from_scratch_checked(
    journal: &Path,
    day: &str,
    id: &str,
    success: bool,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<(), String> {
    mutate_checked(journal, checkpoint, |days| {
        if let Some(intent) = days.get_mut(day).filter(|intent| {
            intent.id == id && intent.from_scratch && intent.state == IntentState::Claimed
        }) {
            if success {
                days.remove(day);
            } else {
                intent.state = IntentState::Failed;
            }
        }
        Ok(())
    })
}

/// Called only after the discovery reader has verified current source coverage.
pub fn retire_current(journal: &Path, day: &str, id: &str) -> Result<(), String> {
    retire_current_checked(journal, day, id, &|| Ok(()))
}

pub fn retire_current_checked(
    journal: &Path,
    day: &str,
    id: &str,
    checkpoint: &dyn Fn() -> Result<(), String>,
) -> Result<(), String> {
    mutate_checked(journal, checkpoint, |days| {
        if days
            .get(day)
            .is_some_and(|intent| intent.id == id && !intent.from_scratch)
        {
            days.remove(day);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_attempt_never_repeats_and_old_terminal_cannot_remove_new_intent() {
        let root = tempfile::tempdir().unwrap();
        let first = register(root.path(), "20260101", true).unwrap();
        claim_from_scratch(root.path(), "20260101", &first.id).unwrap();
        assert!(claim_from_scratch(root.path(), "20260101", &first.id).is_err());
        assert_eq!(
            read(root.path()).unwrap()["20260101"].state,
            IntentState::Claimed
        );
        let second = register(root.path(), "20260101", true).unwrap();
        finish_from_scratch(root.path(), "20260101", &first.id, true).unwrap();
        assert_eq!(read(root.path()).unwrap()["20260101"], second);
        claim_from_scratch(root.path(), "20260101", &second.id).unwrap();
        finish_from_scratch(root.path(), "20260101", &second.id, false).unwrap();
        assert!(claim_from_scratch(root.path(), "20260101", &second.id).is_err());
        assert_eq!(
            read(root.path()).unwrap()["20260101"].state,
            IntentState::Failed
        );
    }

    #[test]
    fn interrupted_from_scratch_finish_stays_claimed() {
        let root = tempfile::tempdir().unwrap();
        let intent = register(root.path(), "20260101", true).unwrap();
        claim_from_scratch(root.path(), "20260101", &intent.id).unwrap();
        let error = finish_from_scratch_checked(root.path(), "20260101", &intent.id, true, &|| {
            Err("stopped".into())
        })
        .unwrap_err();
        assert_eq!(error, "stopped");
        assert_eq!(
            read(root.path()).unwrap()["20260101"].state,
            IntentState::Claimed
        );
        let error =
            finish_from_scratch_checked(root.path(), "20260101", &intent.id, false, &|| {
                Err("stopped".into())
            })
            .unwrap_err();
        assert_eq!(error, "stopped");
        assert_eq!(
            read(root.path()).unwrap()["20260101"].state,
            IntentState::Claimed
        );
    }

    #[test]
    fn malformed_intent_is_preserved_and_cannot_authorize_work() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("health")).unwrap();
        let path = root.path().join("health/work-intent.json");
        std::fs::write(&path, "{").unwrap();
        assert!(register(root.path(), "20260101", true).is_err());
        assert!(claim_from_scratch(root.path(), "20260101", "request").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{");
    }
}
