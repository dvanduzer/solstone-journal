// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Explicit-teardown preparation custody. Drop closes only; it never sweeps.

use super::*;
use crate::activity::TeardownLease;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd};

pub const PREPARATIONS: &str = "teardown-preparations";
pub const MARKER: &str = "teardown-preparation.json";
pub const MARKER_CAP: usize = 384 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationObservation {
    pub version: u32,
    pub metadata_parent: MediaIdentity,
    pub source: StageObservation,
    pub destination: StageObservation,
}

/// Facts from this invocation only. A missing acknowledgment leaves the
/// syscall's possible effect unresolved; it never means "not applied".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PublicationFacts {
    pub rename_attempted: bool,
    pub rename_acknowledged: bool,
    pub destination_rebound: bool,
    pub source_parent_synced: bool,
    pub destination_parent_synced: bool,
}

#[cfg(all(test, feature = "full-tests"))]
#[cfg(not(target_os = "ios"))]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, Arc<TeardownLease>, TeardownPreparation) {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let lease = TeardownLease::try_acquire(root).unwrap();
        let preparation = TeardownPreparation::create(lease.clone()).unwrap();
        (temp, lease, preparation)
    }

    #[test]
    fn actual_create_only_directory_publication_preserves_occupied_destination() {
        let (temp, lease, mut preparation) = fixture();
        let observation = preparation.observation().clone();
        let marker = serde_json::to_vec(&observation).unwrap();
        preparation.write_marker(&marker).unwrap();
        let destination = temp.path().join(&observation.destination.directory_name);
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("foreign"), b"preserve").unwrap();
        assert!(preparation.publish().is_err());
        assert_eq!(fs::read(destination.join("foreign")).unwrap(), b"preserve");
        assert_eq!(preparation.marker().unwrap().unwrap().bytes, marker);
        assert!(TeardownPreparation::reopen(lease, &observation).is_err());
    }

    #[test]
    fn applied_error_and_each_parent_sync_failure_reconcile_exact_inode() {
        for primitive in [
            MediaPrimitive::TeardownPublishApplied,
            MediaPrimitive::TeardownSourceSync,
            MediaPrimitive::TeardownDestinationSync,
        ] {
            let (_temp, lease, mut preparation) = fixture();
            let expected = preparation.observation().clone();
            let bytes = serde_json::to_vec(&expected).unwrap();
            preparation.write_marker(&bytes).unwrap();
            let (result, consumed) = run_with_media_hook(
                primitive,
                || Err(io::Error::from_raw_os_error(libc::EIO)),
                || preparation.publish(),
            );
            assert!(result.is_err() && consumed, "{primitive:?}");
            drop(preparation);
            let mut recovered = TeardownPreparation::reopen(lease, &expected).unwrap();
            recovered.publish().unwrap();
            assert_eq!(
                recovered.published_directory().unwrap().identity(),
                expected.destination.directory_identity
            );
            assert_eq!(recovered.marker().unwrap().unwrap().bytes, bytes);
        }
    }

    #[test]
    fn copied_marker_and_replaced_parent_never_confer_directory_identity() {
        for replacement in ["directory", "parent", "unexpected_child"] {
            let (temp, lease, preparation) = fixture();
            let expected = preparation.observation().clone();
            let bytes = serde_json::to_vec(&expected).unwrap();
            preparation.write_marker(&bytes).unwrap();
            let parent = temp.path().join("backup").join(PREPARATIONS);
            let original = parent.join(&expected.source.directory_name);
            if replacement == "unexpected_child" {
                fs::write(original.join("foreign"), b"preserve").unwrap();
            } else if replacement == "directory" {
                fs::rename(&original, temp.path().join("original-preparation")).unwrap();
                fs::create_dir(&original).unwrap();
                fs::write(original.join(MARKER), &bytes).unwrap();
            } else {
                fs::rename(&parent, temp.path().join("original-parent")).unwrap();
                fs::create_dir(&parent).unwrap();
                fs::create_dir(&original).unwrap();
                fs::write(original.join(MARKER), &bytes).unwrap();
            }
            drop(preparation);
            match TeardownPreparation::reopen(lease, &expected) {
                Err(_) => assert_ne!(replacement, "unexpected_child"),
                Ok(mut recovered) => {
                    assert_eq!(replacement, "unexpected_child");
                    assert!(recovered.publish().is_err());
                }
            }
            assert!(original.is_dir());
            assert_eq!(fs::read(original.join(MARKER)).unwrap(), bytes);
        }
    }

    #[test]
    fn preparation_process_helper() {
        let Some(journal) = std::env::var_os("SOLSTONE_PREPARATION_TEST_JOURNAL") else {
            return;
        };
        let witness = std::env::var_os("SOLSTONE_PREPARATION_TEST_WITNESS").unwrap();
        let cut = std::env::var("SOLSTONE_PREPARATION_TEST_CUT").unwrap();
        let root = Arc::new(JournalRoot::open(Path::new(&journal)).unwrap());
        let lease = TeardownLease::try_acquire(root).unwrap();
        let mut preparation = TeardownPreparation::create(lease).unwrap();
        let bytes = serde_json::to_vec(preparation.observation()).unwrap();
        fs::write(witness, &bytes).unwrap();
        if cut == "before_marker" {
            std::process::exit(77);
        }
        preparation.write_marker(&bytes).unwrap();
        if cut == "marker" {
            std::process::exit(77);
        }
        let primitive = match cut.as_str() {
            "applied" => MediaPrimitive::TeardownPublishApplied,
            "between_syncs" => MediaPrimitive::TeardownDestinationSync,
            "synced" => {
                preparation.publish().unwrap();
                std::process::exit(77);
            }
            _ => panic!("unknown cut"),
        };
        let _ = run_with_media_hook(
            primitive,
            || std::process::exit(77),
            || preparation.publish(),
        );
        panic!("process cut did not run");
    }

    #[test]
    fn fresh_process_recovers_complete_units_and_holds_pre_marker_residue() {
        for cut in [
            "before_marker",
            "marker",
            "applied",
            "between_syncs",
            "synced",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let journal = temp.path().join("journal");
            fs::create_dir(&journal).unwrap();
            let witness = temp.path().join("witness.json");
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "media::teardown_preparation::tests::preparation_process_helper",
                    "--nocapture",
                ])
                .env("SOLSTONE_PREPARATION_TEST_JOURNAL", &journal)
                .env("SOLSTONE_PREPARATION_TEST_WITNESS", &witness)
                .env("SOLSTONE_PREPARATION_TEST_CUT", cut)
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(77),
                "{cut}: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            let expected: PreparationObservation =
                serde_json::from_slice(&fs::read(witness).unwrap()).unwrap();
            let root = Arc::new(JournalRoot::open(&journal).unwrap());
            let lease = TeardownLease::try_acquire(root).unwrap();
            let mut recovered = TeardownPreparation::reopen(lease, &expected).unwrap();
            if cut == "before_marker" {
                assert!(recovered.publish().is_err());
                assert!(
                    journal
                        .join("backup")
                        .join(PREPARATIONS)
                        .join(&expected.source.directory_name)
                        .is_dir()
                );
            } else {
                recovered.publish().unwrap();
                assert_eq!(
                    recovered.published_directory().unwrap().identity(),
                    expected.destination.directory_identity
                );
            }
        }
    }
}

/// The actual lease, original parents and actual directory survive uncertain
/// publication. A serialized observation cannot construct any of these handles.
pub struct TeardownPreparation {
    lease: Arc<TeardownLease>,
    metadata: Arc<MediaDirectory>,
    source_parent: Arc<MediaDirectory>,
    root: Arc<MediaDirectory>,
    directory: Arc<MediaDirectory>,
    observation: PreparationObservation,
    published: bool,
    publication: PublicationFacts,
}

impl TeardownPreparation {
    pub fn create(lease: Arc<TeardownLease>) -> Result<Self, Box<OwnedMediaFailure<Option<Self>>>> {
        let create = || -> io::Result<Self> {
            lease.revalidate().map_err(io::Error::other)?;
            let root = MediaDirectory::open(lease.journal_root().clone(), Path::new(""))?;
            let child = |parent: &Arc<MediaDirectory>,
                         name: &str|
             -> io::Result<Arc<MediaDirectory>> {
                match parent.open_child(OsStr::new(name))? {
                    Some(value) => Ok(value),
                    None => {
                        let inner = platform::create_directory(&parent.inner, OsStr::new(name))?;
                        let value = Arc::new(MediaDirectory { inner });
                        value.sync()?;
                        parent.sync()?;
                        Ok(value)
                    }
                }
            };
            let metadata = child(&root, "backup")?;
            let source_parent = child(&metadata, PREPARATIONS)?;
            if source_parent.entries(0)?.is_none() {
                return Err(changed());
            }
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
            let name = format!(".tmp-offload-{:032x}", u128::from_le_bytes(random));
            // Any failure after mkdir leaves explicit staging residue. No Drop
            // path or later empty-name sweep is allowed to remove it.
            let directory = Arc::new(MediaDirectory {
                inner: platform::create_directory(&source_parent.inner, OsStr::new(&name))?,
            });
            let source = StageObservation {
                journal_observation: lease.journal_root().identity().observation_id(),
                parent_relative: vec!["backup".into(), PREPARATIONS.into()],
                parent_identity: source_parent.identity(),
                directory_name: name.clone(),
                directory_identity: directory.identity(),
            };
            let destination = StageObservation {
                parent_relative: vec![],
                parent_identity: root.identity(),
                ..source.clone()
            };
            Ok(Self {
                lease: lease.clone(),
                metadata: metadata.clone(),
                source_parent,
                root,
                directory,
                observation: PreparationObservation {
                    version: 1,
                    metadata_parent: metadata.identity(),
                    source,
                    destination,
                },
                published: false,
                publication: PublicationFacts::default(),
            })
        };
        create().map_err(|error| Box::new(OwnedMediaFailure { error, owned: None }))
    }

    pub fn observation(&self) -> &PreparationObservation {
        &self.observation
    }

    pub fn reopen(
        lease: Arc<TeardownLease>,
        expected: &PreparationObservation,
    ) -> io::Result<Self> {
        lease.revalidate().map_err(io::Error::other)?;
        let root = MediaDirectory::open(lease.journal_root().clone(), Path::new(""))?;
        let metadata = root.open_child(OsStr::new("backup"))?.ok_or_else(changed)?;
        let source_parent = metadata
            .open_child(OsStr::new(PREPARATIONS))?
            .ok_or_else(changed)?;
        let suffix = expected
            .source
            .directory_name
            .strip_prefix(".tmp-offload-")
            .ok_or_else(changed)?;
        let mut destination = expected.source.clone();
        destination.parent_relative.clear();
        destination.parent_identity = root.identity();
        if expected.version != 1
            || expected.metadata_parent != metadata.identity()
            || expected.source.journal_observation
                != lease.journal_root().identity().observation_id()
            || expected.source.parent_relative != ["backup", PREPARATIONS]
            || expected.source.parent_identity != source_parent.identity()
            || suffix.len() != 32
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || expected.destination != destination
        {
            return Err(changed());
        }
        let source = source_parent.open_child(OsStr::new(&expected.source.directory_name))?;
        let target = root.open_child(OsStr::new(&expected.destination.directory_name))?;
        let (directory, published) = match (source, target) {
            (Some(source), None) => (source, false),
            (None, Some(target)) => (target, true),
            _ => return Err(changed()),
        };
        if directory.identity() != expected.source.directory_identity {
            return Err(changed());
        }
        let value = Self {
            lease,
            metadata,
            source_parent,
            root,
            directory,
            observation: expected.clone(),
            published,
            publication: PublicationFacts::default(),
        };
        value.revalidate()?;
        Ok(value)
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.lease.revalidate().map_err(io::Error::other)?;
        self.metadata.revalidate()?;
        self.source_parent.revalidate()?;
        self.root.revalidate()?;
        self.directory.revalidate()?;
        if self.metadata.identity() != self.observation.metadata_parent
            || self.source_parent.identity() != self.observation.source.parent_identity
            || self.root.identity() != self.observation.destination.parent_identity
            || self.directory.identity() != self.observation.destination.directory_identity
        {
            return Err(changed());
        }
        if self.published
            && self
                .source_parent
                .open_child(OsStr::new(&self.observation.source.directory_name))?
                .is_some()
        {
            return Err(changed());
        }
        Ok(())
    }

    pub fn write_marker(&self, bytes: &[u8]) -> io::Result<()> {
        self.revalidate()?;
        if self.published
            || bytes.is_empty()
            || bytes.len() > MARKER_CAP
            || self.directory.entries(0)?.is_none()
        {
            return Err(changed());
        }
        let mut file = platform::create_file(&self.directory.inner, OsStr::new(MARKER))?;
        step(MediaPrimitive::PreparationWrite)?;
        file.write_all(bytes)?;
        step(MediaPrimitive::FileSync)?;
        file.sync_all()?;
        self.revalidate()?;
        self.directory.sync()?;
        self.source_parent.sync()?;
        self.marker()?
            .filter(|read| read.bytes == bytes)
            .ok_or_else(changed)?;
        Ok(())
    }

    /// Complete directory census: only the exact marker may precede transfer.
    pub fn marker(&self) -> io::Result<Option<MediaRead>> {
        self.revalidate()?;
        let entries = self.directory.entries(1)?.ok_or_else(changed)?;
        if entries.is_empty() {
            return Ok(None);
        }
        if entries[0].name != OsStr::new(MARKER) {
            return Err(changed());
        }
        self.directory
            .read_file_bounded(OsStr::new(MARKER), MARKER_CAP)
    }

    /// Create-only cross-parent publication. On any applied error this object
    /// still retains the original descriptors; a new explicit invocation can
    /// reconcile the marker and exact inode, then establish both parent syncs.
    pub fn publish(&mut self) -> io::Result<()> {
        self.marker()?.ok_or_else(changed)?;
        if !self.published {
            let from = CString::new(self.observation.source.directory_name.as_bytes())
                .map_err(io::Error::other)?;
            let to = CString::new(self.observation.destination.directory_name.as_bytes())
                .map_err(io::Error::other)?;
            step(MediaPrimitive::TeardownBeforePublish)?;
            self.revalidate()?;
            self.publication.rename_attempted = true;
            let result = crate::claim_remove::platform_rename_no_replace(
                self.source_parent.inner.as_fd().as_raw_fd(),
                &from,
                self.root.inner.as_fd().as_raw_fd(),
                &to,
            )?;
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            self.publication.rename_acknowledged = true;
            step(MediaPrimitive::TeardownPublishApplied)?;
            let directory = self
                .root
                .open_child(OsStr::new(&self.observation.destination.directory_name))?
                .ok_or_else(changed)?;
            if directory.identity() != self.observation.destination.directory_identity {
                return Err(changed());
            }
            self.directory = directory;
            self.published = true;
            self.publication.destination_rebound = true;
        }
        self.revalidate()?;
        step(MediaPrimitive::TeardownSourceSync)?;
        self.source_parent.sync()?;
        self.publication.source_parent_synced = true;
        self.revalidate()?;
        step(MediaPrimitive::TeardownDestinationSync)?;
        self.root.sync()?;
        self.publication.destination_parent_synced = true;
        self.revalidate()?;
        self.marker()?.ok_or_else(changed)?;
        Ok(())
    }

    pub fn published_directory(&self) -> io::Result<&Arc<MediaDirectory>> {
        self.revalidate()?;
        if !self.published {
            return Err(changed());
        }
        Ok(&self.directory)
    }

    pub fn publication_facts(&self) -> PublicationFacts {
        self.publication
    }
}
