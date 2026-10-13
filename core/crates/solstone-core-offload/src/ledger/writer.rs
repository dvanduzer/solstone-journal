// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Retained append facts. A durable record is never release authority.

use std::sync::Arc;

use solstone_core_journal_io::{JournalRoot, paths::SegmentLayout};
use solstone_core_retention::{archive_proof::OffloadProof, receipt::Target};

use solstone_core_retention::archive_catalog::{
    LegacyRestoreBinding, LegacyRestoreOperation, RestoreOperation,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LedgerWriteRefusal {
    InvalidInput,
    RootMismatch,
    Busy,
    HistoryUnavailable(String),
    Collision,
    Changed,
    Io(String),
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LedgerWriteEffect {
    AlreadyRecorded,
    Appended { bytes: usize },
    Attempted { written: Option<usize> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LedgerSync {
    Confirmed,
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LedgerConfirmation {
    Confirmed,
    Uncertain,
}

#[derive(Debug)]
pub(crate) struct LedgerWriteReceipt {
    effect: LedgerWriteEffect,
    file_sync: LedgerSync,
    offload_sync: LedgerSync,
    health_sync: LedgerSync,
    root_sync: LedgerSync,
    confirmation: LedgerConfirmation,
    failures: Vec<String>,
}

impl LedgerWriteReceipt {
    pub(crate) fn effect(&self) -> LedgerWriteEffect {
        self.effect
    }
    pub(crate) fn durability(&self) -> [LedgerSync; 4] {
        [
            self.file_sync,
            self.offload_sync,
            self.health_sync,
            self.root_sync,
        ]
    }
    pub(crate) fn confirmation(&self) -> LedgerConfirmation {
        self.confirmation
    }
    pub(crate) fn failures(&self) -> &[String] {
        &self.failures
    }
    pub(crate) fn settles(&self) -> bool {
        self.confirmation == LedgerConfirmation::Confirmed
            && self.failures.is_empty()
            && !matches!(self.effect, LedgerWriteEffect::Attempted { .. })
            && self.durability()
                == if cfg!(windows) {
                    [
                        LedgerSync::Confirmed,
                        LedgerSync::Uncertain,
                        LedgerSync::Uncertain,
                        LedgerSync::Uncertain,
                    ]
                } else {
                    [LedgerSync::Confirmed; 4]
                }
    }
}

fn io_refusal(error: impl ToString) -> LedgerWriteRefusal {
    LedgerWriteRefusal::Io(error.to_string())
}

pub(crate) fn append_archive_witness(
    root: &Arc<JournalRoot>,
    proof: &OffloadProof,
    time: u64,
) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
    #[cfg(unix)]
    {
        unix::append_archive(root, proof, time, &mut unix::NoHooks)
    }
    #[cfg(windows)]
    {
        windows::append_archive(root, proof, time)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, proof, time);
        Err(LedgerWriteRefusal::Unsupported)
    }
}

/// Resync an exact existing complete manifest without republishing missing history.
pub(crate) fn confirm_archive_witness(
    root: &Arc<JournalRoot>,
    proof: &OffloadProof,
    time: u64,
) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
    #[cfg(unix)]
    {
        unix::confirm_archive(root, proof, time, &mut unix::NoHooks)
    }
    #[cfg(windows)]
    {
        windows::confirm_archive(root, proof, time)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, proof, time);
        Err(LedgerWriteRefusal::Unsupported)
    }
}

pub(crate) fn append_restore_operation(
    root: &Arc<JournalRoot>,
    target: &Target,
    layout: SegmentLayout,
    operation: &RestoreOperation,
    time: u64,
) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
    #[cfg(unix)]
    {
        unix::append_restore(root, target, layout, operation, time, &mut unix::NoHooks)
    }
    #[cfg(windows)]
    {
        windows::append_restore(root, target, layout, operation, time)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, target, layout, operation, time);
        Err(LedgerWriteRefusal::Unsupported)
    }
}

pub(crate) fn append_legacy_restore_binding(
    root: &Arc<JournalRoot>,
    binding: &LegacyRestoreBinding,
    time: u64,
) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
    #[cfg(unix)]
    {
        unix::append_legacy_binding(root, binding, time, &mut unix::NoHooks)
    }
    #[cfg(windows)]
    {
        windows::append_legacy_binding(root, binding, time)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, binding, time);
        Err(LedgerWriteRefusal::Unsupported)
    }
}

pub(crate) fn append_legacy_restore_operation(
    root: &Arc<JournalRoot>,
    binding: &LegacyRestoreBinding,
    operation: &LegacyRestoreOperation,
    time: u64,
) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
    #[cfg(unix)]
    {
        unix::append_legacy_restore(root, binding, operation, time, &mut unix::NoHooks)
    }
    #[cfg(windows)]
    {
        windows::append_legacy_restore(root, binding, operation, time)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, binding, operation, time);
        Err(LedgerWriteRefusal::Unsupported)
    }
}

mod plan {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use solstone_core_format::segment::segment_key;
    use solstone_core_journal_io::JournalRoot;
    use solstone_core_retention::archive_catalog::{
        JournalCatalog, LegacyRestoreBinding, LegacyRestoreOperation, RestoreOperation,
        SegmentCatalog,
        codec::{Event, component, fold_lines, valid_day},
    };

    use super::*;

    pub(super) struct Request {
        pub(super) target: Target,
        pub(super) layout: SegmentLayout,
        pub(super) segment: String,
        pub(super) event: Event,
        pub(super) expected_binding: Option<LegacyRestoreBinding>,
        pub(super) mode: AppendMode,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum AppendMode {
        AppendOrConfirm,
        ConfirmExisting,
    }

    pub(super) fn encode(event: &Event) -> Result<Vec<u8>, LedgerWriteRefusal> {
        let mut bytes = serde_json::to_vec(event).map_err(io_refusal)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub(super) fn append_archive(
        root: &Arc<JournalRoot>,
        proof: &OffloadProof,
        time: u64,
    ) -> Result<Request, LedgerWriteRefusal> {
        archive_record(root, proof, time, AppendMode::AppendOrConfirm)
    }

    pub(super) fn confirm_archive(
        root: &Arc<JournalRoot>,
        proof: &OffloadProof,
        time: u64,
    ) -> Result<Request, LedgerWriteRefusal> {
        archive_record(root, proof, time, AppendMode::ConfirmExisting)
    }

    pub(super) fn archive_record(
        root: &Arc<JournalRoot>,
        proof: &OffloadProof,
        time: u64,
        mode: AppendMode,
    ) -> Result<Request, LedgerWriteRefusal> {
        proof
            .validate()
            .map_err(|_| LedgerWriteRefusal::InvalidInput)?;
        if proof.journal_id != root.identity().observation_id() {
            return Err(LedgerWriteRefusal::RootMismatch);
        }
        let segment = segment_key(&proof.target.dir).ok_or(LedgerWriteRefusal::InvalidInput)?;
        let event = Event::ArchiveV2 {
            schema_version: 2,
            time,
            day: proof.target.day.clone(),
            stream: proof.target.stream.clone(),
            segment: segment.clone(),
            segment_dir: proof.target.dir.clone(),
            layout: proof.layout,
            journal_id: proof.journal_id.clone(),
            manifest_id: proof.manifest_id.clone(),
            repository_id: proof.repository_id.clone(),
            snapshot_id: proof.snapshot_id.clone(),
            files: proof.files.clone(),
        };
        if fold_lines(&proof.target.day, &encode(&event)?).degraded() {
            return Err(LedgerWriteRefusal::InvalidInput);
        }
        Ok(Request {
            target: proof.target.clone(),
            layout: proof.layout,
            segment,
            event,
            expected_binding: None,
            mode,
        })
    }

    pub(super) fn append_restore(
        _root: &Arc<JournalRoot>,
        target: &Target,
        layout: SegmentLayout,
        operation: &RestoreOperation,
        time: u64,
    ) -> Result<Request, LedgerWriteRefusal> {
        if !valid_day(&target.day)
            || !component(&target.stream)
            || !component(&target.dir)
            || operation.operation_id.is_empty()
            || operation.files.is_empty()
        {
            return Err(LedgerWriteRefusal::InvalidInput);
        }
        let segment = segment_key(&target.dir).ok_or(LedgerWriteRefusal::InvalidInput)?;
        let event = Event::RestoreV2 {
            schema_version: 2,
            time,
            day: target.day.clone(),
            stream: target.stream.clone(),
            segment: segment.clone(),
            operation_id: operation.operation_id.clone(),
            files: operation.files.clone(),
        };
        Ok(Request {
            target: target.clone(),
            layout,
            segment,
            event,
            expected_binding: None,
            mode: AppendMode::AppendOrConfirm,
        })
    }

    pub(super) fn append_legacy_binding(
        _root: &Arc<JournalRoot>,
        binding: &LegacyRestoreBinding,
        time: u64,
    ) -> Result<Request, LedgerWriteRefusal> {
        binding
            .validate()
            .map_err(|_| LedgerWriteRefusal::InvalidInput)?;
        let segment = segment_key(&binding.target.dir).ok_or(LedgerWriteRefusal::InvalidInput)?;
        Ok(Request {
            target: binding.target.clone(),
            layout: binding.layout,
            segment: segment.clone(),
            event: Event::RestoreBindingV1 {
                schema_version: 1,
                time,
                day: binding.target.day.clone(),
                stream: binding.target.stream.clone(),
                segment,
                binding: binding.clone(),
            },
            expected_binding: None,
            mode: AppendMode::AppendOrConfirm,
        })
    }

    pub(super) fn append_legacy_restore(
        _root: &Arc<JournalRoot>,
        binding: &LegacyRestoreBinding,
        operation: &LegacyRestoreOperation,
        time: u64,
    ) -> Result<Request, LedgerWriteRefusal> {
        binding
            .validate()
            .map_err(|_| LedgerWriteRefusal::InvalidInput)?;
        if operation.adoption_id != binding.adoption_id
            || !component(&operation.operation.operation_id)
            || operation.operation.files.is_empty()
        {
            return Err(LedgerWriteRefusal::InvalidInput);
        }
        let segment = segment_key(&binding.target.dir).ok_or(LedgerWriteRefusal::InvalidInput)?;
        Ok(Request {
            target: binding.target.clone(),
            layout: binding.layout,
            segment: segment.clone(),
            event: Event::LegacyRestoreV1 {
                schema_version: 1,
                time,
                day: binding.target.day.clone(),
                stream: binding.target.stream.clone(),
                segment,
                segment_dir: binding.target.dir.clone(),
                layout: binding.layout,
                adoption_id: operation.adoption_id.clone(),
                operation_id: operation.operation.operation_id.clone(),
                files: operation.operation.files.clone(),
            },
            expected_binding: Some(binding.clone()),
            mode: AppendMode::AppendOrConfirm,
        })
    }

    pub(super) fn unique_ids(catalog: &JournalCatalog) -> Result<(), LedgerWriteRefusal> {
        let mut manifests = BTreeSet::new();
        let mut witnesses = BTreeSet::new();
        let mut operations = BTreeSet::new();
        let mut adoptions = BTreeSet::new();
        for segment in catalog.days.iter().flat_map(|d| &d.segments) {
            for manifest in &segment.history {
                if !manifests.insert(&manifest.manifest_id) {
                    return Err(LedgerWriteRefusal::Collision);
                }
                for witness in &manifest.files {
                    if !witnesses.insert(&witness.witness_id) {
                        return Err(LedgerWriteRefusal::Collision);
                    }
                }
            }
            for operation in &segment.restores {
                if !operations.insert(&operation.operation_id) {
                    return Err(LedgerWriteRefusal::Collision);
                }
            }
            for binding in &segment.restore_bindings {
                if !adoptions.insert(&binding.adoption_id) {
                    return Err(LedgerWriteRefusal::Collision);
                }
            }
        }
        Ok(())
    }

    pub(super) fn already_recorded(
        request: &Request,
        catalog: &JournalCatalog,
        line: &[u8],
    ) -> Result<bool, LedgerWriteRefusal> {
        unique_ids(catalog)?;
        let segments = || catalog.days.iter().flat_map(|d| &d.segments);
        let matching_segment = |s: &SegmentCatalog| {
            s.day == request.target.day
                && s.stream == request.target.stream
                && s.segment == request.segment
        };
        match &request.event {
            Event::ArchiveV2 {
                manifest_id, files, ..
            } => {
                let expected = fold_lines(&request.target.day, line);
                let manifest = &expected.segments[0].history[0];
                let existing = segments()
                    .flat_map(|s| s.history.iter().map(move |m| (s, m)))
                    .find(|(_, m)| &m.manifest_id == manifest_id);
                if existing.is_some_and(|(s, m)| !matching_segment(s) || m != manifest) {
                    return Err(LedgerWriteRefusal::Collision);
                }
                for file in files {
                    if let Some(w) = segments()
                        .flat_map(|s| &s.history)
                        .flat_map(|m| &m.files)
                        .find(|w| w.witness_id == file.witness_id)
                        && (existing.is_none() || &w.manifest_id != manifest_id)
                    {
                        return Err(LedgerWriteRefusal::Collision);
                    }
                }
                Ok(existing.is_some())
            }
            Event::RestoreV2 {
                operation_id,
                files,
                ..
            } => {
                for file in files {
                    let (s, w) = segments()
                        .flat_map(|s| {
                            s.history
                                .iter()
                                .flat_map(move |m| m.files.iter().map(move |w| (s, w)))
                        })
                        .find(|(_, w)| w.witness_id == file.witness_id)
                        .ok_or(LedgerWriteRefusal::InvalidInput)?;
                    if !matching_segment(s)
                        || w.file.name != file.name
                        || w.segment_dir.as_deref() != Some(request.target.dir.as_str())
                        || w.layout != Some(request.layout)
                        || w.journal_id.is_none()
                        || w.repository_id.is_none()
                        || w.archive_path.is_none()
                        || w.processing.is_none()
                    {
                        return Err(LedgerWriteRefusal::InvalidInput);
                    }
                }
                let operation = RestoreOperation {
                    operation_id: operation_id.clone(),
                    files: files.clone(),
                };
                match segments()
                    .flat_map(|s| s.restores.iter().map(move |op| (s, op)))
                    .find(|(_, op)| &op.operation_id == operation_id)
                {
                    Some((s, op)) if matching_segment(s) && op == &operation => Ok(true),
                    Some(_) => Err(LedgerWriteRefusal::Collision),
                    None => Ok(false),
                }
            }
            Event::RestoreBindingV1 { binding, .. } => {
                if segments()
                    .filter(|segment| matching_segment(segment))
                    .flat_map(|segment| &segment.history)
                    .filter(|manifest| **manifest == binding.legacy_manifest)
                    .count()
                    != 1
                {
                    return Err(LedgerWriteRefusal::Changed);
                }
                match segments()
                    .flat_map(|segment| segment.restore_bindings.iter().map(move |b| (segment, b)))
                    .find(|(_, current)| current.adoption_id == binding.adoption_id)
                {
                    Some((segment, current)) if matching_segment(segment) && current == binding => {
                        Ok(true)
                    }
                    Some(_) => Err(LedgerWriteRefusal::Collision),
                    None => Ok(false),
                }
            }
            Event::LegacyRestoreV1 {
                adoption_id,
                operation_id,
                files,
                segment_dir,
                layout,
                ..
            } => {
                let (segment, binding) = segments()
                    .flat_map(|segment| segment.restore_bindings.iter().map(move |b| (segment, b)))
                    .find(|(_, binding)| &binding.adoption_id == adoption_id)
                    .ok_or(LedgerWriteRefusal::InvalidInput)?;
                if !matching_segment(segment)
                    || request.expected_binding.as_ref() != Some(binding)
                    || binding.target.dir != *segment_dir
                    || binding.layout != *layout
                    || files.iter().any(|file| {
                        !binding.files.iter().any(|bound| {
                            bound.witness_id == file.witness_id && bound.name == file.name
                        })
                    })
                {
                    return Err(LedgerWriteRefusal::InvalidInput);
                }
                let expected = LegacyRestoreOperation {
                    adoption_id: adoption_id.clone(),
                    operation: RestoreOperation {
                        operation_id: operation_id.clone(),
                        files: files.clone(),
                    },
                };
                match segments()
                    .flat_map(|segment| segment.restores.iter().map(move |op| (segment, op)))
                    .find(|(_, op)| &op.operation_id == operation_id)
                {
                    Some((segment, _))
                        if matching_segment(segment)
                            && segment
                                .legacy_restores
                                .iter()
                                .any(|current| current == &expected) =>
                    {
                        Ok(true)
                    }
                    Some(_) => Err(LedgerWriteRefusal::Collision),
                    None => Ok(false),
                }
            }
            _ => Err(LedgerWriteRefusal::InvalidInput),
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::time::Duration;

    use nix::fcntl::{AtFlags, OFlag, openat};
    use nix::sys::stat::{Mode, fstatat};
    use solstone_core_journal_io::{
        FlatDirectory, create_or_open_flat_directory_bound, errors::ExistingParentLockError,
        locking::acquire_existing_parent_lock_bound,
    };

    use super::plan::{self, AppendMode, Request};
    use super::*;
    use solstone_core_retention::archive_catalog::{codec::fold_lines, read_complete_catalog};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Point {
        AfterBaseline,
        BeforeWrite,
        AfterWrite,
        BeforeConfirmation,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum SyncPoint {
        File,
        Offload,
        Health,
        Root,
    }

    pub(super) trait Hooks {
        fn checkpoint(&mut self, _point: Point) {}
        fn write(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<usize> {
            file.write(bytes)
        }
        fn sync(&mut self, file: &impl AsFd, _point: SyncPoint) -> io::Result<()> {
            nix::unistd::fsync(file).map_err(io::Error::from)
        }
    }

    pub(super) struct NoHooks;
    impl Hooks for NoHooks {
        #[cfg(all(test, feature = "full-tests"))]
        fn sync(&mut self, file: &impl AsFd, point: SyncPoint) -> io::Result<()> {
            let injected = FILE_SYNC_FAULT.with_borrow(|state| {
                state.as_ref().is_some_and(|state| {
                    if point != SyncPoint::File {
                        return false;
                    }
                    state.calls.set(state.calls.get() + 1);
                    !state.consumed.replace(true)
                })
            });
            if injected {
                Err(io::Error::other(
                    "injected owning caller FileSync uncertainty",
                ))
            } else {
                nix::unistd::fsync(file).map_err(io::Error::from)
            }
        }
    }

    #[cfg(all(test, feature = "full-tests"))]
    std::thread_local! {
        static FILE_SYNC_FAULT: std::cell::RefCell<Option<std::rc::Rc<FileSyncState>>> = const {
            std::cell::RefCell::new(None)
        };
    }

    #[cfg(all(test, feature = "full-tests"))]
    struct FileSyncState {
        calls: std::cell::Cell<usize>,
        consumed: std::cell::Cell<bool>,
    }

    /// Thread-bound fixture state only; Drop never disposes any native owner.
    #[cfg(all(test, feature = "full-tests"))]
    pub(super) struct FileSyncUncertainty(std::rc::Rc<FileSyncState>);

    #[cfg(all(test, feature = "full-tests"))]
    impl FileSyncUncertainty {
        pub(super) fn arm() -> io::Result<Self> {
            FILE_SYNC_FAULT.with_borrow_mut(|slot| {
                if slot.is_some() {
                    return Err(io::Error::other("FileSync fixture already armed"));
                }
                let state = std::rc::Rc::new(FileSyncState {
                    calls: std::cell::Cell::new(0),
                    consumed: std::cell::Cell::new(false),
                });
                *slot = Some(std::rc::Rc::clone(&state));
                Ok(Self(state))
            })
        }
        pub(super) fn observations(&self) -> (usize, bool) {
            (self.0.calls.get(), self.0.consumed.get())
        }
    }

    #[cfg(all(test, feature = "full-tests"))]
    impl Drop for FileSyncUncertainty {
        fn drop(&mut self) {
            FILE_SYNC_FAULT.with_borrow_mut(|slot| *slot = None);
        }
    }

    struct BoundLedger<'a> {
        root: &'a Arc<JournalRoot>,
        health: FlatDirectory,
        offload: FlatDirectory,
    }

    fn io_refusal(error: impl ToString) -> LedgerWriteRefusal {
        LedgerWriteRefusal::Io(error.to_string())
    }

    fn native_identifier(value: impl TryInto<u64>) -> Result<u64, LedgerWriteRefusal> {
        value.try_into().map_err(|_| LedgerWriteRefusal::Changed)
    }

    impl<'a> BoundLedger<'a> {
        fn open(root: &'a Arc<JournalRoot>) -> Result<Self, LedgerWriteRefusal> {
            root.revalidate_canonical_binding().map_err(io_refusal)?;
            let health = create_or_open_flat_directory_bound(
                root.as_ref(),
                OsStr::new("health"),
                0o700,
                root.canonical_path(),
            )
            .map_err(io_refusal)?;
            let offload = create_or_open_flat_directory_bound(
                &health,
                OsStr::new("offload"),
                0o700,
                &root.canonical_path().join("health"),
            )
            .map_err(io_refusal)?;
            let bound = Self {
                root,
                health,
                offload,
            };
            bound.revalidate()?;
            Ok(bound)
        }

        fn revalidate(&self) -> Result<(), LedgerWriteRefusal> {
            self.root
                .revalidate_canonical_binding()
                .map_err(io_refusal)?;
            self.health.revalidate().map_err(io_refusal)?;
            self.offload.revalidate().map_err(io_refusal)?;
            let health = FlatDirectory::open(self.root, Path::new("health")).map_err(io_refusal)?;
            let offload =
                FlatDirectory::open(self.root, Path::new("health/offload")).map_err(io_refusal)?;
            if health.identity() != self.health.identity()
                || offload.identity() != self.offload.identity()
            {
                return Err(LedgerWriteRefusal::Changed);
            }
            self.root.revalidate_canonical_binding().map_err(io_refusal)
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Stamp {
        dev: u64,
        ino: u64,
        size: u64,
        links: u64,
        mode: u32,
        mtime: i64,
        mtime_ns: i64,
        ctime: i64,
        ctime_ns: i64,
    }

    impl Stamp {
        fn capture(file: &File) -> Result<Self, LedgerWriteRefusal> {
            let m = file.metadata().map_err(io_refusal)?;
            if !m.is_file() || m.nlink() != 1 {
                return Err(LedgerWriteRefusal::Changed);
            }
            Ok(Self {
                dev: m.dev(),
                ino: m.ino(),
                size: m.len(),
                links: m.nlink(),
                mode: m.mode(),
                mtime: m.mtime(),
                mtime_ns: m.mtime_nsec(),
                ctime: m.ctime(),
                ctime_ns: m.ctime_nsec(),
            })
        }
        fn same_object(&self, other: &Self) -> bool {
            self.dev == other.dev && self.ino == other.ino
        }
        fn check_leaf(
            &self,
            bound: &BoundLedger<'_>,
            name: &OsStr,
        ) -> Result<(), LedgerWriteRefusal> {
            let current =
                fstatat(&bound.offload, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io_refusal)?;
            if native_identifier(current.st_dev)? != self.dev
                || native_identifier(current.st_ino)? != self.ino
                || current.st_nlink != 1
            {
                return Err(LedgerWriteRefusal::Changed);
            }
            Ok(())
        }
    }

    fn observed(
        file: &mut File,
        bound: &BoundLedger<'_>,
        name: &OsStr,
    ) -> Result<(Stamp, Vec<u8>), LedgerWriteRefusal> {
        bound.revalidate()?;
        let before = Stamp::capture(file)?;
        before.check_leaf(bound, name)?;
        file.seek(SeekFrom::Start(0)).map_err(io_refusal)?;
        let limit = before
            .size
            .checked_add(1)
            .ok_or(LedgerWriteRefusal::Changed)?;
        let mut bytes = Vec::new();
        file.take(limit)
            .read_to_end(&mut bytes)
            .map_err(io_refusal)?;
        let after = Stamp::capture(file)?;
        if before != after || u64::try_from(bytes.len()).ok() != Some(before.size) {
            return Err(LedgerWriteRefusal::Changed);
        }
        after.check_leaf(bound, name)?;
        bound.revalidate()?;
        Ok((after, bytes))
    }

    pub(super) fn append_archive(
        root: &Arc<JournalRoot>,
        proof: &OffloadProof,
        time: u64,
        hooks: &mut impl Hooks,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_archive(root, proof, time)?;
        append(root, request, hooks)
    }

    pub(super) fn confirm_archive(
        root: &Arc<JournalRoot>,
        proof: &OffloadProof,
        time: u64,
        hooks: &mut impl Hooks,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::confirm_archive(root, proof, time)?;
        append(root, request, hooks)
    }

    pub(super) fn append_restore(
        root: &Arc<JournalRoot>,
        target: &Target,
        layout: SegmentLayout,
        operation: &RestoreOperation,
        time: u64,
        hooks: &mut impl Hooks,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_restore(root, target, layout, operation, time)?;
        append(root, request, hooks)
    }

    pub(super) fn append_legacy_binding(
        root: &Arc<JournalRoot>,
        binding: &LegacyRestoreBinding,
        time: u64,
        hooks: &mut impl Hooks,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_legacy_binding(root, binding, time)?;
        append(root, request, hooks)
    }

    pub(super) fn append_legacy_restore(
        root: &Arc<JournalRoot>,
        binding: &LegacyRestoreBinding,
        operation: &LegacyRestoreOperation,
        time: u64,
        hooks: &mut impl Hooks,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_legacy_restore(root, binding, operation, time)?;
        append(root, request, hooks)
    }

    fn append(
        root: &Arc<JournalRoot>,
        request: Request,
        hooks: &mut impl Hooks,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let bound = BoundLedger::open(root)?;
        let _lock = acquire_existing_parent_lock_bound(
            &bound.offload,
            OsStr::new("ledger.lock"),
            Duration::ZERO,
            Duration::from_millis(1),
        )
        .map_err(|e| match e {
            ExistingParentLockError::Timeout(_) => LedgerWriteRefusal::Busy,
            other => io_refusal(other),
        })?;
        bound.revalidate()?;
        let name = format!("{}.jsonl", request.target.day);
        let name = OsStr::new(&name);
        let fd = openat(
            &bound.offload,
            name,
            OFlag::O_RDWR
                | OFlag::O_APPEND
                | OFlag::O_CREAT
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC
                | OFlag::O_NONBLOCK,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(io_refusal)?;
        let mut file = File::from(fd);
        let (stamp, bytes) = observed(&mut file, &bound, name)?;
        let baseline =
            read_complete_catalog(root).map_err(LedgerWriteRefusal::HistoryUnavailable)?;
        if baseline.days.iter().find(|d| d.day == request.target.day)
            != Some(&fold_lines(&request.target.day, &bytes))
        {
            return Err(LedgerWriteRefusal::Changed);
        }
        let line = plan::encode(&request.event)?;
        let already = plan::already_recorded(&request, &baseline, &line)?;
        if !already && matches!(request.mode, AppendMode::ConfirmExisting) {
            return Err(LedgerWriteRefusal::Changed);
        }
        let mut expected_bytes = bytes.clone();
        if !already {
            expected_bytes.extend_from_slice(&line);
        }
        let expected_day = fold_lines(&request.target.day, &expected_bytes);
        if expected_day.degraded() {
            return Err(LedgerWriteRefusal::InvalidInput);
        }
        let mut expected = baseline.clone();
        *expected
            .days
            .iter_mut()
            .find(|d| d.day == request.target.day)
            .ok_or(LedgerWriteRefusal::Changed)? = expected_day;
        hooks.checkpoint(Point::AfterBaseline);
        // No WAN or caller effect occurs under this exclusion.
        hooks.checkpoint(Point::BeforeWrite);
        let prewrite = (|| {
            let (current_stamp, current_bytes) = observed(&mut file, &bound, name)?;
            if current_stamp != stamp
                || current_bytes != bytes
                || read_complete_catalog(root).map_err(LedgerWriteRefusal::HistoryUnavailable)?
                    != baseline
            {
                return Err(LedgerWriteRefusal::Changed);
            }
            bound.revalidate()?;
            stamp.check_leaf(&bound, name)
        })();
        if !already {
            prewrite.as_ref().map_err(Clone::clone)?;
        }
        // Existing-record facts and every attempted append survive later failures.
        let mut receipt = LedgerWriteReceipt {
            effect: LedgerWriteEffect::AlreadyRecorded,
            file_sync: LedgerSync::Uncertain,
            offload_sync: LedgerSync::Uncertain,
            health_sync: LedgerSync::Uncertain,
            root_sync: LedgerSync::Uncertain,
            confirmation: LedgerConfirmation::Uncertain,
            failures: Vec::new(),
        };
        if let Err(error) = prewrite {
            receipt
                .failures
                .push(format!("existing record changed: {error:?}"));
        }
        if !already {
            receipt.effect = match hooks.write(&mut file, &line) {
                Ok(written) if written == line.len() => {
                    LedgerWriteEffect::Appended { bytes: written }
                }
                Ok(written) => LedgerWriteEffect::Attempted {
                    written: Some(written),
                },
                Err(error) => {
                    receipt.failures.push(format!("append: {error}"));
                    LedgerWriteEffect::Attempted { written: None }
                }
            };
        }
        hooks.checkpoint(Point::AfterWrite);
        for (point, result) in [
            (SyncPoint::File, hooks.sync(&file, SyncPoint::File)),
            (
                SyncPoint::Offload,
                hooks.sync(&bound.offload, SyncPoint::Offload),
            ),
            (
                SyncPoint::Health,
                hooks.sync(&bound.health, SyncPoint::Health),
            ),
            (SyncPoint::Root, hooks.sync(root.as_ref(), SyncPoint::Root)),
        ] {
            let fact = if let Err(error) = result {
                receipt.failures.push(format!("sync {point:?}: {error}"));
                LedgerSync::Uncertain
            } else {
                LedgerSync::Confirmed
            };
            match point {
                SyncPoint::File => receipt.file_sync = fact,
                SyncPoint::Offload => receipt.offload_sync = fact,
                SyncPoint::Health => receipt.health_sync = fact,
                SyncPoint::Root => receipt.root_sync = fact,
            }
        }
        hooks.checkpoint(Point::BeforeConfirmation);
        let confirmation = (|| {
            let (after_stamp, after_bytes) = observed(&mut file, &bound, name)?;
            if !stamp.same_object(&after_stamp)
                || after_bytes != expected_bytes
                || read_complete_catalog(root).map_err(LedgerWriteRefusal::HistoryUnavailable)?
                    != expected
            {
                return Err(LedgerWriteRefusal::Changed);
            }
            bound.revalidate()
        })();
        match confirmation {
            Ok(())
                if matches!(
                    receipt.effect,
                    LedgerWriteEffect::AlreadyRecorded | LedgerWriteEffect::Appended { .. }
                ) =>
            {
                receipt.confirmation = LedgerConfirmation::Confirmed;
            }
            Ok(()) => {}
            Err(error) => receipt.failures.push(format!("confirmation: {error:?}")),
        }
        Ok(receipt)
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use solstone_core_journal_io::JournalRoot;
    use solstone_core_journal_io::{
        errors::ExistingParentLockError,
        locking::acquire_existing_parent_lock_bound,
        read_windows_observed_file_bounded,
        windows_sync_dir::{
            create_or_open_windows_append_file_bound, create_or_open_windows_flat_directory_bound,
        },
    };
    use solstone_core_retention::archive_catalog::{codec::fold_lines, read_complete_catalog};

    use super::plan::{self, AppendMode, Request};
    use super::*;

    pub(super) fn append_archive(
        root: &Arc<JournalRoot>,
        proof: &OffloadProof,
        time: u64,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_archive(root, proof, time)?;
        append(root, request)
    }

    pub(super) fn confirm_archive(
        root: &Arc<JournalRoot>,
        proof: &OffloadProof,
        time: u64,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::confirm_archive(root, proof, time)?;
        append(root, request)
    }

    pub(super) fn append_restore(
        root: &Arc<JournalRoot>,
        target: &Target,
        layout: SegmentLayout,
        operation: &RestoreOperation,
        time: u64,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_restore(root, target, layout, operation, time)?;
        append(root, request)
    }

    pub(super) fn append_legacy_binding(
        root: &Arc<JournalRoot>,
        binding: &LegacyRestoreBinding,
        time: u64,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_legacy_binding(root, binding, time)?;
        append(root, request)
    }

    pub(super) fn append_legacy_restore(
        root: &Arc<JournalRoot>,
        binding: &LegacyRestoreBinding,
        operation: &LegacyRestoreOperation,
        time: u64,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        let request = plan::append_legacy_restore(root, binding, operation, time)?;
        append(root, request)
    }

    fn append(
        root: &Arc<JournalRoot>,
        request: Request,
    ) -> Result<LedgerWriteReceipt, LedgerWriteRefusal> {
        root.revalidate_canonical_binding().map_err(io_refusal)?;
        let health = create_or_open_windows_flat_directory_bound(
            root.as_ref(),
            OsStr::new("health"),
            root.canonical_path(),
        )
        .map_err(io_refusal)?;
        let offload = create_or_open_windows_flat_directory_bound(
            &health,
            OsStr::new("offload"),
            &root.canonical_path().join("health"),
        )
        .map_err(io_refusal)?;

        let media = solstone_core_journal_io::media::MediaDirectory::open(
            Arc::clone(root),
            Path::new("health/offload"),
        )
        .map_err(io_refusal)?;
        if media.object_identity() != offload.object_identity() {
            return Err(LedgerWriteRefusal::Changed);
        }

        let _lock = acquire_existing_parent_lock_bound(
            &offload,
            OsStr::new("ledger.lock"),
            Duration::ZERO,
            Duration::from_millis(1),
        )
        .map_err(|e| match e {
            ExistingParentLockError::Timeout(_) => LedgerWriteRefusal::Busy,
            other => io_refusal(other),
        })?;

        offload.revalidate_bound().map_err(io_refusal)?;
        let name = format!("{}.jsonl", request.target.day);
        let name = OsStr::new(&name);
        let observation =
            read_windows_observed_file_bounded(&offload, name, usize::MAX).map_err(io_refusal)?;
        let bytes = observation.map(|o| o.bytes).unwrap_or_default();
        offload.revalidate_bound().map_err(io_refusal)?;

        let baseline =
            read_complete_catalog(root).map_err(LedgerWriteRefusal::HistoryUnavailable)?;
        let expected_baseline_day = fold_lines(&request.target.day, &bytes);
        let baseline_day = baseline.days.iter().find(|d| d.day == request.target.day);
        match baseline_day {
            Some(day) if day == &expected_baseline_day => {}
            None if bytes.is_empty() => {}
            _ => return Err(LedgerWriteRefusal::Changed),
        }

        let line = plan::encode(&request.event)?;
        let already = plan::already_recorded(&request, &baseline, &line)?;
        if !already && matches!(request.mode, AppendMode::ConfirmExisting) {
            return Err(LedgerWriteRefusal::Changed);
        }

        let mut expected_bytes = bytes.clone();
        if !already {
            expected_bytes.extend_from_slice(&line);
        }
        let expected_day = fold_lines(&request.target.day, &expected_bytes);
        if expected_day.degraded() {
            return Err(LedgerWriteRefusal::InvalidInput);
        }

        let mut file = create_or_open_windows_append_file_bound(
            &offload,
            name,
            &root.canonical_path().join("health/offload"),
        )
        .map_err(io_refusal)?;

        let prewrite = (|| {
            offload.revalidate_bound().map_err(io_refusal)?;
            media.revalidate().map_err(io_refusal)?;
            let mut current_bytes = Vec::new();
            file.read_to_end(&mut current_bytes).map_err(io_refusal)?;
            // The ledger lock protects the catalog; confirm this leaf through
            // the same writable handle, whose sharing mode excludes replacement.
            if current_bytes != bytes {
                return Err(LedgerWriteRefusal::Changed);
            }
            offload.revalidate_bound().map_err(io_refusal)
        })();
        if !already {
            prewrite.as_ref().map_err(Clone::clone)?;
        }

        let mut receipt = LedgerWriteReceipt {
            effect: LedgerWriteEffect::AlreadyRecorded,
            file_sync: LedgerSync::Uncertain,
            offload_sync: LedgerSync::Uncertain,
            health_sync: LedgerSync::Uncertain,
            root_sync: LedgerSync::Uncertain,
            confirmation: LedgerConfirmation::Uncertain,
            failures: Vec::new(),
        };
        if let Err(error) = prewrite {
            receipt
                .failures
                .push(format!("existing record changed: {error:?}"));
        }

        if !already {
            match file
                .seek(SeekFrom::End(0))
                .and_then(|_| file.write_all(&line))
            {
                Ok(()) => receipt.effect = LedgerWriteEffect::Appended { bytes: line.len() },
                Err(error) => {
                    receipt.effect = LedgerWriteEffect::Attempted { written: None };
                    receipt.failures.push(format!("append: {error}"));
                }
            }
        }
        match file.sync_all() {
            Ok(()) => receipt.file_sync = LedgerSync::Confirmed,
            Err(error) => receipt.failures.push(format!("sync file: {error}")),
        }

        let confirmation = (|| {
            offload.revalidate_bound().map_err(io_refusal)?;
            media.revalidate().map_err(io_refusal)?;
            file.seek(SeekFrom::Start(0)).map_err(io_refusal)?;
            let mut after_bytes = Vec::new();
            file.read_to_end(&mut after_bytes).map_err(io_refusal)?;
            if after_bytes != expected_bytes {
                return Err(LedgerWriteRefusal::Changed);
            }
            media.revalidate().map_err(io_refusal)?;
            offload.revalidate_bound().map_err(io_refusal)
        })();
        match confirmation {
            Ok(())
                if receipt.file_sync == LedgerSync::Confirmed
                    && matches!(
                        receipt.effect,
                        LedgerWriteEffect::AlreadyRecorded | LedgerWriteEffect::Appended { .. }
                    ) =>
            {
                receipt.confirmation = LedgerConfirmation::Confirmed;
            }
            Ok(()) => {}
            Err(error) => receipt.failures.push(format!("confirmation: {error:?}")),
        }

        Ok(receipt)
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod tests;

#[cfg(all(test, unix, feature = "full-tests"))]
#[path = "../run/offer_packaged.rs"]
mod offer_packaged;
