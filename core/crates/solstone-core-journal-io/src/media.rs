// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Operation-owned, bounded media I/O. No pathname constitutes cleanup authority.
//!
//! Helper completion is deliberately a caller obligation: only the adapter that
//! proves its actual writer has exited may call `prepare_after_successful_helper`
//! or `cleanup_after_helper_quiescence`. A generic runner success or process-group
//! signal is insufficient. Unknown completion retains the entire owned target.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::JournalRoot;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

const BUFFER_BYTES: usize = 64 * 1024;

/// Narrow mutation/observation barriers for native fault evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaPrimitive {
    CapacityQuery,
    PreparationRead,
    PreparationWrite,
    FileSync,
    BeforePublish,
    AfterPublish,
    ParentSync,
    StageUnlink,
    StageRemove,
    Completion,
}

#[cfg(any(test, feature = "test-hooks"))]
type Hook = (MediaPrimitive, Box<dyn FnOnce() -> io::Result<()>>);
#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(any(test, feature = "test-hooks"))]
pub fn run_with_media_hook<T>(
    primitive: MediaPrimitive,
    hook: impl FnOnce() -> io::Result<()> + 'static,
    operation: impl FnOnce() -> T,
) -> (T, bool) {
    HOOK.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some((primitive, Box::new(hook)));
    });
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    let _guard = Guard;
    let result = operation();
    let consumed = HOOK.with(|slot| slot.borrow().is_none());
    (result, consumed)
}

fn step(primitive: MediaPrimitive) -> io::Result<()> {
    #[cfg(any(test, feature = "test-hooks"))]
    {
        let callback = HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|(candidate, _)| *candidate == primitive)
            {
                slot.take().map(|(_, callback)| callback)
            } else {
                None
            }
        });
        if let Some(callback) = callback {
            return callback();
        }
    }
    let _ = primitive;
    Ok(())
}

/// Lossless physical identity. Persisted witnesses never grant cleanup authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum MediaIdentity {
    Unix { device: u64, inode: u64 },
    Windows { volume: u64, file_id: [u8; 16] },
}

impl MediaIdentity {
    fn same_volume(self, other: Self) -> bool {
        match (self, other) {
            (Self::Unix { device: a, .. }, Self::Unix { device: b, .. }) => a == b,
            (Self::Windows { volume: a, .. }, Self::Windows { volume: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// Size and SHA256, without retained content bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaDigest {
    pub size: u64,
    pub sha256: String,
}

/// Hash a reader with one fixed buffer. Interrupted reads are retried.
pub fn digest_reader(reader: &mut impl Read) -> io::Result<MediaDigest> {
    stream(reader, &mut io::sink(), None)
}

fn stream(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum: Option<u64>,
) -> io::Result<MediaDigest> {
    let mut buffer = [0u8; BUFFER_BYTES];
    let mut size = 0u64;
    let mut hash = Sha256::new();
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            break;
        }
        size = checked_length(size, count as u64)?;
        if maximum.is_some_and(|maximum| size > maximum) {
            return Err(invalid("media exceeds expected size"));
        }
        writer.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
    }
    Ok(MediaDigest {
        size,
        sha256: format!("{:x}", hash.finalize()),
    })
}

fn checked_length(current: u64, count: u64) -> io::Result<u64> {
    current
        .checked_add(count)
        .ok_or_else(|| invalid("media length overflow"))
}

/// Existing-only directory with its entire admitted chain retained.
pub struct MediaDirectory {
    inner: platform::Directory,
}

impl MediaDirectory {
    /// Admit an existing relative directory; never creates any component.
    pub fn open(root: Arc<JournalRoot>, relative: &Path) -> io::Result<Arc<Self>> {
        let names = components(relative)?;
        Ok(Arc::new(Self {
            inner: platform::Directory::open(root, &names)?,
        }))
    }

    pub fn identity(&self) -> MediaIdentity {
        self.inner.identity()
    }

    /// Diagnostic spelling only, unsuitable for helper targets or cleanup.
    pub fn diagnostic_path(&self) -> &Path {
        self.inner.path()
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.inner.revalidate()
    }

    /// Available bytes on this retained directory's filesystem, with checked
    /// arithmetic and binding validation surrounding the native query.
    pub fn available_bytes(&self) -> io::Result<u64> {
        self.revalidate()?;
        step(MediaPrimitive::CapacityQuery)?;
        let available = self.inner.available_bytes()?;
        self.revalidate()?;
        Ok(available)
    }

    /// Observe one no-follow plain file with bounded streaming. Non-files refuse.
    pub fn observe_file(&self, name: &OsStr) -> io::Result<Option<MediaObservation>> {
        plain_name(name)?;
        self.revalidate()?;
        let Some(mut file) = platform::open_plain(&self.inner, name)? else {
            return Ok(None);
        };
        let identity = platform::identity(&file)?;
        let before = platform::stamp(&file)?;
        let maximum = file.metadata()?.len();
        let digest = stream(&mut file, &mut io::sink(), Some(maximum))?;
        if before != platform::stamp(&file)? || platform::identity(&file)? != identity {
            return Err(changed());
        }
        platform::check_leaf(&self.inner, name, identity)?;
        self.revalidate()?;
        Ok(Some(MediaObservation { identity, digest }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaObservation {
    pub identity: MediaIdentity,
    pub digest: MediaDigest,
}

/// An error carrying the owned object even after a partial mutation.
pub struct OwnedMediaFailure<T> {
    pub error: io::Error,
    pub owned: T,
}

impl<T> std::fmt::Debug for OwnedMediaFailure<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedMediaFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

struct Leaf {
    file: File,
    identity: Option<MediaIdentity>,
}

/// Exactly one private sibling stage. Drop only closes handles, never unlinks.
pub struct MediaStage {
    parent: Arc<MediaDirectory>,
    name: OsString,
    directory: Option<Arc<MediaDirectory>>,
    leaf: Option<Leaf>,
    removed: bool,
}

impl MediaStage {
    /// Create a unique `.tmp-offload-` sibling. Failures after creation retain
    /// the reservation, even when its identity could not be established.
    pub fn create(parent: Arc<MediaDirectory>) -> Result<Self, OwnedMediaFailure<Option<Self>>> {
        if let Err(error) = platform::supported_mutation().and_then(|()| parent.revalidate()) {
            return Err(OwnedMediaFailure { error, owned: None });
        }
        let mut random = [0u8; 16];
        if let Err(error) = getrandom::fill(&mut random) {
            return Err(OwnedMediaFailure {
                error: io::Error::other(error.to_string()),
                owned: None,
            });
        }
        let name = OsString::from(format!(".tmp-offload-{:032x}", u128::from_le_bytes(random)));
        if let Err(error) = platform::create_directory(&parent.inner, &name) {
            return Err(OwnedMediaFailure { error, owned: None });
        }
        let mut stage = Self {
            parent,
            name,
            directory: None,
            leaf: None,
            removed: false,
        };
        match stage.parent.inner.open_child(&stage.name) {
            Ok(inner) => {
                stage.directory = Some(Arc::new(MediaDirectory { inner }));
                Ok(stage)
            }
            Err(error) => Err(OwnedMediaFailure {
                error,
                owned: Some(stage),
            }),
        }
    }

    pub fn diagnostic_path(&self) -> PathBuf {
        self.parent.diagnostic_path().join(&self.name)
    }

    pub fn available_bytes(&self) -> io::Result<u64> {
        self.directory
            .as_ref()
            .ok_or_else(changed)?
            .available_bytes()
    }

    /// Create the sole fresh, empty, offset-zero payload and transfer stage ownership.
    pub fn create_target(mut self) -> Result<OwnedMediaTarget, OwnedMediaFailure<Self>> {
        let result = (|| {
            let directory = self.directory.as_ref().ok_or_else(changed)?;
            directory.revalidate()?;
            let file = platform::create_file(&directory.inner, OsStr::new("payload"))?;
            self.leaf = Some(Leaf {
                file,
                identity: None,
            });
            let leaf = self.leaf.as_mut().expect("created leaf retained");
            leaf.identity = Some(platform::identity(&leaf.file)?);
            if leaf.file.metadata()?.len() != 0 || leaf.file.stream_position()? != 0 {
                return Err(invalid("media target is not fresh"));
            }
            directory.revalidate()
        })();
        match result {
            Ok(()) => {
                let identity = self
                    .leaf
                    .as_ref()
                    .and_then(|leaf| leaf.identity)
                    .expect("admitted target identity");
                Ok(OwnedMediaTarget {
                    stage: Box::new(self),
                    identity,
                    helper_taken: false,
                })
            }
            Err(error) => Err(OwnedMediaFailure { error, owned: self }),
        }
    }

    /// Cleanup of an unlaunched stage only; launched targets hide this stage.
    pub fn cleanup(&mut self) -> Cleanup {
        match self.cleanup_exact() {
            Ok(()) => Cleanup::Complete,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Cleanup::Refused,
            Err(_) => Cleanup::Pending,
        }
    }

    fn cleanup_exact(&mut self) -> io::Result<()> {
        if self.removed {
            return Ok(());
        }
        self.parent.revalidate()?;
        let directory = self.directory.as_ref().ok_or_else(changed)?;
        directory.revalidate()?;
        if let Some(leaf) = &self.leaf {
            let identity = leaf.identity.ok_or_else(changed)?;
            platform::check_leaf(&directory.inner, OsStr::new("payload"), identity)?;
            step(MediaPrimitive::StageUnlink)?;
            platform::check_leaf(&directory.inner, OsStr::new("payload"), identity)?;
            platform::remove_leaf(&directory.inner, OsStr::new("payload"), &leaf.file)?;
            self.leaf = None;
        }
        step(MediaPrimitive::StageRemove)?;
        directory.revalidate()?;
        platform::remove_directory(&self.parent.inner, &self.name, &directory.inner)?;
        self.removed = true;
        Ok(())
    }
}

/// One invocation's target; no writable file or live pathname accessor exists.
pub struct OwnedMediaTarget {
    stage: Box<MediaStage>,
    identity: MediaIdentity,
    helper_taken: bool,
}

#[cfg(unix)]
pub struct HelperTransport<'a> {
    pub target: PathBuf,
    pub descriptor: std::os::fd::BorrowedFd<'a>,
}

impl OwnedMediaTarget {
    pub fn identity(&self) -> MediaIdentity {
        self.identity
    }

    pub fn available_bytes(&self) -> io::Result<u64> {
        self.stage.available_bytes()
    }

    #[cfg(unix)]
    pub fn helper_transport(&mut self) -> io::Result<HelperTransport<'_>> {
        use std::os::fd::{AsFd, AsRawFd};
        if self.helper_taken {
            return Err(invalid("media target already used by a helper"));
        }
        self.revalidate()?;
        let file = &self.stage.leaf.as_ref().expect("target leaf").file;
        if file.metadata()?.len() != 0 {
            return Err(invalid("helper target is not empty"));
        }
        self.helper_taken = true;
        #[cfg(target_os = "linux")]
        let target = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        #[cfg(not(target_os = "linux"))]
        let target = PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()));
        Ok(HelperTransport {
            target,
            descriptor: file.as_fd(),
        })
    }

    /// Windows path-helper containment has not been qualified. Refuse prelaunch.
    #[cfg(windows)]
    pub fn helper_transport(&mut self) -> io::Result<()> {
        Err(unsupported())
    }

    /// Caller must prove the actual helper writer has quiesced with successful
    /// execution. No runner status or supplied boolean grants that proof here.
    pub fn prepare_after_successful_helper(
        self,
        expected: &MediaDigest,
    ) -> Result<PreparedMedia, OwnedMediaFailure<Self>> {
        if !self.helper_taken {
            return Err(OwnedMediaFailure {
                error: invalid("helper target was not issued"),
                owned: self,
            });
        }
        self.prepare(expected)
    }

    /// Caller must independently prove actual writer quiescence, including on
    /// nonzero exit. Unknown writer lifetime must retain this object instead.
    pub fn cleanup_after_helper_quiescence(&mut self) -> Cleanup {
        self.stage.cleanup()
    }

    /// Copy one retained no-follow plain source into this fresh target, streaming.
    pub fn copy_from(
        mut self,
        source: &MediaDirectory,
        name: &OsStr,
        expected: &MediaDigest,
    ) -> Result<PreparedMedia, OwnedMediaFailure<Self>> {
        let result = (|| {
            if self.helper_taken {
                return Err(invalid("helper target cannot be reused"));
            }
            plain_name(name)?;
            self.revalidate()?;
            source.revalidate()?;
            let mut input = platform::open_plain(&source.inner, name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            let identity = platform::identity(&input)?;
            let before = platform::stamp(&input)?;
            let output = &mut self.stage.leaf.as_mut().expect("target leaf").file;
            if output.metadata()?.len() != 0 || output.stream_position()? != 0 {
                return Err(invalid("local copy target is not fresh"));
            }
            step(MediaPrimitive::PreparationWrite)?;
            let digest = stream(&mut input, output, Some(expected.size))?;
            if before != platform::stamp(&input)? || digest != *expected {
                return Err(changed());
            }
            platform::check_leaf(&source.inner, name, identity)?;
            source.revalidate()
        })();
        if let Err(error) = result {
            return Err(OwnedMediaFailure { error, owned: self });
        }
        self.prepare(expected)
    }

    fn revalidate(&self) -> io::Result<()> {
        let directory = self.stage.directory.as_ref().ok_or_else(changed)?;
        directory.revalidate()?;
        let leaf = self.stage.leaf.as_ref().ok_or_else(changed)?;
        if platform::identity(&leaf.file)? != self.identity() {
            return Err(changed());
        }
        platform::check_leaf(&directory.inner, OsStr::new("payload"), self.identity())
    }

    fn prepare(mut self, expected: &MediaDigest) -> Result<PreparedMedia, OwnedMediaFailure<Self>> {
        let result = (|| {
            self.revalidate()?;
            let file = &mut self.stage.leaf.as_mut().expect("target leaf").file;
            let before = platform::stamp(file)?;
            file.seek(SeekFrom::Start(0))?;
            step(MediaPrimitive::PreparationRead)?;
            let digest = stream(file, &mut io::sink(), Some(expected.size))?;
            if digest != *expected {
                return Err(invalid("media size or SHA256 mismatch"));
            }
            if before != platform::stamp(file)? {
                return Err(changed());
            }
            step(MediaPrimitive::FileSync)?;
            file.sync_all()?;
            if before != platform::stamp(file)? {
                return Err(changed());
            }
            self.revalidate()?;
            Ok((digest, before))
        })();
        match result {
            Ok((digest, stamp)) => Ok(PreparedMedia {
                target: self,
                digest,
                stamp,
            }),
            Err(error) => Err(OwnedMediaFailure { error, owned: self }),
        }
    }
}

/// A quiescent, independently hashed and file-synced owned source.
pub struct PreparedMedia {
    target: OwnedMediaTarget,
    digest: MediaDigest,
    stamp: platform::Stamp,
}

impl std::fmt::Debug for PreparedMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedMedia")
            .field("identity", &self.identity())
            .field("digest", &self.digest)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationEffect {
    NotPublished,
    Published,
    AlreadyPresent,
    Conflict,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirmation {
    Confirmed,
    Unverified,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    Synced,
    Unproven,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    Complete,
    Pending,
    Refused,
}

#[derive(Debug)]
pub struct PublicationReceipt {
    pub effect: PublicationEffect,
    pub confirmation: Confirmation,
    pub file_durability: Durability,
    pub parent_durability: Durability,
    pub cleanup: Cleanup,
    pub identity: Option<MediaIdentity>,
    pub error: Option<io::Error>,
}

impl PreparedMedia {
    pub fn digest(&self) -> &MediaDigest {
        &self.digest
    }
    pub fn identity(&self) -> MediaIdentity {
        self.target.identity()
    }
    pub fn cleanup(&mut self) -> Cleanup {
        self.target.stage.cleanup()
    }

    /// Create-only publication. Any postmutation failure preserves Published;
    /// receipts keep effect, confirmation, durability and cleanup independent.
    pub fn publish(&mut self, destination: &MediaDirectory, name: &OsStr) -> PublicationReceipt {
        let mut receipt = PublicationReceipt {
            effect: PublicationEffect::NotPublished,
            confirmation: Confirmation::Unverified,
            file_durability: Durability::Synced,
            parent_durability: Durability::Unproven,
            cleanup: Cleanup::Pending,
            identity: None,
            error: None,
        };
        let result = (|| {
            plain_name(name)?;
            platform::supported_mutation()?;
            self.target.revalidate()?;
            if platform::stamp(&self.target.stage.leaf.as_ref().expect("prepared leaf").file)?
                != self.stamp
            {
                return Err(changed());
            }
            destination.revalidate()?;
            if !self.identity().same_volume(destination.identity()) {
                return Err(invalid("media publication crosses volumes"));
            }
            let directory = self
                .target
                .stage
                .directory
                .as_ref()
                .expect("prepared stage");
            step(MediaPrimitive::BeforePublish)?;
            self.target.revalidate()?;
            if platform::stamp(&self.target.stage.leaf.as_ref().expect("prepared leaf").file)?
                != self.stamp
            {
                return Err(changed());
            }
            destination.revalidate()?;
            match platform::publish(
                &directory.inner,
                OsStr::new("payload"),
                &self.target.stage.leaf.as_ref().expect("prepared leaf").file,
                &destination.inner,
                name,
            ) {
                Ok(()) => {
                    receipt.effect = PublicationEffect::Published;
                    receipt.identity = Some(self.identity());
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match destination.observe_file(name) {
                        Ok(Some(observed)) if observed.digest == self.digest => {
                            receipt.effect = PublicationEffect::AlreadyPresent;
                            receipt.file_durability = Durability::Unproven;
                            receipt.identity = Some(observed.identity);
                        }
                        Ok(_) => {
                            receipt.effect = PublicationEffect::Conflict;
                            return Ok(());
                        }
                        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                            receipt.effect = PublicationEffect::Conflict;
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
            step(MediaPrimitive::AfterPublish)?;
            // Final-name confirmation includes content and retained chain checks.
            let observation = destination.observe_file(name)?.ok_or_else(changed)?;
            if Some(observation.identity) != receipt.identity || observation.digest != self.digest {
                return Err(changed());
            }
            destination.revalidate()?;
            receipt.confirmation = Confirmation::Confirmed;
            if step(MediaPrimitive::ParentSync)
                .and_then(|()| destination.inner.sync())
                .is_ok()
            {
                receipt.parent_durability = Durability::Synced
            }
            receipt.cleanup = self.cleanup();
            // Cleanup/sync are separate effects; confirm again at completion.
            step(MediaPrimitive::Completion)?;
            let completion = destination.observe_file(name)?.ok_or_else(changed)?;
            if Some(completion.identity) != receipt.identity || completion.digest != self.digest {
                return Err(changed());
            }
            destination.revalidate()?;
            Ok(())
        })();
        if let Err(error) = result {
            receipt.confirmation = Confirmation::Unverified;
            receipt.error = Some(error)
        }
        receipt
    }
}

fn components(relative: &Path) -> io::Result<Vec<OsString>> {
    relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => {
                plain_name(name)?;
                Ok(name.to_os_string())
            }
            _ => Err(invalid("media directory requires a plain relative path")),
        })
        .collect()
}

fn plain_name(name: &OsStr) -> io::Result<()> {
    let text = name
        .to_str()
        .ok_or_else(|| invalid("media name must be portable UTF-8"))?;
    crate::name_admission::check_portable_component(text)
        .map_err(|_| invalid("invalid media basename"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn changed() -> io::Error {
    invalid("retained media binding changed")
}
#[cfg(windows)]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "native media helper containment is unproven",
    )
}

#[cfg(test)]
mod tests;
