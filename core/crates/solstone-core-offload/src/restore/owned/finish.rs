// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Consuming settlement preserves receipts and keeps every uncertain owner closed.

use super::*;
use crate::restore_intent_store::transaction::{SettledTransaction, SettlementFailure};
use solstone_core_backup::archive_readback::{
    HeldDirectRead, OwnedDirectReadHold, OwnedHeldDirectRead, OwnedSettledDirectRead,
    ReadbackError, SettledDirectRead,
};
use solstone_core_journal_io::media::RootActivityCarrier;
use solstone_core_retention::restore_intent::AttemptEffects;

pub enum RestoreFinishRefusal<'a> {
    Native {
        file: Option<usize>,
        reason: &'static str,
        source: Option<&'a std::io::Error>,
        readback: Option<ReadbackError>,
    },
    Lower(ReadbackError),
}

struct NativeRefusal {
    file: Option<usize>,
    failure: SettlementFailure,
    readback: Option<ReadbackError>,
}

impl NativeRefusal {
    fn facts(&self) -> RestoreFinishRefusal<'_> {
        RestoreFinishRefusal::Native {
            file: self.file,
            reason: self.failure.reason,
            source: self.failure.source.as_ref(),
            readback: self.readback,
        }
    }
}

enum HeldOwner<'clock> {
    Native {
        outcome: Box<RestoreRunOutcome<'clock>>,
        refusal: NativeRefusal,
    },
    Lower {
        report: RestoreResult,
        files: Vec<FileRecord>,
        read: Box<HeldDirectRead<'clock>>,
    },
}

#[must_use = "retain every native and lower owner with the outside runtime finish owner"]
pub struct HeldRestoreFinish<'clock> {
    owner: HeldOwner<'clock>,
}

enum OwnedRead {
    Native {
        read: Box<OwnedDirectReadHold>,
        refusal: NativeRefusal,
    },
    Lower(OwnedHeldDirectRead),
}

#[must_use = "closed custody must survive worker exit and rejected handoff"]
pub struct OwnedRestoreFinishHold {
    report: RestoreResult,
    files: Vec<FileRecord>,
    _binding: Option<QualifiedArchiveRecoveryBinding>,
    read: OwnedRead,
}

struct SettledFile {
    _detail: usize,
    day: String,
    stream: String,
    segment: String,
    witness: ArchiveWitness,
    disposition: RestoreDisposition,
    reason: Option<&'static str>,
    files: u64,
    bytes: u64,
    transaction: Option<Box<SettledTransaction>>,
}

/// Original receipts/data only. Retain this alongside the outside Runtime result.
#[must_use]
pub struct SettledRestoreReport {
    report: RestoreResult,
    files: Vec<SettledFile>,
}

fn file_facts(file: &FileRecord) -> RestoreFileFacts<'_> {
    RestoreFileFacts {
        day: &file.day,
        stream: &file.stream,
        segment: &file.segment,
        witness: &file.witness,
        disposition: file.disposition,
        reason: file.reason,
        files_restored: file.files,
        bytes_restored: file.bytes,
        evidence: match &file.owner {
            FileOwner::None => FileEvidence::NotAttempted,
            FileOwner::Staging(failure) => FileEvidence::Staging(failure.facts()),
            FileOwner::Transaction(outcome) => FileEvidence::Transaction(Box::new(outcome.facts())),
        },
    }
}

impl<'clock> RestoreRunOutcome<'clock> {
    pub fn settle_direct(
        mut self,
    ) -> Result<(SettledDirectRead<'clock>, SettledRestoreReport), Box<HeldRestoreFinish<'clock>>>
    {
        if let Err(refusal) = recovery_check(&self._operation, self._binding.as_ref())
            .and_then(|()| qualify_native(&self.files, &self._operation))
        {
            return Err(Box::new(HeldRestoreFinish {
                owner: HeldOwner::Native {
                    outcome: Box::new(self),
                    refusal,
                },
            }));
        }
        if let Err(refusal) =
            cleanup_native(&mut self.files, &self._operation, self._binding.as_ref())
        {
            return Err(Box::new(HeldRestoreFinish {
                owner: HeldOwner::Native {
                    outcome: Box::new(self),
                    refusal,
                },
            }));
        }
        let Self {
            report,
            _operation: operation,
            _binding: binding,
            files,
        } = self;
        drop(binding);
        match operation.settle_direct() {
            Err(read) => Err(Box::new(HeldRestoreFinish {
                owner: HeldOwner::Lower {
                    report,
                    files,
                    read,
                },
            })),
            Ok(read) => {
                let files = settled_files(files);
                Ok((read, SettledRestoreReport { report, files }))
            }
        }
    }
}

fn qualify_native(
    files: &[FileRecord],
    operation: &ReadOperation<'_>,
) -> Result<(), NativeRefusal> {
    for (index, file) in files.iter().enumerate() {
        let failure = match &file.owner {
            FileOwner::None
                if file.files == 0
                    && file.bytes == 0
                    && file.disposition == RestoreDisposition::Unattempted =>
            {
                None
            }
            FileOwner::None | FileOwner::Staging(_) => {
                Some(SettlementFailure::refused("unsettled_file"))
            }
            FileOwner::Transaction(transaction) => transaction
                .qualify_settlement(
                    operation.carrier(),
                    &file.witness,
                    file.disposition,
                    AttemptEffects {
                        files: file.files,
                        bytes: file.bytes,
                    },
                )
                .and_then(|()| transaction.revalidate_settlement(operation.carrier()))
                .err(),
        };
        if let Some(failure) = failure {
            return Err(NativeRefusal {
                file: Some(index),
                failure,
                readback: None,
            });
        }
    }
    operation
        .revalidate_admission()
        .map_err(|readback| NativeRefusal {
            file: None,
            failure: SettlementFailure::refused("settlement_read_refused"),
            readback: Some(readback),
        })
}

impl<'clock> HeldRestoreFinish<'clock> {
    pub fn report(&self) -> &RestoreResult {
        match &self.owner {
            HeldOwner::Native { outcome, .. } => &outcome.report,
            HeldOwner::Lower { report, .. } => report,
        }
    }
    pub fn files(&self) -> impl ExactSizeIterator<Item = RestoreFileFacts<'_>> {
        let files = match &self.owner {
            HeldOwner::Native { outcome, .. } => &outcome.files,
            HeldOwner::Lower { files, .. } => files,
        };
        files.iter().map(file_facts)
    }
    pub fn refusal(&self) -> RestoreFinishRefusal<'_> {
        match &self.owner {
            HeldOwner::Native { refusal, .. } => refusal.facts(),
            HeldOwner::Lower { read, .. } => RestoreFinishRefusal::Lower(read.reason()),
        }
    }
    pub fn carrier(&self) -> &Arc<RootActivityCarrier> {
        match &self.owner {
            HeldOwner::Native { outcome, .. } => outcome._operation.carrier(),
            HeldOwner::Lower { read, .. } => read.carrier(),
        }
    }
    pub fn into_owned_hold(self) -> Result<OwnedRestoreFinishHold, Box<Self>> {
        match self.owner {
            HeldOwner::Native { outcome, refusal } => {
                let RestoreRunOutcome {
                    report,
                    files,
                    _operation: operation,
                    _binding: binding,
                } = *outcome;
                match operation.into_owned_hold() {
                    Ok(read) => Ok(OwnedRestoreFinishHold {
                        report,
                        files,
                        _binding: binding,
                        read: OwnedRead::Native {
                            read: Box::new(read),
                            refusal,
                        },
                    }),
                    Err(operation) => Err(Box::new(Self {
                        owner: HeldOwner::Native {
                            outcome: Box::new(RestoreRunOutcome {
                                report,
                                files,
                                _operation: *operation,
                                _binding: binding,
                            }),
                            refusal,
                        },
                    })),
                }
            }
            HeldOwner::Lower {
                report,
                files,
                read,
            } => match read.into_owned_hold() {
                Ok(read) => Ok(OwnedRestoreFinishHold {
                    report,
                    files,
                    _binding: None,
                    read: OwnedRead::Lower(read),
                }),
                Err(read) => Err(Box::new(Self {
                    owner: HeldOwner::Lower {
                        report,
                        files,
                        read,
                    },
                })),
            },
        }
    }
}

impl OwnedRestoreFinishHold {
    pub fn report(&self) -> &RestoreResult {
        &self.report
    }
    pub fn files(&self) -> impl ExactSizeIterator<Item = RestoreFileFacts<'_>> {
        self.files.iter().map(file_facts)
    }
    pub fn refusal(&self) -> RestoreFinishRefusal<'_> {
        match &self.read {
            OwnedRead::Lower(read) => RestoreFinishRefusal::Lower(read.reason()),
            OwnedRead::Native { refusal, .. } => refusal.facts(),
        }
    }
    pub fn carrier(&self) -> &Arc<RootActivityCarrier> {
        match &self.read {
            OwnedRead::Native { read, .. } => read.carrier(),
            OwnedRead::Lower(read) => read.carrier(),
        }
    }
}

impl SettledRestoreReport {
    pub fn report(&self) -> &RestoreResult {
        &self.report
    }
    pub fn files(&self) -> impl ExactSizeIterator<Item = RestoreFileFacts<'_>> {
        self.files.iter().map(|file| RestoreFileFacts {
            day: &file.day,
            stream: &file.stream,
            segment: &file.segment,
            witness: &file.witness,
            disposition: file.disposition,
            reason: file.reason,
            files_restored: file.files,
            bytes_restored: file.bytes,
            evidence: file
                .transaction
                .as_ref()
                .map_or(FileEvidence::NotAttempted, |transaction| {
                    FileEvidence::Transaction(Box::new(transaction.facts()))
                }),
        })
    }
}

fn recovery_check<'clock>(
    operation: &impl RestoreRead<'clock>,
    binding: Option<&QualifiedArchiveRecoveryBinding>,
) -> Result<(), NativeRefusal> {
    check_recovery(operation, binding).map_err(|_| NativeRefusal {
        file: None,
        failure: SettlementFailure::refused("settlement_recovery_binding_changed"),
        readback: None,
    })
}

fn cleanup_native<'clock>(
    files: &mut [FileRecord],
    operation: &impl RestoreRead<'clock>,
    binding: Option<&QualifiedArchiveRecoveryBinding>,
) -> Result<(), NativeRefusal> {
    recovery_check(operation, binding)?;
    let carrier = operation.as_read().carrier();
    // Inspect all files first; retain every earlier successful cleanup on a later refusal.
    for (index, file) in files.iter_mut().enumerate() {
        recovery_check(operation, binding)?;
        if let FileOwner::Transaction(transaction) = &mut file.owner
            && let Err(failure) = transaction
                .revalidate_settlement(carrier)
                .and_then(|()| transaction.cleanup_settlement())
                .and_then(|()| transaction.revalidate_settlement(carrier))
        {
            return Err(NativeRefusal {
                file: Some(index),
                failure,
                readback: None,
            });
        }
        recovery_check(operation, binding)?;
    }
    // Last original-token observation precedes release and lower settlement.
    recovery_check(operation, binding)
}

fn settled_files(files: Vec<FileRecord>) -> Vec<SettledFile> {
    files
        .into_iter()
        .map(|file| {
            let FileRecord {
                detail,
                day,
                stream,
                segment,
                witness,
                disposition,
                reason,
                files,
                bytes,
                owner,
            } = file;
            let transaction = match owner {
                FileOwner::Transaction(transaction) => Some(Box::new(transaction.into_settled())),
                // Qualification excludes Staging; no cleanup is inferred on consumption.
                FileOwner::None | FileOwner::Staging(_) => None,
            };
            SettledFile {
                _detail: detail,
                day,
                stream,
                segment,
                witness,
                disposition,
                reason,
                files,
                bytes,
                transaction,
            }
        })
        .collect()
}

impl OwnedRestoreRunOutcome {
    pub fn settle_direct(
        mut self,
    ) -> Result<(OwnedSettledDirectRead, SettledRestoreReport), Box<OwnedRestoreFinishHold>> {
        let qualified = recovery_check(&self._operation, self._binding.as_ref())
            .and_then(|()| qualify_native(&self.files, self._operation.as_read()));
        if let Err(refusal) = qualified.and_then(|()| {
            cleanup_native(&mut self.files, &self._operation, self._binding.as_ref())
        }) {
            return Err(Box::new(OwnedRestoreFinishHold {
                report: self.report,
                files: self.files,
                _binding: self._binding,
                read: OwnedRead::Native {
                    read: Box::new(self._operation.into_owned_hold()),
                    refusal,
                },
            }));
        }
        let RestoreExecution {
            report,
            files,
            _operation,
            _binding,
        } = self;
        drop(_binding);
        match _operation.settle_direct() {
            Err(read) => Err(Box::new(OwnedRestoreFinishHold {
                report,
                files,
                _binding: None,
                read: OwnedRead::Lower(read),
            })),
            Ok(read) => Ok((
                read,
                SettledRestoreReport {
                    report,
                    files: settled_files(files),
                },
            )),
        }
    }

    pub fn into_owned_hold(self) -> OwnedRestoreFinishHold {
        OwnedRestoreFinishHold {
            report: self.report,
            files: self.files,
            _binding: self._binding,
            read: OwnedRead::Native {
                read: Box::new(self._operation.into_owned_hold()),
                refusal: NativeRefusal {
                    file: None,
                    failure: SettlementFailure::refused("restore_unsettled"),
                    readback: None,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn closed_native_custody_and_settled_data_are_send_static() {
        fn require<T: Send + 'static>() {}
        require::<super::OwnedRestoreFinishHold>();
        require::<super::SettledRestoreReport>();
    }
}
