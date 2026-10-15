// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(all(test, feature = "full-tests"))]

use super::*;
use crate::JournalRoot;
use crate::activity::{ActivityAdmissionError, ActivityLease};
use std::fs;
use std::os::unix::fs::MetadataExt;

struct Fixture {
    temp: tempfile::TempDir,
    root: Arc<JournalRoot>,
    directory: FlatDirectory,
    lease: Arc<TeardownLease>,
    prior: FileObservation,
    claim: ClaimName,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("android-claim-")
            .tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap())
            .unwrap();
        fs::create_dir(temp.path().join("flat")).unwrap();
        fs::write(temp.path().join("flat/entry"), b"observed").unwrap();
        let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
        let directory = FlatDirectory::open(&root, Path::new("flat")).unwrap();
        let prior = crate::read_observed_file_bounded(&directory, OsStr::new("entry"), 512)
            .unwrap()
            .unwrap();
        let lease = TeardownLease::try_acquire(root.clone()).unwrap();
        let claim = ClaimName::parse("!solstone-claim-00000001-0000000000000001").unwrap();
        Self {
            temp,
            root,
            directory,
            lease,
            prior,
            claim,
        }
    }
    fn original(&self) -> PathBuf {
        self.temp.path().join("flat/entry")
    }
    fn claimed(&self) -> PathBuf {
        self.temp.path().join("flat").join(self.claim.as_str())
    }
    fn remove(&self) -> Result<ClaimRemovalOutcome, ClaimRemovalError> {
        claim_and_remove_android_teardown_observed_bounded(
            &self.lease,
            &self.directory,
            Path::new("flat"),
            OsStr::new("entry"),
            &self.prior,
            &self.claim,
            512,
        )
    }
    fn assert_held(&self) {
        self.lease.revalidate().unwrap();
        assert!(matches!(
            ActivityLease::try_acquire(self.root.clone()),
            Err(ActivityAdmissionError::Busy)
        ));
    }
}
fn identity(path: &Path) -> (u64, u64) {
    let m = fs::symlink_metadata(path).unwrap();
    (m.dev(), m.ino())
}

#[test]
fn generic_android_stays_closed_and_typed_exact_removal_syncs() {
    let f = Fixture::new();
    let before = identity(&f.original());
    let result = claim_and_remove_observed_bounded(
        &f.directory,
        OsStr::new("entry"),
        &f.prior,
        &f.claim,
        512,
    )
    .unwrap();
    assert_eq!(
        result,
        ClaimRemovalOutcome::Unchanged {
            reason: ClaimUnchangedReason::UnsupportedNoReplace {
                primitive: NoReplacePrimitive::UnsupportedUnix
            }
        }
    );
    assert_eq!(identity(&f.original()), before);
    assert_eq!(fs::read(f.original()).unwrap(), b"observed");
    assert!(!f.claimed().exists());
    assert_eq!(f.remove().unwrap(), ClaimRemovalOutcome::Removed);
    assert!(!f.original().exists() && !f.claimed().exists());
    f.assert_held();
}

#[test]
fn different_root_lease_refuses_without_effect() {
    let f = Fixture::new();
    let other = Fixture::new();
    let before = identity(&f.original());
    assert!(
        claim_and_remove_android_teardown_observed_bounded(
            &other.lease,
            &f.directory,
            Path::new("flat"),
            OsStr::new("entry"),
            &f.prior,
            &f.claim,
            512
        )
        .is_err()
    );
    assert_eq!(identity(&f.original()), before);
    assert_eq!(fs::read(f.original()).unwrap(), b"observed");
    assert!(!f.claimed().exists());
    f.assert_held();
    other.assert_held();
}

#[test]
fn replaced_parent_before_claim_preserves_both_directories() {
    let f = Fixture::new();
    let before = identity(&f.original());
    fs::rename(f.temp.path().join("flat"), f.temp.path().join("kept")).unwrap();
    fs::create_dir(f.temp.path().join("flat")).unwrap();
    fs::write(f.original(), b"unrelated").unwrap();
    let replacement = identity(&f.original());
    assert!(f.remove().is_err());
    assert_eq!(identity(&f.original()), replacement);
    assert_eq!(fs::read(f.original()).unwrap(), b"unrelated");
    assert_eq!(identity(&f.temp.path().join("kept/entry")), before);
    f.assert_held();
}

#[test]
fn replaced_parent_after_claim_retains_exact_claim_and_substitute() {
    let f = Fixture::new();
    let before = identity(&f.original());
    let parent = f.temp.path().to_path_buf();
    let (result, fired) = run_with_claim_removal_barrier(
        ClaimRemovalPrimitive::BeforeInspection,
        1,
        move || {
            fs::rename(parent.join("flat"), parent.join("kept")).unwrap();
            fs::create_dir(parent.join("flat")).unwrap();
            fs::write(parent.join("flat/entry"), b"unrelated").unwrap();
        },
        || f.remove(),
    );
    assert!(fired);
    assert!(matches!(
        result,
        Err(ClaimRemovalError::UnlinkFailure { .. })
    ));
    let kept = f.temp.path().join("kept").join(f.claim.as_str());
    assert_eq!(identity(&kept), before);
    assert_eq!(fs::read(kept).unwrap(), b"observed");
    assert_eq!(fs::read(f.original()).unwrap(), b"unrelated");
    f.assert_held();
}

#[test]
fn occupied_claim_preserves_unrelated_entry() {
    let f = Fixture::new();
    fs::write(f.claimed(), b"occupied").unwrap();
    let before = identity(&f.claimed());
    assert_eq!(
        f.remove().unwrap(),
        ClaimRemovalOutcome::Unchanged {
            reason: ClaimUnchangedReason::ClaimNameOccupied
        }
    );
    assert_eq!(identity(&f.claimed()), before);
    assert_eq!(fs::read(f.claimed()).unwrap(), b"occupied");
    assert_eq!(fs::read(f.original()).unwrap(), b"observed");
    f.assert_held();
}

#[test]
fn changed_original_restores_with_same_admitted_policy() {
    let f = Fixture::new();
    let original = f.original();
    let before = identity(&original);
    let (result, fired) = run_with_claim_removal_barrier(
        ClaimRemovalPrimitive::BeforeClaim,
        1,
        move || fs::write(original, b"changed").unwrap(),
        || f.remove(),
    );
    assert!(fired);
    assert_eq!(
        result.unwrap(),
        ClaimRemovalOutcome::IdentityChanged {
            disposition: IdentityChangeDisposition::Restored,
            durability: ClaimDurability::Synced
        }
    );
    assert_eq!(identity(&f.original()), before);
    assert_eq!(fs::read(f.original()).unwrap(), b"changed");
    assert!(!f.claimed().exists());
    f.assert_held();
}

#[test]
fn occupied_original_during_restoration_is_never_overwritten() {
    let f = Fixture::new();
    let original = f.original();
    let occupied = f.original();
    let before = identity(&original);
    let (result, fired) = run_with_two_claim_removal_barriers(
        ClaimRemovalPrimitive::BeforeClaim,
        1,
        move || fs::write(original, b"changed").unwrap(),
        ClaimRemovalPrimitive::BeforeRestore,
        1,
        move || fs::write(occupied, b"occupied").unwrap(),
        || f.remove(),
    );
    assert_eq!(fired, 2);
    assert_eq!(
        result.unwrap(),
        ClaimRemovalOutcome::IdentityChanged {
            disposition: IdentityChangeDisposition::RetainedClaim {
                claim: f.claim.clone()
            },
            durability: ClaimDurability::Synced
        }
    );
    assert_eq!(identity(&f.claimed()), before);
    assert_eq!(fs::read(f.claimed()).unwrap(), b"changed");
    assert_eq!(fs::read(f.original()).unwrap(), b"occupied");
    assert_ne!(identity(&f.original()), before);
    f.assert_held();
}

fn late_substitution(point: ClaimRemovalPrimitive, same_bytes: bool) {
    let f = Fixture::new();
    let claimed = f.claimed();
    let kept = f.temp.path().join("kept-original");
    let kept_check = kept.clone();
    let before = identity(&f.original());
    let modified = fs::metadata(f.original()).unwrap().modified().unwrap();
    let replacement_bytes: &[u8] = if same_bytes {
        b"observed"
    } else {
        b"unrelated"
    };
    let replacement_identity = std::rc::Rc::new(std::cell::Cell::new(None));
    let captured_identity = replacement_identity.clone();
    let (result, fired) = run_with_claim_removal_barrier(
        point,
        1,
        move || {
            fs::rename(&claimed, &kept).unwrap();
            fs::write(&claimed, replacement_bytes).unwrap();
            if same_bytes {
                fs::File::open(&claimed)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(modified))
                    .unwrap();
                assert_eq!(
                    fs::metadata(&claimed).unwrap().modified().unwrap(),
                    modified
                );
            }
            captured_identity.set(Some(identity(&claimed)));
        },
        || f.remove(),
    );
    assert!(fired);
    assert!(matches!(
        result,
        Err(ClaimRemovalError::UnlinkFailure { .. })
    ));
    let replacement = replacement_identity.get().unwrap();
    assert_ne!(replacement, before);
    assert_eq!(fs::read(f.claimed()).unwrap(), replacement_bytes);
    assert_eq!(identity(&f.claimed()), replacement);
    assert_eq!(identity(&kept_check), before);
    assert_eq!(fs::read(kept_check).unwrap(), b"observed");
    f.assert_held();
}
#[test]
fn before_unlink_substitution_preserves_unrelated_bytes_and_identity() {
    late_substitution(ClaimRemovalPrimitive::BeforeUnlink, false);
}
#[test]
fn unlink_substitution_preserves_unrelated_bytes_and_identity() {
    late_substitution(ClaimRemovalPrimitive::Unlink, false);
}

#[test]
fn identical_bytes_and_mtime_at_new_inode_are_retained_before_unlink() {
    late_substitution(ClaimRemovalPrimitive::Unlink, true);
}

#[test]
fn initial_syscall_unavailable_preserves_original_without_fallback() {
    for raw_errno in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP] {
        let f = Fixture::new();
        let before = identity(&f.original());
        let (result, fired) =
            run_with_claim_removal_fault(ClaimRemovalPrimitive::ClaimRename, 1, raw_errno, || {
                f.remove()
            });
        assert!(fired);
        assert_eq!(
            result.unwrap(),
            ClaimRemovalOutcome::Unchanged {
                reason: ClaimUnchangedReason::UnsupportedNoReplace {
                    primitive: NoReplacePrimitive::LinuxRenameAt2
                }
            }
        );
        assert_eq!(identity(&f.original()), before);
        assert_eq!(fs::read(f.original()).unwrap(), b"observed");
        assert!(!f.claimed().exists());
        f.assert_held();
    }
}
#[test]
fn restoration_syscall_unavailable_retains_changed_claim_without_fallback() {
    for raw_errno in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP] {
        let f = Fixture::new();
        let original = f.original();
        let before = identity(&original);
        let claimed = f.claimed();
        let occupied_identity = std::rc::Rc::new(std::cell::Cell::new(None));
        let captured_identity = occupied_identity.clone();
        let (result, fault, barrier) = run_with_claim_removal_fault_and_barrier(
            ClaimRemovalPrimitive::RestoreRename,
            1,
            raw_errno,
            ClaimRemovalPrimitive::BeforeInspection,
            1,
            move || {
                fs::write(claimed, b"changed").unwrap();
                fs::write(&original, b"occupied").unwrap();
                captured_identity.set(Some(identity(&original)));
            },
            || f.remove(),
        );
        assert!(fault && barrier);
        assert!(matches!(
            result,
            Err(ClaimRemovalError::RestoreFailure { .. })
        ));
        assert_eq!(identity(&f.claimed()), before);
        assert_eq!(fs::read(f.claimed()).unwrap(), b"changed");
        assert_eq!(fs::read(f.original()).unwrap(), b"occupied");
        assert_eq!(identity(&f.original()), occupied_identity.get().unwrap());
        f.assert_held();
    }
}
#[test]
fn unlink_failure_retains_original_claim_and_custody() {
    let f = Fixture::new();
    let before = identity(&f.original());
    let (result, fired) =
        run_with_claim_removal_fault(ClaimRemovalPrimitive::Unlink, 1, libc::EIO, || f.remove());
    assert!(fired);
    assert!(matches!(
        result,
        Err(ClaimRemovalError::UnlinkFailure { .. })
    ));
    assert_eq!(identity(&f.claimed()), before);
    assert_eq!(fs::read(f.claimed()).unwrap(), b"observed");
    assert!(!f.original().exists());
    f.assert_held();
}
#[test]
fn directory_sync_failure_reports_actual_removal_uncertainty() {
    let f = Fixture::new();
    let (result, fired) =
        run_with_claim_removal_fault(ClaimRemovalPrimitive::DirectorySync, 1, libc::EIO, || {
            f.remove()
        });
    assert!(fired);
    assert_eq!(
        result.unwrap(),
        ClaimRemovalOutcome::RemovedDurabilityUncertain
    );
    assert!(!f.original().exists() && !f.claimed().exists());
    f.assert_held();
}

#[test]
fn late_restoration_substitution_retains_inspected_and_replacement_identities() {
    for point in [
        ClaimRemovalPrimitive::BeforeRestore,
        ClaimRemovalPrimitive::RestoreRename,
    ] {
        let f = Fixture::new();
        let before = identity(&f.original());
        let inspected = f.claimed();
        let replaced = f.claimed();
        let kept = f.temp.path().join("kept-changed");
        let kept_check = kept.clone();
        let modified = std::rc::Rc::new(std::cell::Cell::new(None));
        let captured_modified = modified.clone();
        let replacement = std::rc::Rc::new(std::cell::Cell::new(None));
        let captured_replacement = replacement.clone();
        let (result, fired) = run_with_two_claim_removal_barriers(
            ClaimRemovalPrimitive::BeforeInspection,
            1,
            move || {
                fs::write(&inspected, b"changed").unwrap();
                captured_modified.set(Some(fs::metadata(inspected).unwrap().modified().unwrap()));
            },
            point,
            1,
            move || {
                fs::rename(&replaced, &kept).unwrap();
                fs::write(&replaced, b"changed").unwrap();
                fs::File::open(&replaced)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(modified.get().unwrap()))
                    .unwrap();
                assert_eq!(
                    fs::metadata(&replaced).unwrap().modified().unwrap(),
                    modified.get().unwrap()
                );
                assert_eq!(
                    fs::metadata(&replaced).unwrap().len(),
                    b"changed".len() as u64
                );
                captured_replacement.set(Some(identity(&replaced)));
            },
            || f.remove(),
        );
        assert_eq!(fired, 2);
        assert!(matches!(
            result,
            Err(ClaimRemovalError::RestoreFailure { .. })
        ));
        assert_eq!(identity(&kept_check), before);
        assert_eq!(fs::read(kept_check).unwrap(), b"changed");
        assert_eq!(identity(&f.claimed()), replacement.get().unwrap());
        assert_ne!(identity(&f.claimed()), before);
        assert_eq!(fs::read(f.claimed()).unwrap(), b"changed");
        assert!(!f.original().exists());
        f.assert_held();
    }
}

#[test]
fn nonregular_restoration_preserves_inspected_identity_and_refuses_late_substitution() {
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    for late in [
        None,
        Some(ClaimRemovalPrimitive::BeforeRestore),
        Some(ClaimRemovalPrimitive::RestoreRename),
    ] {
        let f = Fixture::new();
        let before = identity(&f.original());
        let original = f.original();
        let kept_original = f.temp.path().join("kept-original");
        let kept_original_check = kept_original.clone();
        let inspected = std::rc::Rc::new(std::cell::Cell::new(None));
        let captured_inspected = inspected.clone();
        let inspected_metadata = std::rc::Rc::new(std::cell::Cell::new(None));
        let captured_metadata = inspected_metadata.clone();
        let replacement_metadata = inspected_metadata.clone();
        let replacement = std::rc::Rc::new(std::cell::Cell::new(None));
        let captured_replacement = replacement.clone();
        let claimed = f.claimed();
        let kept_claim = f.temp.path().join("kept-fifo");
        let kept_claim_check = kept_claim.clone();
        let (result, fired) = run_with_two_claim_removal_barriers(
            ClaimRemovalPrimitive::BeforeClaim,
            1,
            move || {
                fs::rename(&original, &kept_original).unwrap();
                nix::unistd::mkfifo(&original, nix::sys::stat::Mode::from_bits_truncate(0o600))
                    .unwrap();
                captured_inspected.set(Some(identity(&original)));
                let metadata = fs::metadata(&original).unwrap();
                captured_metadata.set(Some((metadata.modified().unwrap(), metadata.len())));
            },
            late.unwrap_or(ClaimRemovalPrimitive::BeforeRestore),
            1,
            move || {
                if late.is_some() {
                    fs::rename(&claimed, &kept_claim).unwrap();
                    nix::unistd::mkfifo(&claimed, nix::sys::stat::Mode::from_bits_truncate(0o600))
                        .unwrap();
                    let expected = replacement_metadata.get().unwrap();
                    fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
                        .open(&claimed)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(expected.0))
                        .unwrap();
                    let metadata = fs::metadata(&claimed).unwrap();
                    assert_eq!((metadata.modified().unwrap(), metadata.len()), expected);
                    captured_replacement.set(Some(identity(&claimed)));
                }
            },
            || f.remove(),
        );
        assert_eq!(fired, 2);
        assert_eq!(identity(&kept_original_check), before);
        assert_eq!(fs::read(kept_original_check).unwrap(), b"observed");
        match late {
            None => {
                assert_eq!(
                    result.unwrap(),
                    ClaimRemovalOutcome::IdentityChanged {
                        disposition: IdentityChangeDisposition::Restored,
                        durability: ClaimDurability::Synced
                    }
                );
                assert!(
                    fs::symlink_metadata(f.original())
                        .unwrap()
                        .file_type()
                        .is_fifo()
                );
                assert_eq!(identity(&f.original()), inspected.get().unwrap());
                assert!(!f.claimed().exists());
            }
            Some(_) => {
                assert!(matches!(
                    result,
                    Err(ClaimRemovalError::RestoreFailure { .. })
                ));
                assert_eq!(identity(&kept_claim_check), inspected.get().unwrap());
                assert_eq!(identity(&f.claimed()), replacement.get().unwrap());
                assert_ne!(identity(&f.claimed()), inspected.get().unwrap());
                assert!(
                    fs::symlink_metadata(f.claimed())
                        .unwrap()
                        .file_type()
                        .is_fifo()
                );
                assert!(!f.original().exists());
            }
        }
        f.assert_held();
    }
}
