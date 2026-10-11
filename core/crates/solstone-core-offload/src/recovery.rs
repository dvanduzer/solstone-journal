// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Strict intent semantics over the sole native stage/intent observer. Saved
//! records, including serialized-complete records, grant no replay or cleanup.

use std::path::PathBuf;
use std::sync::Arc;

use solstone_core_journal_io::JournalRoot;
use solstone_core_journal_io::activity_obligations::{
    IntentRead, NativeObligation, RecoveryObservationError,
};
use solstone_core_journal_io::media::MediaEntry;
use solstone_core_retention::restore_intent::RestoreIntent;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentProblem {
    UnknownEntry,
    NotPlainFile,
    Oversized,
    InvalidRecord,
    OperationNameMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntentObservation {
    Recorded { operation_id: String },
    Uncertain(IntentProblem),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryObligation {
    Stage {
        relative: PathBuf,
        entry: MediaEntry,
    },
    Intent {
        relative: PathBuf,
        entry: MediaEntry,
        observation: IntentObservation,
    },
}

/// Namespace evidence only. Even serialized-complete records remain obligations.
#[derive(Debug)]
pub struct RecoveryObligations {
    entries: Vec<RecoveryObligation>,
}

impl RecoveryObligations {
    pub fn entries(&self) -> &[RecoveryObligation] {
        &self.entries
    }
}

/// Strict semantic projection of the sole complete native namespace observer.
/// Complete serialized records remain holds, including copied-root evidence.
pub fn observe_obligations(
    root: &Arc<JournalRoot>,
) -> Result<RecoveryObligations, RecoveryObservationError> {
    let raw = solstone_core_journal_io::activity_obligations::observe_obligations(root)?;
    let entries = raw
        .into_entries()
        .into_iter()
        .map(|record| match record {
            NativeObligation::Stage { relative, entry } => {
                RecoveryObligation::Stage { relative, entry }
            }
            NativeObligation::Intent {
                relative,
                entry,
                contents,
            } => {
                let observation = match contents {
                    IntentRead::UnknownEntry => {
                        IntentObservation::Uncertain(IntentProblem::UnknownEntry)
                    }
                    IntentRead::NotPlainFile => {
                        IntentObservation::Uncertain(IntentProblem::NotPlainFile)
                    }
                    IntentRead::Oversized => IntentObservation::Uncertain(IntentProblem::Oversized),
                    IntentRead::Complete {
                        operation_id,
                        bytes,
                    } => match RestoreIntent::decode(Ok(&bytes)) {
                        Ok(intent) if intent.event.operation_id == operation_id => {
                            IntentObservation::Recorded { operation_id }
                        }
                        Ok(_) => IntentObservation::Uncertain(IntentProblem::OperationNameMismatch),
                        Err(_) => IntentObservation::Uncertain(IntentProblem::InvalidRecord),
                    },
                };
                RecoveryObligation::Intent {
                    relative,
                    entry,
                    observation,
                }
            }
        })
        .collect();
    Ok(RecoveryObligations { entries })
}

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use super::*;
    use solstone_core_journal_io::activity_obligations::MAX_INTENT_BYTES;
    use std::fs;

    use crate::test_restore_intent::fixture;
    use solstone_core_retention::archive_catalog::RestoreDisposition;
    use solstone_core_retention::restore_intent::{ConfirmedContent, Obligation};

    struct Fixture {
        temp: tempfile::TempDir,
        root: Arc<JournalRoot>,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = match std::env::var_os("SOLSTONE_MEDIA_TEST_ROOT") {
                Some(root) => tempfile::tempdir_in(root).unwrap(),
                None => tempfile::tempdir().unwrap(),
            };
            let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
            Self { temp, root }
        }
        fn path(&self, relative: &str) -> PathBuf {
            self.temp.path().join(relative)
        }
        fn directory(&self, relative: &str) {
            fs::create_dir_all(self.path(relative)).unwrap();
        }
        fn observed(&self) -> Result<RecoveryObligations, RecoveryObservationError> {
            observe_obligations(&self.root)
        }
        fn intents(&self) -> PathBuf {
            self.directory("health/offload/restore-intents");
            self.path("health/offload/restore-intents")
        }
    }

    #[test]
    fn strict_pending_complete_and_copied_root_intents_all_hold() {
        for complete in [false, true] {
            let f = Fixture::new();
            let mut intent = fixture(false);
            assert_ne!(intent.destination.root, f.root.identity().observation_id());
            if complete {
                intent.event.files[0].disposition = RestoreDisposition::AlreadyPresent;
                intent.confirmed = Some(ConfirmedContent {
                    identity: intent.stage.leaf,
                    digest: intent.expected_digest(),
                });
                intent.marker = Obligation::Complete;
                intent.event_recorded = Obligation::Complete;
                intent.cleanup = Obligation::Complete;
            }
            let bytes = intent.encode().unwrap();
            let path = f
                .intents()
                .join(format!("{}.json", intent.event.operation_id));
            fs::write(&path, &bytes).unwrap();
            let observed = f.observed().unwrap();
            assert!(matches!(observed.entries(), [RecoveryObligation::Intent {
                observation: IntentObservation::Recorded { operation_id }, ..
            }] if operation_id == &intent.event.operation_id));
            drop(observed);
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn torn_unknown_oversized_and_unrecognized_carriers_are_preserved_holds() {
        let f = Fixture::new();
        let intent = fixture(false);
        let bytes = intent.encode().unwrap();
        let directory = f.intents();
        let path = directory.join(format!("{}.json", intent.event.operation_id));
        let mut unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        unknown["future"] = serde_json::json!(1);
        let mut version: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        version["version"] = serde_json::json!(999);
        for data in [
            bytes[..bytes.len() - 1].to_vec(),
            serde_json::to_vec(&unknown).unwrap(),
            serde_json::to_vec(&version).unwrap(),
            vec![b' '; MAX_INTENT_BYTES + 1],
        ] {
            fs::write(&path, &data).unwrap();
            assert!(matches!(
                f.observed().unwrap().entries(),
                [RecoveryObligation::Intent {
                    observation: IntentObservation::Uncertain(_),
                    ..
                }]
            ));
            assert_eq!(fs::read(&path).unwrap(), data);
        }
        fs::remove_file(&path).unwrap();
        fs::write(directory.join("unknown.lock"), b"unknown").unwrap();
        fs::create_dir(directory.join("empty")).unwrap();
        fs::write(
            directory.join("00000000000000000000000000000000.json"),
            &bytes,
        )
        .unwrap();
        let observed = f.observed().unwrap();
        assert_eq!(observed.entries().len(), 3);
        assert!(observed.entries().iter().any(|entry| matches!(entry,
            RecoveryObligation::Intent {
                entry,
                observation: IntentObservation::Uncertain(IntentProblem::OperationNameMismatch),
                ..
            } if entry.name == "00000000000000000000000000000000.json"
        )));
    }
}
