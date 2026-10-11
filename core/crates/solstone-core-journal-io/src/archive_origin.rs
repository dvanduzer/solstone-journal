// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Current Linux origin observations retained with their original journal root.
//! Serialized observations are data. They grant no recovery or cleanup authority.
//! Direct PCIe NVMe presentation does not establish unshared or physical storage.

use crate::journal_root::{JournalRoot, JournalRootError, is_canonical_identity_observation};
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "linux", test))]
use sha2::{Digest, Sha256};
use std::{error::Error, fmt, io, sync::Arc};

const MAX_EVIDENCE_BYTES: usize = 65_536;
const MAX_CHAIN: usize = 64;
const MAX_NATIVE_PATH: usize = 4096;
#[cfg(any(target_os = "linux", test))]
const MACHINE_DOMAIN: &[u8] = b"solstone-local-archive-origin-v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginInput {
    Root,
    CanonicalChain,
    Boot,
    Machine,
    Mount,
    Topology,
    Evidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginRelation {
    CanonicalChain,
    Boot,
    Machine,
    Mount,
    Topology,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginUnsupported {
    Platform,
    Filesystem,
    Topology,
}

/// The original cause remains accessible without formatting origin identifiers.
pub enum OriginError {
    Root(JournalRootError),
    Io {
        input: OriginInput,
        source: io::Error,
    },
    Malformed(OriginInput),
    Changed(OriginRelation),
    Unsupported(OriginUnsupported),
}

impl fmt::Display for OriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(_) => f.write_str("archive origin root validation failed"),
            Self::Io { input, source } => {
                write!(
                    f,
                    "archive origin {input:?} read failed (errno {:?})",
                    source.raw_os_error()
                )
            }
            Self::Malformed(input) => write!(f, "malformed archive origin {input:?} observation"),
            Self::Changed(relation) => write!(f, "archive origin {relation:?} changed"),
            Self::Unsupported(reason) => write!(f, "unsupported archive origin {reason:?}"),
        }
    }
}

impl fmt::Debug for OriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for OriginError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub enum OriginEvidenceError {
    TooLarge,
    Decode(serde_json::Error),
    Invalid,
}

impl fmt::Display for OriginEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "archive origin evidence exceeds its bound",
            Self::Decode(_) => "archive origin evidence cannot be decoded",
            Self::Invalid => "invalid archive origin evidence",
        })
    }
}

impl fmt::Debug for OriginEvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for OriginEvidenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::Decode(error) = self {
            Some(error)
        } else {
            None
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    major: u64,
    minor: u64,
}

// Evidence v1 stores the Linux dev_t observation, even when decoded on another OS.
fn linux_device(raw: u64) -> Device {
    Device {
        major: ((raw >> 8) & 0xfff) | ((raw >> 32) & 0xffff_f000),
        minor: (raw & 0xff) | ((raw >> 12) & 0xffff_ff00),
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChainEntry {
    name: Vec<u8>,
    identity: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceWire {
    format_version: u8,
    boot_id: String,
    machine_app_id_sha256: String,
    root_identity: String,
    canonical_chain: Vec<ChainEntry>,
    filesystem: String,
    storage_topology: String,
    filesystem_device: Device,
    namespace_device: Device,
    partition: Option<u32>,
}

/// Strict immutable data, distinct from a currently observed root owner.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ArchiveOriginEvidence(EvidenceWire);

impl fmt::Debug for ArchiveOriginEvidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveOriginEvidence { observations: redacted }")
    }
}

impl ArchiveOriginEvidence {
    pub fn boot_id(&self) -> &str {
        &self.0.boot_id
    }

    pub(crate) fn root_device(&self) -> u64 {
        unix_observation(&self.0.root_identity).expect("validated origin identity")
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, OriginEvidenceError> {
        if bytes.len() > MAX_EVIDENCE_BYTES {
            return Err(OriginEvidenceError::TooLarge);
        }
        let wire: EvidenceWire =
            serde_json::from_slice(bytes).map_err(OriginEvidenceError::Decode)?;
        validate_evidence(&wire).map_err(|_| OriginEvidenceError::Invalid)?;
        Ok(Self(wire))
    }
}

pub(crate) fn deserialize_evidence<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ArchiveOriginEvidence, D::Error> {
    let wire = EvidenceWire::deserialize(deserializer)?;
    validate_evidence(&wire).map_err(serde::de::Error::custom)?;
    Ok(ArchiveOriginEvidence(wire))
}

fn lower_hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

fn one_line(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

fn parse_boot(bytes: &[u8]) -> Result<String, OriginError> {
    let bytes = one_line(bytes);
    if bytes.len() != 36
        || bytes.iter().enumerate().any(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                *b != b'-'
            } else {
                !lower_hex(&[*b])
            }
        })
        || bytes.iter().filter(|b| **b != b'-').all(|b| *b == b'0')
    {
        return Err(OriginError::Malformed(OriginInput::Boot));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| OriginError::Malformed(OriginInput::Boot))
}

#[cfg(any(target_os = "linux", test))]
fn machine_digest(bytes: &[u8]) -> Result<String, OriginError> {
    let bytes = one_line(bytes);
    if bytes.len() != 32 || !lower_hex(bytes) || bytes.iter().all(|b| *b == b'0') {
        return Err(OriginError::Malformed(OriginInput::Machine));
    }
    let mut decoded = [0u8; 16];
    fn digit(byte: u8) -> u8 {
        if byte.is_ascii_digit() {
            byte - b'0'
        } else {
            byte - b'a' + 10
        }
    }
    for (out, pair) in decoded.iter_mut().zip(bytes.chunks_exact(2)) {
        *out = (digit(pair[0]) << 4) | digit(pair[1]);
    }
    let mut hash = Sha256::new();
    hash.update(MACHINE_DOMAIN);
    hash.update(decoded);
    Ok(format!("{:x}", hash.finalize()))
}

fn unix_observation(value: &str) -> Option<u64> {
    if !value.starts_with("native-root-v1:unix:") || !is_canonical_identity_observation(value) {
        return None;
    }
    u64::from_str_radix(
        value
            .strip_prefix("native-root-v1:unix:")?
            .split(':')
            .next()?,
        16,
    )
    .ok()
}

fn validate_evidence(wire: &EvidenceWire) -> Result<(), OriginError> {
    let invalid = || OriginError::Malformed(OriginInput::Evidence);
    let chain = &wire.canonical_chain;
    let root_device = unix_observation(&wire.root_identity).ok_or_else(invalid)?;
    if wire.format_version != 1
        || wire.boot_id.len() != 36
        || parse_boot(wire.boot_id.as_bytes()).is_err()
        || wire.machine_app_id_sha256.len() != 64
        || !lower_hex(wire.machine_app_id_sha256.as_bytes())
        || wire.filesystem != "ext4"
        || wire.storage_topology != "direct-pcie-nvme"
        || wire.filesystem_device != linux_device(root_device)
        || wire.namespace_device.major > u32::MAX.into()
        || wire.namespace_device.minor > u32::MAX.into()
        || wire.partition == Some(0)
        || chain.is_empty()
        || chain.len() > MAX_CHAIN
        || chain
            .last()
            .is_none_or(|v| v.identity != wire.root_identity)
        || !chain[0].name.is_empty()
    {
        return Err(invalid());
    }
    let mut path_bytes = 1usize;
    for (index, entry) in chain.iter().enumerate() {
        if unix_observation(&entry.identity).is_none()
            || (index != 0
                && (entry.name.is_empty()
                    || entry.name.len() > 255
                    || entry.name == b"."
                    || entry.name == b".."
                    || entry.name.contains(&b'/')
                    || entry.name.contains(&0)))
        {
            return Err(invalid());
        }
        path_bytes = path_bytes
            .checked_add(entry.name.len() + 1)
            .ok_or_else(invalid)?;
    }
    if path_bytes > MAX_NATIVE_PATH {
        return Err(invalid());
    }
    if wire.partition.is_none() && wire.filesystem_device != wire.namespace_device {
        return Err(invalid());
    }
    Ok(())
}

/// Holds the supplied root while its current origin remains revalidatable.
pub struct LocalArchiveOrigin {
    #[cfg(target_os = "linux")]
    root: Arc<JournalRoot>,
    evidence: ArchiveOriginEvidence,
    #[cfg(target_os = "linux")]
    current: linux::Current,
}

impl fmt::Debug for LocalArchiveOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalArchiveOrigin { retained_root: redacted }")
    }
}

pub struct OriginObservationFailure {
    root: Arc<JournalRoot>,
    error: OriginError,
}

impl fmt::Debug for OriginObservationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OriginObservationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl OriginObservationFailure {
    pub fn error(&self) -> &OriginError {
        &self.error
    }
    pub fn root(&self) -> &Arc<JournalRoot> {
        &self.root
    }
}

impl LocalArchiveOrigin {
    #[cfg(target_os = "linux")]
    pub(crate) fn root(&self) -> &Arc<JournalRoot> {
        &self.root
    }
    pub fn evidence(&self) -> &ArchiveOriginEvidence {
        &self.evidence
    }
    pub fn revalidate(&self) -> Result<(), OriginError> {
        #[cfg(target_os = "linux")]
        {
            let (evidence, current) = linux::observe(&self.root)?;
            linux::compare(&self.evidence, &self.current, &evidence, &current)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(OriginError::Unsupported(OriginUnsupported::Platform))
        }
    }
}

pub fn observe_archive_origin(
    root: Arc<JournalRoot>,
) -> Result<LocalArchiveOrigin, OriginObservationFailure> {
    #[cfg(target_os = "linux")]
    {
        let observations = (|| {
            let (evidence, current) = linux::observe(&root)?;
            let (second, second_current) = linux::observe(&root)?;
            linux::compare(&evidence, &current, &second, &second_current)?;
            Ok((evidence, current))
        })();
        match observations {
            Ok((evidence, current)) => Ok(LocalArchiveOrigin {
                root,
                evidence,
                current,
            }),
            Err(error) => Err(OriginObservationFailure { root, error }),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(OriginObservationFailure {
            root,
            error: OriginError::Unsupported(OriginUnsupported::Platform),
        })
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use nix::{
        dir::Dir,
        fcntl::{AtFlags, OFlag, open, openat, readlinkat},
        sys::{
            stat::{Mode, SFlag, fstat, fstatat},
            statfs::{EXT2_SUPER_MAGIC, PROC_SUPER_MAGIC, SYSFS_MAGIC, fstatfs},
        },
    };
    use std::{
        ffi::OsStr,
        fs::File,
        io::Read,
        os::{
            fd::{AsFd, AsRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
        path::{Component, Path, PathBuf},
    };

    const DIR_FLAGS: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_DIRECTORY)
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC);
    const FILE_FLAGS: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC)
        .union(OFlag::O_NONBLOCK);

    #[derive(Clone, Eq, PartialEq)]
    pub(super) struct Current {
        mount_id: u64,
        mount_line: String,
        device_path: PathBuf,
        namespace_identity: String,
        controller_identity: String,
    }

    fn syscall(input: OriginInput, error: nix::errno::Errno) -> OriginError {
        OriginError::Io {
            input,
            source: io::Error::from_raw_os_error(error as i32),
        }
    }

    fn malformed(input: OriginInput) -> OriginError {
        OriginError::Malformed(input)
    }
    fn topology() -> OriginError {
        OriginError::Unsupported(OriginUnsupported::Topology)
    }
    fn identity(stat: &nix::sys::stat::FileStat) -> String {
        crate::ObjectIdentity::from_device_inode(stat.st_dev, stat.st_ino).observation_id()
    }

    fn directory(
        parent: &impl AsFd,
        name: &OsStr,
        input: OriginInput,
    ) -> Result<OwnedFd, OriginError> {
        openat(parent, name, DIR_FLAGS, Mode::empty()).map_err(|e| syscall(input, e))
    }

    fn absolute_directory(path: &Path, input: OriginInput) -> Result<OwnedFd, OriginError> {
        let mut current = open("/", DIR_FLAGS, Mode::empty()).map_err(|e| syscall(input, e))?;
        if path.as_os_str().as_bytes().len() > MAX_NATIVE_PATH {
            return Err(malformed(input));
        }
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    current = directory(&current, name, input)?;
                }
                _ => return Err(malformed(input)),
            }
        }
        Ok(current)
    }

    fn bounded_regular(
        parent: &impl AsFd,
        name: &OsStr,
        cap: usize,
        input: OriginInput,
        root_owned: bool,
        read_only: bool,
    ) -> Result<Vec<u8>, OriginError> {
        let fd = openat(parent, name, FILE_FLAGS, Mode::empty()).map_err(|e| syscall(input, e))?;
        let stat = fstat(&fd).map_err(|e| syscall(input, e))?;
        if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG
            || (root_owned && stat.st_uid != 0)
            || stat.st_mode & 0o022 != 0
            || (read_only && stat.st_mode & 0o222 != 0)
        {
            return Err(malformed(input));
        }
        bounded_bytes(File::from(fd), cap, input)
    }

    fn bounded_bytes(
        reader: impl Read,
        cap: usize,
        input: OriginInput,
    ) -> Result<Vec<u8>, OriginError> {
        let mut bytes = Vec::new();
        reader
            .take((cap + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| OriginError::Io { input, source })?;
        if bytes.len() > cap {
            return Err(malformed(input));
        }
        Ok(bytes)
    }

    fn small_attribute(parent: &impl AsFd, name: &str) -> Result<Vec<u8>, OriginError> {
        bounded_regular(
            parent,
            OsStr::new(name),
            64,
            OriginInput::Topology,
            true,
            true,
        )
    }

    fn chain(root: &JournalRoot) -> Result<Vec<ChainEntry>, OriginError> {
        let path = root.canonical_path();
        if path.as_os_str().as_bytes().len() > MAX_NATIVE_PATH {
            return Err(malformed(OriginInput::CanonicalChain));
        }
        let mut current = open("/", DIR_FLAGS, Mode::empty())
            .map_err(|e| syscall(OriginInput::CanonicalChain, e))?;
        let stat = fstat(&current).map_err(|e| syscall(OriginInput::CanonicalChain, e))?;
        let mut result = vec![ChainEntry {
            name: Vec::new(),
            identity: identity(&stat),
        }];
        for part in path.components() {
            let Component::Normal(name) = part else {
                if part == Component::RootDir {
                    continue;
                }
                return Err(malformed(OriginInput::CanonicalChain));
            };
            if result.len() == MAX_CHAIN || name.as_bytes().len() > 255 {
                return Err(malformed(OriginInput::CanonicalChain));
            }
            let before = fstatat(&current, name, AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_err(|e| syscall(OriginInput::CanonicalChain, e))?;
            let child = directory(&current, name, OriginInput::CanonicalChain)?;
            let after = fstat(&child).map_err(|e| syscall(OriginInput::CanonicalChain, e))?;
            let named = fstatat(&current, name, AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_err(|e| syscall(OriginInput::CanonicalChain, e))?;
            if SFlag::from_bits_truncate(before.st_mode) & SFlag::S_IFMT != SFlag::S_IFDIR
                || identity(&before) != identity(&after)
                || identity(&named) != identity(&after)
            {
                return Err(OriginError::Changed(OriginRelation::CanonicalChain));
            }
            result.push(ChainEntry {
                name: name.as_bytes().to_vec(),
                identity: identity(&after),
            });
            current = child;
        }
        if result
            .last()
            .is_none_or(|e| e.identity != root.identity().observation_id())
        {
            return Err(OriginError::Changed(OriginRelation::CanonicalChain));
        }
        Ok(result)
    }

    fn decimal(value: &str) -> Option<u64> {
        if value.is_empty()
            || !value.bytes().all(|b| b.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return None;
        }
        value.parse().ok()
    }

    fn device(bytes: &[u8]) -> Result<Device, OriginError> {
        let value =
            std::str::from_utf8(one_line(bytes)).map_err(|_| malformed(OriginInput::Topology))?;
        let (major, minor) = value
            .split_once(':')
            .ok_or_else(|| malformed(OriginInput::Topology))?;
        let parsed = Device {
            major: decimal(major).ok_or_else(|| malformed(OriginInput::Topology))?,
            minor: decimal(minor).ok_or_else(|| malformed(OriginInput::Topology))?,
        };
        if parsed.major > u32::MAX.into() || parsed.minor > u32::MAX.into() {
            return Err(malformed(OriginInput::Topology));
        }
        Ok(parsed)
    }

    fn mount_id(bytes: &[u8]) -> Result<u64, OriginError> {
        let value = std::str::from_utf8(bytes).map_err(|_| malformed(OriginInput::Mount))?;
        let mut ids = value
            .lines()
            .filter_map(|line| line.strip_prefix("mnt_id:"));
        let id = ids
            .next()
            .and_then(|v| decimal(v.trim()))
            .filter(|v| *v > 0)
            .ok_or_else(|| malformed(OriginInput::Mount))?;
        if ids.next().is_some() {
            return Err(malformed(OriginInput::Mount));
        }
        Ok(id)
    }

    fn selected_mount(
        bytes: &[u8],
        id: u64,
        expected_device: &Device,
    ) -> Result<String, OriginError> {
        if bytes.is_empty() || bytes.len() > 1_048_576 || !bytes.ends_with(b"\n") {
            return Err(malformed(OriginInput::Mount));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| malformed(OriginInput::Mount))?;
        let mut selected = None;
        for line in text.lines() {
            let parts = line.split_ascii_whitespace().collect::<Vec<_>>();
            let separator = parts
                .iter()
                .position(|v| *v == "-")
                .ok_or_else(|| malformed(OriginInput::Mount))?;
            if separator < 6
                || parts.len() != separator + 4
                || decimal(parts[0]).is_none()
                || decimal(parts[1]).is_none()
            {
                return Err(malformed(OriginInput::Mount));
            }
            let observed_device =
                device(parts[2].as_bytes()).map_err(|_| malformed(OriginInput::Mount))?;
            if decimal(parts[0]) == Some(id) {
                if selected.is_some() || observed_device != *expected_device {
                    return Err(malformed(OriginInput::Mount));
                }
                if parts[separator + 1] != "ext4" {
                    return Err(OriginError::Unsupported(OriginUnsupported::Filesystem));
                }
                selected = Some(line.to_owned());
            }
        }
        selected.ok_or_else(|| malformed(OriginInput::Mount))
    }

    fn normalized_sys_link(parent: &Path, link: &OsStr) -> Result<PathBuf, OriginError> {
        if link.as_bytes().len() > MAX_NATIVE_PATH || link.as_bytes().contains(&0) {
            return Err(malformed(OriginInput::Topology));
        }
        let mut path = if Path::new(link).is_absolute() {
            PathBuf::new()
        } else {
            parent.to_path_buf()
        };
        for part in Path::new(link).components() {
            match part {
                Component::RootDir => path.push("/"),
                Component::Normal(name) => path.push(name),
                Component::ParentDir => {
                    if !path.pop() {
                        return Err(topology());
                    }
                }
                _ => return Err(topology()),
            }
        }
        if !path.starts_with("/sys") || path.as_os_str().as_bytes().len() > MAX_NATIVE_PATH {
            return Err(topology());
        }
        Ok(path)
    }

    struct TopologyPath {
        path: PathBuf,
        namespace: PathBuf,
        controller: PathBuf,
        pci: PathBuf,
        partition: Option<u32>,
    }

    fn pci_address(value: &str) -> bool {
        let bytes = value.as_bytes();
        bytes.len() == 12
            && bytes[4] == b':'
            && bytes[7] == b':'
            && bytes[10] == b'.'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| [4, 7, 10].contains(&i) || lower_hex(&[*b]))
    }

    fn topology_path(path: PathBuf) -> Result<TopologyPath, OriginError> {
        let components = path
            .strip_prefix("/sys/devices")
            .map_err(|_| topology())?
            .components()
            .map(|part| part.as_os_str().to_str().ok_or_else(topology))
            .collect::<Result<Vec<_>, _>>()?;
        let pivot = components
            .iter()
            .position(|c| *c == "nvme")
            .ok_or_else(topology)?;
        if pivot < 2
            || !components[0].starts_with("pci")
            || components[0].len() != 10
            || !components[1..pivot].iter().all(|v| pci_address(v))
            || components.len() < pivot + 3
            || components.len() > pivot + 4
        {
            return Err(topology());
        }
        let bus = &components[0].as_bytes()[3..];
        if bus[4] != b':'
            || !bus
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || lower_hex(&[*b]))
        {
            return Err(topology());
        }
        let controller = components[pivot + 1];
        if controller.strip_prefix("nvme").and_then(decimal).is_none() {
            return Err(topology());
        }
        let namespace = components[pivot + 2];
        let ns_id = namespace
            .strip_prefix(controller)
            .and_then(|v| v.strip_prefix('n'))
            .and_then(decimal);
        if ns_id.is_none_or(|v| v == 0) {
            return Err(topology());
        }
        let partition = if components.len() == pivot + 4 {
            let number = components[pivot + 3]
                .strip_prefix(namespace)
                .and_then(|v| v.strip_prefix('p'))
                .and_then(decimal)
                .filter(|v| *v > 0)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(topology)?;
            Some(number)
        } else {
            None
        };
        let namespace_path = if partition.is_some() {
            path.parent().ok_or_else(topology)?.to_path_buf()
        } else {
            path.clone()
        };
        let controller_path = namespace_path.parent().ok_or_else(topology)?.to_path_buf();
        let pci = controller_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(topology)?
            .to_path_buf();
        Ok(TopologyPath {
            path,
            namespace: namespace_path,
            controller: controller_path,
            pci,
            partition,
        })
    }

    fn empty_directory(parent: &impl AsFd, name: &str) -> Result<(), OriginError> {
        let fd = directory(parent, OsStr::new(name), OriginInput::Topology)?;
        let mut dir = Dir::from_fd(fd).map_err(|e| syscall(OriginInput::Topology, e))?;
        for entry in dir.iter() {
            let entry = entry.map_err(|e| syscall(OriginInput::Topology, e))?;
            if entry.file_name().to_bytes() != b"." && entry.file_name().to_bytes() != b".." {
                return Err(topology());
            }
        }
        Ok(())
    }

    fn subsystem(parent: &impl AsFd, path: &Path, expected: &Path) -> Result<(), OriginError> {
        let link =
            readlinkat(parent, "subsystem").map_err(|e| syscall(OriginInput::Topology, e))?;
        if normalized_sys_link(path, &link)? != expected {
            return Err(topology());
        }
        Ok(())
    }

    pub(super) fn observe(
        root: &JournalRoot,
    ) -> Result<(ArchiveOriginEvidence, Current), OriginError> {
        root.revalidate().map_err(OriginError::Root)?;
        root.revalidate_canonical_binding()
            .map_err(OriginError::Root)?;
        let canonical_chain = chain(root)?;
        let root_stat = fstat(root).map_err(|e| syscall(OriginInput::Root, e))?;
        let root_device = linux_device(root_stat.st_dev);
        if fstatfs(root)
            .map_err(|e| syscall(OriginInput::Root, e))?
            .filesystem_type()
            != EXT2_SUPER_MAGIC
        {
            return Err(OriginError::Unsupported(OriginUnsupported::Filesystem));
        }
        let proc = absolute_directory(Path::new("/proc"), OriginInput::Boot)?;
        if fstatfs(&proc)
            .map_err(|e| syscall(OriginInput::Boot, e))?
            .filesystem_type()
            != PROC_SUPER_MAGIC
        {
            return Err(malformed(OriginInput::Boot));
        }
        let proc_sys = directory(&proc, OsStr::new("sys"), OriginInput::Boot)?;
        let kernel = directory(&proc_sys, OsStr::new("kernel"), OriginInput::Boot)?;
        let boot_parent = directory(&kernel, OsStr::new("random"), OriginInput::Boot)?;
        if fstatfs(&boot_parent)
            .map_err(|e| syscall(OriginInput::Boot, e))?
            .filesystem_type()
            != PROC_SUPER_MAGIC
        {
            return Err(malformed(OriginInput::Boot));
        }
        let boot_id = parse_boot(&bounded_regular(
            &boot_parent,
            OsStr::new("boot_id"),
            37,
            OriginInput::Boot,
            true,
            true,
        )?)?;
        let etc = absolute_directory(Path::new("/etc"), OriginInput::Machine)?;
        let machine_app_id_sha256 = machine_digest(&bounded_regular(
            &etc,
            OsStr::new("machine-id"),
            33,
            OriginInput::Machine,
            true,
            false,
        )?)?;
        let pid = std::process::id().to_string();
        let process = directory(&proc, OsStr::new(&pid), OriginInput::Mount)?;
        let fdinfo_parent = directory(&process, OsStr::new("fdinfo"), OriginInput::Mount)?;
        let fdinfo = bounded_regular(
            &fdinfo_parent,
            OsStr::new(&root.as_fd().as_raw_fd().to_string()),
            4096,
            OriginInput::Mount,
            false,
            true,
        )?;
        let id = mount_id(&fdinfo)?;
        let mount_line = selected_mount(
            &bounded_regular(
                &process,
                OsStr::new("mountinfo"),
                1_048_576,
                OriginInput::Mount,
                false,
                true,
            )?,
            id,
            &root_device,
        )?;
        let sys = absolute_directory(Path::new("/sys"), OriginInput::Topology)?;
        if fstatfs(&sys)
            .map_err(|e| syscall(OriginInput::Topology, e))?
            .filesystem_type()
            != SYSFS_MAGIC
        {
            return Err(malformed(OriginInput::Topology));
        }
        let dev = directory(&sys, OsStr::new("dev"), OriginInput::Topology)?;
        let block = directory(&dev, OsStr::new("block"), OriginInput::Topology)?;
        let link = readlinkat(
            &block,
            OsStr::new(&format!("{}:{}", root_device.major, root_device.minor)),
        )
        .map_err(|e| syscall(OriginInput::Topology, e))?;
        let paths = topology_path(normalized_sys_link(Path::new("/sys/dev/block"), &link)?)?;
        let node = absolute_directory(&paths.path, OriginInput::Topology)?;
        let namespace = absolute_directory(&paths.namespace, OriginInput::Topology)?;
        let controller = absolute_directory(&paths.controller, OriginInput::Topology)?;
        let pci = absolute_directory(&paths.pci, OriginInput::Topology)?;
        if device(&small_attribute(&node, "dev")?)? != root_device {
            return Err(topology());
        }
        let namespace_device = device(&small_attribute(&namespace, "dev")?)?;
        if paths.partition.is_some() {
            let actual = small_attribute(&node, "partition")?;
            let actual = std::str::from_utf8(one_line(&actual))
                .ok()
                .and_then(decimal)
                .and_then(|v| u32::try_from(v).ok());
            if actual != paths.partition {
                return Err(topology());
            }
            empty_directory(&node, "holders")?;
        } else if namespace_device != root_device {
            return Err(topology());
        }
        if one_line(&small_attribute(&namespace, "removable")?) != b"0"
            || one_line(&small_attribute(&controller, "transport")?) != b"pcie"
        {
            return Err(topology());
        }
        subsystem(&controller, &paths.controller, Path::new("/sys/class/nvme"))?;
        subsystem(&pci, &paths.pci, Path::new("/sys/bus/pci"))?;
        empty_directory(&namespace, "holders")?;
        empty_directory(&namespace, "slaves")?;
        let current = Current {
            mount_id: id,
            mount_line,
            device_path: paths.path,
            namespace_identity: identity(
                &fstat(&namespace).map_err(|e| syscall(OriginInput::Topology, e))?,
            ),
            controller_identity: identity(
                &fstat(&controller).map_err(|e| syscall(OriginInput::Topology, e))?,
            ),
        };
        let evidence = ArchiveOriginEvidence(EvidenceWire {
            format_version: 1,
            boot_id,
            machine_app_id_sha256,
            root_identity: root.identity().observation_id(),
            canonical_chain,
            filesystem: "ext4".into(),
            storage_topology: "direct-pcie-nvme".into(),
            filesystem_device: root_device,
            namespace_device,
            partition: paths.partition,
        });
        validate_evidence(&evidence.0)?;
        let serialized =
            serde_json::to_vec(&evidence).map_err(|_| malformed(OriginInput::Evidence))?;
        if serialized.len() > MAX_EVIDENCE_BYTES {
            return Err(malformed(OriginInput::Evidence));
        }
        root.revalidate().map_err(OriginError::Root)?;
        root.revalidate_canonical_binding()
            .map_err(OriginError::Root)?;
        Ok((evidence, current))
    }

    pub(super) fn compare(
        old: &ArchiveOriginEvidence,
        old_current: &Current,
        now: &ArchiveOriginEvidence,
        now_current: &Current,
    ) -> Result<(), OriginError> {
        let old = &old.0;
        let now = &now.0;
        let relation = if old.canonical_chain != now.canonical_chain
            || old.root_identity != now.root_identity
        {
            Some(OriginRelation::CanonicalChain)
        } else if old.boot_id != now.boot_id {
            Some(OriginRelation::Boot)
        } else if old.machine_app_id_sha256 != now.machine_app_id_sha256 {
            Some(OriginRelation::Machine)
        } else if old_current.mount_id != now_current.mount_id
            || old_current.mount_line != now_current.mount_line
        {
            Some(OriginRelation::Mount)
        } else if old != now || old_current != now_current {
            Some(OriginRelation::Topology)
        } else {
            None
        };
        relation.map_or(Ok(()), |value| Err(OriginError::Changed(value)))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn mount_and_topology_parsers_refuse_ambiguous_or_unqualified_inputs() {
            let dev = Device {
                major: 259,
                minor: 6,
            };
            let line = b"42 1 259:6 / /var/tmp rw - ext4 /dev/nvme0n1p1 rw\n";
            assert!(selected_mount(line, 42, &dev).is_ok());
            assert!(
                selected_mount(&[line.as_slice(), line.as_slice()].concat(), 42, &dev).is_err()
            );
            assert!(selected_mount(&line[..line.len() - 1], 42, &dev).is_err());
            assert!(selected_mount(line, 43, &dev).is_err());
            assert!(
                selected_mount(
                    line,
                    42,
                    &Device {
                        major: 259,
                        minor: 7
                    }
                )
                .is_err()
            );
            assert_eq!(mount_id(b"pos:\t0\nmnt_id:\t42\n").unwrap(), 42);
            assert!(mount_id(b"mnt_id:\t42\nmnt_id:\t42\n").is_err());
            let good =
                "/sys/devices/pci0000:00/0000:00:01.3/0000:08:00.0/nvme/nvme0/nvme0n1/nvme0n1p1";
            assert_eq!(topology_path(good.into()).unwrap().partition, Some(1));
            for bad in [
                "/sys/devices/virtual/block/dm-0",
                "/sys/devices/pci0000:00/0000:08:00.0/nvme/nvme0/nvme0c1n1",
                "/sys/devices/pci0000:00/0000:08:00.0/nvme/nvme0/nvme1n1",
            ] {
                assert!(topology_path(bad.into()).is_err());
            }
            assert!(
                normalized_sys_link(Path::new("/sys/dev/block"), OsStr::new("../../../../etc"))
                    .is_err()
            );
        }

        #[test]
        fn valid_snapshot_changes_are_refused_by_the_production_comparison() {
            let evidence = crate::archive_origin::tests::evidence();
            let current = Current {
                mount_id: 42,
                mount_line: "42".into(),
                device_path: "/sys/devices/example".into(),
                namespace_identity: "ns".into(),
                controller_identity: "controller".into(),
            };
            assert!(compare(&evidence, &current, &evidence, &current).is_ok());
            for index in 0..5 {
                let mut changed = evidence.clone();
                let mut relation = current.clone();
                match index {
                    0 => changed.0.boot_id = "22222222-2222-2222-2222-222222222222".into(),
                    1 => changed.0.machine_app_id_sha256 = "b".repeat(64),
                    2 => {
                        changed.0.canonical_chain[0].identity =
                            "native-root-v1:unix:0000000000010306:0000000000000003".into()
                    }
                    3 => relation.mount_id = 43,
                    _ => relation.controller_identity = "another".into(),
                }
                assert!(validate_evidence(&changed.0).is_ok());
                assert!(matches!(
                    compare(&evidence, &current, &changed, &relation),
                    Err(OriginError::Changed(_))
                ));
            }
        }

        #[cfg(feature = "full-tests")]
        #[test]
        fn bounded_input_preserves_native_type_limit_and_errno_refusals() {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let temp = tempfile::tempdir().unwrap();
            std::fs::write(temp.path().join("file"), b"abcd").unwrap();
            let dir = absolute_directory(temp.path(), OriginInput::Evidence).unwrap();
            assert_eq!(
                bounded_regular(
                    &dir,
                    OsStr::new("file"),
                    4,
                    OriginInput::Evidence,
                    false,
                    false
                )
                .unwrap(),
                b"abcd"
            );
            assert!(matches!(
                bounded_regular(
                    &dir,
                    OsStr::new("file"),
                    3,
                    OriginInput::Evidence,
                    false,
                    false
                ),
                Err(OriginError::Malformed(_))
            ));
            symlink("file", temp.path().join("link")).unwrap();
            assert!(
                matches!(bounded_regular(&dir,OsStr::new("link"),4,OriginInput::Evidence,false,false),Err(OriginError::Io{source,..}) if source.raw_os_error()==Some(libc::ELOOP))
            );
            assert!(
                matches!(bounded_regular(&dir,OsStr::new("missing"),4,OriginInput::Evidence,false,false),Err(OriginError::Io{source,..}) if source.raw_os_error()==Some(libc::ENOENT))
            );
            std::fs::set_permissions(
                temp.path().join("file"),
                std::fs::Permissions::from_mode(0o666),
            )
            .unwrap();
            assert!(matches!(
                bounded_regular(
                    &dir,
                    OsStr::new("file"),
                    4,
                    OriginInput::Evidence,
                    false,
                    false
                ),
                Err(OriginError::Malformed(_))
            ));
            std::fs::create_dir(temp.path().join("not-file")).unwrap();
            assert!(matches!(
                bounded_regular(
                    &dir,
                    OsStr::new("not-file"),
                    4,
                    OriginInput::Evidence,
                    false,
                    false
                ),
                Err(OriginError::Malformed(_))
            ));
            struct FailedRead;
            impl Read for FailedRead {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    Err(io::Error::from_raw_os_error(libc::EIO))
                }
            }
            assert!(
                matches!(bounded_bytes(FailedRead, 4, OriginInput::Evidence), Err(OriginError::Io{source,..}) if source.raw_os_error()==Some(libc::EIO))
            );
            std::fs::create_dir(temp.path().join("holders")).unwrap();
            empty_directory(&dir, "holders").unwrap();
            std::fs::write(temp.path().join("holders/member"), b"").unwrap();
            assert!(matches!(
                empty_directory(&dir, "holders"),
                Err(OriginError::Unsupported(OriginUnsupported::Topology))
            ));
            assert!(
                matches!(empty_directory(&dir, "slaves"), Err(OriginError::Io{source,..}) if source.raw_os_error()==Some(libc::ENOENT))
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn evidence() -> ArchiveOriginEvidence {
        let root = "native-root-v1:unix:0000000000010306:0000000000000002".to_owned();
        ArchiveOriginEvidence(EvidenceWire {
            format_version: 1,
            boot_id: "11111111-1111-1111-1111-111111111111".into(),
            machine_app_id_sha256: "a".repeat(64),
            root_identity: root.clone(),
            canonical_chain: vec![
                ChainEntry {
                    name: Vec::new(),
                    identity: "native-root-v1:unix:0000000000010306:0000000000000001".into(),
                },
                ChainEntry {
                    name: b"journal".to_vec(),
                    identity: root,
                },
            ],
            filesystem: "ext4".into(),
            storage_topology: "direct-pcie-nvme".into(),
            filesystem_device: Device {
                major: 259,
                minor: 6,
            },
            namespace_device: Device {
                major: 259,
                minor: 0,
            },
            partition: Some(1),
        })
    }

    #[test]
    fn strict_origin_data_and_identifier_parsers_reject_invalid_inputs() {
        let original = evidence();
        let bytes = serde_json::to_vec(&original).unwrap();
        assert_eq!(ArchiveOriginEvidence::from_json(&bytes).unwrap(), original);
        assert!(ArchiveOriginEvidence::from_json(&vec![b' '; MAX_EVIDENCE_BYTES + 1]).is_err());
        for mutate in 0..5 {
            let mut value = serde_json::to_value(&original).unwrap();
            match mutate {
                0 => value["extra"] = true.into(),
                1 => value["format_version"] = 2.into(),
                2 => value["canonical_chain"][1]["name"] = serde_json::json!([46, 46]),
                3 => value["filesystem_device"]["minor"] = 7.into(),
                _ => value["boot_id"] = "00000000-0000-0000-0000-000000000000".into(),
            }
            assert!(
                ArchiveOriginEvidence::from_json(&serde_json::to_vec(&value).unwrap()).is_err()
            );
        }
        let duplicate = String::from_utf8(bytes.clone()).unwrap().replacen(
            "\"format_version\":1",
            "\"format_version\":1,\"format_version\":1",
            1,
        );
        assert!(matches!(
            ArchiveOriginEvidence::from_json(duplicate.as_bytes()),
            Err(OriginEvidenceError::Decode(_))
        ));
        assert!(parse_boot(b"11111111-1111-1111-1111-111111111111\n").is_ok());
        assert!(parse_boot(b"11111111-1111-1111-1111-111111111111\n\n").is_err());
        assert!(machine_digest(b"00000000000000000000000000000000").is_err());
        assert!(machine_digest(b"0123456789ABCDEF0123456789abcdef").is_err());
    }

    #[test]
    fn application_digest_matches_independent_literal_pins() {
        assert_eq!(
            machine_digest(b"0123456789abcdef0123456789abcdef").unwrap(),
            "a47ef8ff387b17bc5e032d3ceb25651862323217fb51a57552cb5ef198bbaaa5"
        );
        assert_eq!(
            machine_digest(b"1123456789abcdef0123456789abcdef\n").unwrap(),
            "7817b98651dda23b368f5f31384b4b1f58f600b312ed351e1cd85785fba4912a"
        );
    }

    #[test]
    fn diagnostics_redact_paths_and_keep_the_original_error() {
        let error = OriginError::Root(JournalRootError::Io {
            operation: "fixture",
            path: "/private-origin-sentinel".into(),
            source: io::Error::from_raw_os_error(13),
        });
        assert!(!format!("{error:?} {error}").contains("private-origin-sentinel"));
        let OriginError::Root(JournalRootError::Io { path, source, .. }) = error else {
            panic!()
        };
        assert_eq!(path, std::path::Path::new("/private-origin-sentinel"));
        assert_eq!(source.raw_os_error(), Some(13));
        assert!(!format!("{:?}", evidence()).contains("11111111"));
    }

    #[cfg(all(target_os = "linux", feature = "full-tests"))]
    #[test]
    #[ignore = "requires an actual ext4/direct PCIe NVMe origin under /var/tmp"]
    fn actual_origin_revalidation_detects_replaced_ancestor_and_endpoint() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::Builder::new()
            .prefix("archive-origin-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let parent = temp.path().join("parent");
        let path = parent.join("journal");
        std::fs::create_dir_all(&path).unwrap();
        let root = Arc::new(JournalRoot::open(&path).unwrap());
        let origin =
            observe_archive_origin(Arc::clone(&root)).expect("actual supported host must observe");
        assert!(Arc::ptr_eq(&origin.root, &root));
        assert_eq!(
            origin.evidence.0.root_identity,
            root.identity().observation_id()
        );
        origin.revalidate().unwrap();
        let lease = crate::activity::ActivityLease::try_acquire(Arc::clone(&root)).unwrap();
        let carrier = crate::media::RootActivityCarrier::prepare(Arc::clone(&lease)).unwrap();
        let lock_path = path.join("backup-activity.lock");
        let lock_bytes = std::fs::read(&lock_path).unwrap();
        let binding = carrier
            .observe_recovery_binding(&origin, &"a".repeat(64))
            .unwrap();
        assert_eq!(binding.origin(), origin.evidence());
        assert_eq!(binding.repository_id(), "a".repeat(64));
        carrier.revalidate().unwrap();
        assert_eq!(std::fs::read(&lock_path).unwrap(), lock_bytes);
        let reopened = Arc::new(JournalRoot::open(&path).unwrap());
        let other_origin = observe_archive_origin(reopened).unwrap();
        assert!(
            carrier
                .observe_recovery_binding(&other_origin, &"a".repeat(64))
                .is_err()
        );
        let alias = path.join("lock-alias");
        std::fs::hard_link(&lock_path, &alias).unwrap();
        lease.revalidate().unwrap();
        assert!(
            carrier
                .observe_recovery_binding(&origin, &"a".repeat(64))
                .is_err()
        );
        std::fs::remove_file(alias).unwrap();
        carrier
            .observe_recovery_binding(&origin, &"a".repeat(64))
            .unwrap();
        let payload = carrier.diagnostic_path().join("unexpected");
        std::fs::write(&payload, b"held").unwrap();
        assert!(
            carrier
                .observe_recovery_binding(&origin, &"a".repeat(64))
                .is_err()
        );
        std::fs::remove_file(payload).unwrap();
        let displaced_lock = path.join("old-lock");
        std::fs::rename(&lock_path, &displaced_lock).unwrap();
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&lock_path)
            .unwrap();
        assert!(
            carrier
                .observe_recovery_binding(&origin, &"a".repeat(64))
                .is_err()
        );
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::rename(displaced_lock, &lock_path).unwrap();
        let json = serde_json::to_vec(origin.evidence()).unwrap();
        assert_eq!(
            ArchiveOriginEvidence::from_json(&json).unwrap(),
            *origin.evidence()
        );
        let old = temp.path().join("old");
        std::fs::rename(&parent, &old).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::rename(old.join("journal"), &path).unwrap();
        root.revalidate().unwrap();
        root.revalidate_canonical_binding().unwrap();
        assert!(matches!(
            origin.revalidate(),
            Err(OriginError::Changed(OriginRelation::CanonicalChain))
        ));
        assert!(Arc::ptr_eq(&origin.root, &root));
        let endpoint = parent.join("original");
        std::fs::rename(&path, &endpoint).unwrap();
        std::fs::create_dir(&path).unwrap();
        root.revalidate().unwrap();
        assert!(matches!(origin.revalidate(), Err(OriginError::Root(_))));
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&endpoint, &path).unwrap();
        let displaced = temp.path().join("displaced");
        std::fs::rename(&parent, &displaced).unwrap();
        symlink(&displaced, &parent).unwrap();
        root.revalidate().unwrap();
        assert!(matches!(origin.revalidate(), Err(OriginError::Root(_))));
        assert!(Arc::ptr_eq(&origin.root, &root));
    }
}
