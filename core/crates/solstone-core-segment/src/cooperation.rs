// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Call-local participation in the existing journal activity exclusion.

use std::path::{Component, Path};
use std::sync::Arc;

use solstone_core_journal_io::activity::{ActivityAdmissionError, ActivityLease};
use solstone_core_journal_io::activity_obligations::{
    is_stage_namespace_name, observe_obligations,
};
use solstone_core_journal_io::{JournalRoot, contained_path};

use crate::segment_dir::STAGED_PREFIX;

pub(crate) struct Cooperation {
    activity: Arc<ActivityLease>,
}

impl Cooperation {
    pub(crate) fn admit(journal: &Path) -> Result<Self, String> {
        let root = JournalRoot::open(journal).map_err(|_| binding_hold())?;
        let activity = ActivityLease::try_acquire(Arc::new(root)).map_err(|error| match error {
            ActivityAdmissionError::Busy => activity_hold(),
            ActivityAdmissionError::Unavailable(_) => binding_hold(),
        })?;
        let guard = Self { activity };
        guard.population()?;
        Ok(guard)
    }

    pub(crate) fn journal(&self) -> &Path {
        self.activity.journal_root().canonical_path()
    }

    fn revalidate(&self) -> Result<(), String> {
        self.activity.revalidate().map_err(|_| binding_hold())?;
        self.activity
            .journal_root()
            .revalidate_canonical_binding()
            .map_err(|_| binding_hold())
    }

    pub(crate) fn population(&self) -> Result<(), String> {
        self.revalidate()?;
        let population =
            observe_obligations(self.activity.journal_root()).map_err(|_| binding_hold())?;
        if !population.entries().is_empty() {
            return Err(activity_hold());
        }
        self.revalidate()
    }

    pub(crate) fn target(&self, path: &Path) -> Result<(), String> {
        self.revalidate()?;
        let relative = path
            .strip_prefix(self.journal())
            .map_err(|_| binding_hold())?;
        let plain = relative.to_str().is_some_and(|text| {
            !text.is_empty()
                && text
                    .split(std::path::is_separator)
                    .all(|part| !part.is_empty() && part != "." && part != "..")
        });
        if !plain
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(name) if name.to_str().is_some_and(|name| !name.contains(['/', '\\']))))
        {
            return Err(binding_hold());
        }
        if relative.components().any(|part| {
            let mut name = part.as_os_str();
            loop {
                if is_stage_namespace_name(name) {
                    return true;
                }
                let Some(original) = name
                    .to_str()
                    .and_then(|name| name.strip_prefix(STAGED_PREFIX))
                else {
                    return false;
                };
                name = std::ffi::OsStr::new(original);
            }
        }) {
            return Err(
                "this location is reserved for a journal operation and cannot be changed"
                    .to_owned(),
            );
        }
        let relative = relative
            .components()
            .map(|part| part.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(binding_hold)?
            .join("/");
        let contained = contained_path(self.journal(), &relative).map_err(|_| binding_hold())?;
        if contained != path {
            return Err(binding_hold());
        }
        Ok(())
    }

    pub(crate) fn before_phase(&self, paths: &[&Path]) -> Result<(), String> {
        for path in paths {
            self.target(path)?;
        }
        self.population()
    }
}

fn activity_hold() -> String {
    "another journal operation needs attention before this change can continue".to_owned()
}

fn binding_hold() -> String {
    "the journal could not be checked, so this change is on hold".to_owned()
}

#[cfg(all(test, unix, feature = "full-tests"))]
pub(crate) mod tests;

#[cfg(all(test, feature = "full-tests"))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod path_tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn native_targets_keep_the_admitted_binding_and_reserved_names() {
        let root = TempDir::new();
        let guard = Cooperation::admit(root.path()).unwrap();
        let target = guard
            .journal()
            .join("chronicle")
            .join("20261014")
            .join("work")
            .join("120000_60");
        #[cfg(windows)]
        {
            let native = target
                .strip_prefix(guard.journal())
                .unwrap()
                .to_str()
                .unwrap();
            assert!(native.contains('\\'));
            assert!(contained_path(guard.journal(), native).is_err());
        }
        guard.target(&target).unwrap();
        assert!(guard.target(root.path().parent().unwrap()).is_err());
        assert!(guard.target(&guard.journal().join("../outside")).is_err());
        for reserved in [
            ".tmp-offload-owned",
            ".removing_.tmp-offload-owned",
            ".removing_.removing_.tmp-offload-owned",
        ] {
            assert!(guard.target(&guard.journal().join(reserved)).is_err());
        }
        #[cfg(unix)]
        assert!(
            guard
                .target(&guard.journal().join("literal\\name"))
                .is_err()
        );
    }
}
