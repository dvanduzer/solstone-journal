// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bind owner-rendered actions to their retained journal and complete proposals.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use solstone_core_journal_io::errors::FlatDirectoryError;
use solstone_core_journal_io::journal_root::is_canonical_identity_observation;
#[cfg(unix)]
use solstone_core_journal_io::{
    FlatDirectory as HealthDirectory, open_flat_directory_bound as open_health,
    read_observed_file_bounded as read_register,
};
use solstone_core_journal_io::{JournalRoot, JournalRootError};
#[cfg(windows)]
use solstone_core_journal_io::{
    WindowsFlatDirectory as HealthDirectory, open_windows_flat_directory_bound as open_health,
    read_windows_observed_file_bounded as read_register,
};

use crate::marks::{Approval, Mark, MarkId, MarkState, Register, StoreError, validate};

/// Maximum register size admitted by the retained reader and writer.
pub const MAX_REGISTER_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum ReviewRefusal {
    Empty,
    Duplicate,
    Missing { id: MarkId },
    Stale { id: MarkId },
    Failed { id: MarkId },
    NotRequired { id: MarkId },
    SourceRootChanged { id: MarkId },
    RootChanged,
    Root(JournalRootError),
    Read(FlatDirectoryError),
    Register(StoreError),
}

/// Rendered action data. Deserialization does not grant current authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewContext {
    root_observation: String,
    marks: Vec<Mark>,
}

impl ReviewContext {
    pub fn root_observation(&self) -> &str {
        &self.root_observation
    }
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    /// Select complete rows from this rendered context without reading newer
    /// proposals. Current validation remains the checked entry's responsibility.
    pub fn select(&self, ids: &[MarkId]) -> Result<Self, ReviewRefusal> {
        if ids.is_empty() {
            return Err(ReviewRefusal::Empty);
        }
        let mut unique = BTreeSet::new();
        let mut marks = Vec::with_capacity(ids.len());
        for id in ids {
            if !unique.insert(id) {
                return Err(ReviewRefusal::Duplicate);
            }
            let mut matching = self.marks.iter().filter(|mark| &mark.id == id);
            let selected = matching
                .next()
                .ok_or_else(|| ReviewRefusal::Missing { id: id.clone() })?;
            if matching.next().is_some() {
                return Err(ReviewRefusal::Duplicate);
            }
            marks.push(selected.clone());
        }
        Ok(Self {
            root_observation: self.root_observation.clone(),
            marks,
        })
    }
}

/// Fresh action preflight that retains the root. Execution must still recheck
/// processing, input, consent and archive evidence at its destructive boundary.
pub struct CheckedReviewContext {
    root: Arc<JournalRoot>,
    health: HealthDirectory,
    marks: Vec<Mark>,
}

impl CheckedReviewContext {
    #[cfg(any(unix, windows))]
    pub(crate) fn root_arc(&self) -> &Arc<JournalRoot> {
        &self.root
    }

    pub fn root(&self) -> &JournalRoot {
        &self.root
    }
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }
    pub(crate) fn health(&self) -> &HealthDirectory {
        &self.health
    }
}

pub(crate) fn revalidate_health(
    root: &JournalRoot,
    health: &HealthDirectory,
) -> Result<(), ReviewRefusal> {
    root.revalidate_canonical_binding()
        .map_err(ReviewRefusal::Root)?;
    let current = open_health(root, OsStr::new("health"), root.canonical_path())
        .map_err(ReviewRefusal::Read)?;
    if current.is_none_or(|current| current.identity() != health.identity()) {
        return Err(ReviewRefusal::RootChanged);
    }
    root.revalidate_canonical_binding()
        .map_err(ReviewRefusal::Root)
}

pub(crate) fn read_at_health(health: &HealthDirectory) -> Result<Option<Register>, ReviewRefusal> {
    let observed = read_register(
        health,
        OsStr::new("retention-marks.json"),
        MAX_REGISTER_BYTES,
    )
    .map_err(ReviewRefusal::Read)?;
    match observed {
        Some(observed) => {
            let register = serde_json::from_slice(&observed.bytes)
                .map_err(|error| ReviewRefusal::Register(StoreError::Malformed(error)))?;
            validate(&register).map_err(ReviewRefusal::Register)?;
            Ok(Some(register))
        }
        None => Ok(None),
    }
}

fn load_bound_at_root(
    root: &JournalRoot,
) -> Result<(Register, Option<HealthDirectory>), ReviewRefusal> {
    root.revalidate_canonical_binding()
        .map_err(ReviewRefusal::Root)?;
    let health = open_health(root, OsStr::new("health"), root.canonical_path())
        .map_err(ReviewRefusal::Read)?;
    let register = match &health {
        Some(health) => {
            let register = read_at_health(health)?.unwrap_or_else(Register::empty);
            revalidate_health(root, health)?;
            register
        }
        None => Register::empty(),
    };
    root.revalidate_canonical_binding()
        .map_err(ReviewRefusal::Root)?;
    Ok((register, health))
}

/// Strict retained-root register observation, without current release authority.
pub fn load_at_root(root: &JournalRoot) -> Result<Register, ReviewRefusal> {
    load_bound_at_root(root).map(|(register, _health)| register)
}

fn check_source_root(mark: &Mark, root_observation: &str) -> Result<(), ReviewRefusal> {
    if mark
        .offload
        .as_ref()
        .is_some_and(|offer| offer.proof.journal_id != root_observation)
    {
        return Err(ReviewRefusal::SourceRootChanged {
            id: mark.id.clone(),
        });
    }
    Ok(())
}

pub fn read_review_context(
    root: &JournalRoot,
    ids: &[MarkId],
) -> Result<ReviewContext, ReviewRefusal> {
    let register = load_at_root(root)?;
    let root_observation = root.identity().observation_id();
    let mut unique = BTreeSet::new();
    let mut marks = Vec::with_capacity(ids.len());
    for id in ids {
        if !unique.insert(id) {
            return Err(ReviewRefusal::Duplicate);
        }
        let mark = register
            .marks
            .get(id)
            .ok_or_else(|| ReviewRefusal::Missing { id: id.clone() })?;
        check_source_root(mark, &root_observation)?;
        marks.push(mark.clone());
    }
    root.revalidate_canonical_binding()
        .map_err(ReviewRefusal::Root)?;
    Ok(ReviewContext {
        root_observation,
        marks,
    })
}

pub fn check_review_context(
    root: Arc<JournalRoot>,
    context: &ReviewContext,
) -> Result<CheckedReviewContext, ReviewRefusal> {
    if context.marks.is_empty() {
        return Err(ReviewRefusal::Empty);
    }
    if !is_canonical_identity_observation(&context.root_observation)
        || root.identity().observation_id() != context.root_observation
    {
        return Err(ReviewRefusal::RootChanged);
    }
    let (register, health) = load_bound_at_root(&root)?;
    check_marks(&register, &context.marks, &context.root_observation)?;
    let health = health.ok_or(ReviewRefusal::RootChanged)?;
    revalidate_health(&root, &health)?;
    Ok(CheckedReviewContext {
        root,
        health,
        marks: context.marks.clone(),
    })
}

pub(crate) fn check_marks(
    register: &Register,
    marks: &[Mark],
    root_observation: &str,
) -> Result<(), ReviewRefusal> {
    if marks.is_empty() {
        return Err(ReviewRefusal::Empty);
    }
    let mut unique = BTreeSet::new();
    for expected in marks {
        if !unique.insert(&expected.id) {
            return Err(ReviewRefusal::Duplicate);
        }
        let current = register
            .marks
            .get(&expected.id)
            .ok_or_else(|| ReviewRefusal::Missing {
                id: expected.id.clone(),
            })?;
        if current != expected {
            return Err(ReviewRefusal::Stale {
                id: expected.id.clone(),
            });
        }
        if current.class.axes().1 != Approval::Required {
            return Err(ReviewRefusal::NotRequired {
                id: expected.id.clone(),
            });
        }
        if current.state != MarkState::Marked {
            return Err(ReviewRefusal::Failed {
                id: expected.id.clone(),
            });
        }
        check_source_root(current, root_observation)?;
    }
    Ok(())
}
