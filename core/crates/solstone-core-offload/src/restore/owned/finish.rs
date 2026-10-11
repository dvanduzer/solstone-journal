// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Consuming settlement preserves receipts and keeps every uncertain owner closed.

use super::*;
use crate::restore_intent::AttemptEffects;
use crate::restore_intent_store::transaction::{SettledTransaction, SettlementFailure};
use solstone_core_backup::archive_readback::{
    HeldDirectRead, OwnedDirectReadHold, OwnedHeldDirectRead, ReadbackError, SettledDirectRead,
};
use solstone_core_journal_io::media::RootActivityCarrier;

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
        read: OwnedDirectReadHold,
        refusal: NativeRefusal,
    },
    Lower(OwnedHeldDirectRead),
}

#[must_use = "closed custody must survive worker exit and rejected handoff"]
pub struct OwnedRestoreFinishHold {
    report: RestoreResult,
    files: Vec<FileRecord>,
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
        if let Err(refusal) = self.qualify_native() {
            return Err(Box::new(HeldRestoreFinish {
                owner: HeldOwner::Native {
                    outcome: Box::new(self),
                    refusal,
                },
            }));
        }
        // Inspect every file before any further cleanup. Each failure retains all files,
        // including successful cleanup facts from earlier members of this same attempt.
        for (index, file) in self.files.iter_mut().enumerate() {
            if let FileOwner::Transaction(transaction) = &mut file.owner
                && let Err(failure) = transaction
                    .revalidate_settlement(self._operation.carrier())
                    .and_then(|()| transaction.cleanup_settlement())
                    .and_then(|()| transaction.revalidate_settlement(self._operation.carrier()))
            {
                let refusal = NativeRefusal {
                    file: Some(index),
                    failure,
                    readback: None,
                };
                return Err(Box::new(HeldRestoreFinish {
                    owner: HeldOwner::Native {
                        outcome: Box::new(self),
                        refusal,
                    },
                }));
            }
        }
        let Self {
            report,
            _operation: operation,
            files,
        } = self;
        match operation.settle_direct() {
            Err(read) => Err(Box::new(HeldRestoreFinish {
                owner: HeldOwner::Lower {
                    report,
                    files,
                    read,
                },
            })),
            Ok(read) => {
                let files = files
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
                            FileOwner::Transaction(transaction) => {
                                Some(Box::new(transaction.into_settled()))
                            }
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
                    .collect();
                Ok((read, SettledRestoreReport { report, files }))
            }
        }
    }

    fn qualify_native(&self) -> Result<(), NativeRefusal> {
        for (index, file) in self.files.iter().enumerate() {
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
                        self._operation.carrier(),
                        &file.witness,
                        file.disposition,
                        AttemptEffects {
                            files: file.files,
                            bytes: file.bytes,
                        },
                    )
                    .and_then(|()| transaction.revalidate_settlement(self._operation.carrier()))
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
        self._operation
            .revalidate_admission()
            .map_err(|readback| NativeRefusal {
                file: None,
                failure: SettlementFailure::refused("settlement_read_refused"),
                readback: Some(readback),
            })
    }
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
                } = *outcome;
                match operation.into_owned_hold() {
                    Ok(read) => Ok(OwnedRestoreFinishHold {
                        report,
                        files,
                        read: OwnedRead::Native { read, refusal },
                    }),
                    Err(operation) => Err(Box::new(Self {
                        owner: HeldOwner::Native {
                            outcome: Box::new(RestoreRunOutcome {
                                report,
                                files,
                                _operation: *operation,
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

#[cfg(test)]
mod tests {
    #[test]
    fn closed_native_custody_and_settled_data_are_send_static() {
        fn require<T: Send + 'static>() {}
        require::<super::OwnedRestoreFinishHold>();
        require::<super::SettledRestoreReport>();
    }
}
