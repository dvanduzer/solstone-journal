// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Forward observations from original live owners. Decoded data cannot acquire,
//! recover, settle or retire any owner.

use crate::archive_origin::{ArchiveOriginEvidence, OriginError};
use crate::media::MediaIdentity;
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt, io};

const MAX_BINDING_BYTES: usize = 128 * 1024;
const LOCK_NAME: &str = "backup-activity.lock";
const RESERVATION_PREFIX: &str = ".tmp-offload-";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLockObservation {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) size: u64,
    pub(crate) modified_seconds: i64,
    pub(crate) modified_nanoseconds: i64,
    pub(crate) changed_seconds: i64,
    pub(crate) changed_nanoseconds: i64,
    pub(crate) mode: u32,
    pub(crate) links: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingWire {
    format_version: u8,
    #[serde(deserialize_with = "crate::archive_origin::deserialize_evidence")]
    origin: ArchiveOriginEvidence,
    reservation_name: String,
    reservation_identity: MediaIdentity,
    activity_lock_name: String,
    activity_lock: RecoveryLockObservation,
    repository_id: String,
}

/// Immutable observations only. Native record decoding does not mint an owner.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ArchiveRecoveryBinding(BindingWire);

impl fmt::Debug for ArchiveRecoveryBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveRecoveryBinding { observations: redacted }")
    }
}

impl<'de> Deserialize<'de> for ArchiveRecoveryBinding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = BindingWire::deserialize(deserializer)?;
        validate(&wire).map_err(serde::de::Error::custom)?;
        Ok(Self(wire))
    }
}

pub enum RecoveryBindingDecodeError {
    TooLarge,
    Decode(serde_json::Error),
}
impl fmt::Display for RecoveryBindingDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "archive recovery binding exceeds its bound",
            Self::Decode(_) => "invalid archive recovery binding",
        })
    }
}
impl fmt::Debug for RecoveryBindingDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error for RecoveryBindingDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::TooLarge => None,
        }
    }
}

pub enum RecoveryBindingObservationError {
    Unsupported,
    Origin(OriginError),
    Activity(crate::activity::ActivityAdmissionError),
    Native(io::Error),
}
impl fmt::Display for RecoveryBindingObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "archive recovery observation is unsupported",
            Self::Origin(_) => "archive recovery origin changed or is unavailable",
            Self::Activity(_) => "archive recovery activity validation failed",
            Self::Native(_) => "archive recovery native binding validation failed",
        })
    }
}
impl fmt::Debug for RecoveryBindingObservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error for RecoveryBindingObservationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Origin(error) => Some(error),
            Self::Activity(error) => Some(error),
            Self::Native(error) => Some(error),
            Self::Unsupported => None,
        }
    }
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid original recovery observations",
    )
}
fn validate(wire: &BindingWire) -> io::Result<()> {
    let device = wire.origin.root_device();
    let lock = &wire.activity_lock;
    if wire.format_version != 1
        || device == 0
        || !matches!(wire.reservation_identity, MediaIdentity::Unix { device: d, inode } if d == device && inode != 0)
        || !wire
            .reservation_name
            .strip_prefix(RESERVATION_PREFIX)
            .is_some_and(|s| lower_hex(s, 32))
        || wire.activity_lock_name != LOCK_NAME
        || lock.device != device
        || lock.inode == 0
        || lock.links != 1
        || lock.mode & 0o170000 != 0o100000
        || !matches!(lock.mode & 0o7777, 0o600 | 0o644)
        || !(0..1_000_000_000).contains(&lock.modified_nanoseconds)
        || !(0..1_000_000_000).contains(&lock.changed_nanoseconds)
        || !lower_hex(&wire.repository_id, 64)
    {
        return Err(invalid());
    }
    Ok(())
}

impl ArchiveRecoveryBinding {
    pub fn from_json(bytes: &[u8]) -> Result<Self, RecoveryBindingDecodeError> {
        if bytes.len() > MAX_BINDING_BYTES {
            return Err(RecoveryBindingDecodeError::TooLarge);
        }
        serde_json::from_slice(bytes).map_err(RecoveryBindingDecodeError::Decode)
    }
    pub fn origin(&self) -> &ArchiveOriginEvidence {
        &self.0.origin
    }
    pub fn repository_id(&self) -> &str {
        &self.0.repository_id
    }
    pub fn reservation_name(&self) -> &str {
        &self.0.reservation_name
    }
    pub fn reservation_identity(&self) -> MediaIdentity {
        self.0.reservation_identity
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn observed(
        origin: ArchiveOriginEvidence,
        name: String,
        identity: MediaIdentity,
        lock: RecoveryLockObservation,
        repository_id: &str,
    ) -> io::Result<Self> {
        let wire = BindingWire {
            format_version: 1,
            origin,
            reservation_name: name,
            reservation_identity: identity,
            activity_lock_name: LOCK_NAME.into(),
            activity_lock: lock,
            repository_id: repository_id.into(),
        };
        validate(&wire)?;
        Ok(Self(wire))
    }
}

#[cfg(test)]
mod tests;

mod recovered;
pub use recovered::{
    ArchiveRecoveryAdmissionFailure, RecoveredArchiveReservation, RecoveredArchiveRetirement,
    RecoveryAdmissionError, RecoveryPopulationFailure,
};
