// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use super::*;

#[test]
fn streaming_retries_interrupted_and_matches_million_a_vector() {
    struct Generated {
        remaining: usize,
        interrupt: bool,
    }
    impl Read for Generated {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(buffer.len() <= BUFFER_BYTES);
            if self.interrupt {
                self.interrupt = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = self.remaining.min(buffer.len());
            buffer[..count].fill(b'a');
            self.remaining -= count;
            Ok(count)
        }
    }
    let digest = digest_reader(&mut Generated {
        remaining: 1_000_000,
        interrupt: true,
    })
    .unwrap();
    assert_eq!(digest.size, 1_000_000);
    assert_eq!(
        digest.sha256,
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn full_identity_is_lossless_and_strict() {
    let a = MediaIdentity::Windows {
        volume: 42,
        file_id: [0; 16],
    };
    let mut bits = [0; 16];
    bits[0] = 1;
    bits[8] = 1;
    let b = MediaIdentity::Windows {
        volume: 42,
        file_id: bits,
    };
    assert_ne!(a, b); // These IDs collide under xor-of-halves folding.
    let encoded = serde_json::to_string(&b).unwrap();
    assert_eq!(serde_json::from_str::<MediaIdentity>(&encoded).unwrap(), b);
    assert!(
        serde_json::from_str::<MediaIdentity>(
            r#"{"platform":"unix","device":1,"inode":2,"extra":true}"#
        )
        .is_err()
    );
    assert_eq!(checked_length(u64::MAX - 1, 1).unwrap(), u64::MAX);
    assert!(checked_length(u64::MAX, 1).is_err());
}

#[cfg(all(unix, feature = "full-tests"))]
mod native {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct Fixture {
        temp: tempfile::TempDir,
        parent: Arc<MediaDirectory>,
        destination: Arc<MediaDirectory>,
        digest: MediaDigest,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir_all(temp.path().join("day/segment")).unwrap();
            fs::write(temp.path().join("day/input"), b"source media").unwrap();
            let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
            let parent = MediaDirectory::open(root.clone(), Path::new("day")).unwrap();
            let destination = MediaDirectory::open(root, Path::new("day/segment")).unwrap();
            let digest = digest_reader(&mut &b"source media"[..]).unwrap();
            Self {
                temp,
                parent,
                destination,
                digest,
            }
        }
        fn target(&self) -> OwnedMediaTarget {
            MediaStage::create(self.parent.clone())
                .unwrap()
                .create_target()
                .unwrap()
        }
        fn prepared(&self) -> PreparedMedia {
            self.target()
                .copy_from(&self.parent, OsStr::new("input"), &self.digest)
                .unwrap()
        }
    }

    #[test]
    fn publish_confirmed_then_identical_and_conflicting_content() {
        let f = Fixture::new();
        assert!(f.parent.available_bytes().unwrap() > 0);
        let (capacity, fired) = run_with_media_hook(
            MediaPrimitive::CapacityQuery,
            || Err(io::Error::other("query failed")),
            || f.parent.available_bytes(),
        );
        assert!(fired && capacity.is_err());
        let mut prepared = f.prepared();
        let identity = prepared.identity();
        let r = prepared.publish(&f.destination, OsStr::new("audio.flac"));
        assert_eq!(r.effect, PublicationEffect::Published);
        assert_eq!(r.confirmation, Confirmation::Confirmed);
        assert_eq!(r.cleanup, Cleanup::Complete);
        assert_eq!(r.parent_durability, Durability::Synced);
        assert_eq!(r.identity, Some(identity));
        let mut identical = f.prepared();
        assert_ne!(identical.identity(), identity);
        let r = identical.publish(&f.destination, OsStr::new("audio.flac"));
        assert_eq!(r.effect, PublicationEffect::AlreadyPresent);
        assert_eq!(r.identity, Some(identity));
        fs::write(f.temp.path().join("day/segment/conflict"), b"competitor").unwrap();
        let r = f.prepared().publish(&f.destination, OsStr::new("conflict"));
        assert_eq!(r.effect, PublicationEffect::Conflict);
        assert_eq!(
            fs::read(f.temp.path().join("day/segment/conflict")).unwrap(),
            b"competitor"
        );
    }

    #[test]
    fn occupied_kinds_and_absent_or_rebound_destination_are_preserved() {
        let f = Fixture::new();
        fs::create_dir(f.temp.path().join("day/segment/directory")).unwrap();
        symlink("../input", f.temp.path().join("day/segment/symlink")).unwrap();
        for name in ["directory", "symlink"] {
            assert_eq!(
                f.prepared()
                    .publish(&f.destination, OsStr::new(name))
                    .effect,
                PublicationEffect::Conflict
            );
        }
        let mut prepared = f.prepared();
        fs::rename(
            f.temp.path().join("day/segment"),
            f.temp.path().join("day/moved"),
        )
        .unwrap();
        assert_eq!(
            prepared.publish(&f.destination, OsStr::new("new")).effect,
            PublicationEffect::NotPublished
        );
        assert!(!f.temp.path().join("day/segment").exists());
        fs::create_dir(f.temp.path().join("day/segment")).unwrap();
        assert_eq!(
            prepared.publish(&f.destination, OsStr::new("new")).effect,
            PublicationEffect::NotPublished
        );
        assert!(!f.temp.path().join("day/segment/new").exists());
        assert_eq!(prepared.cleanup(), Cleanup::Complete);
    }

    #[test]
    fn mismatch_and_replaced_stage_keep_cleanup_ownership() {
        let f = Fixture::new();
        for digest in [
            MediaDigest {
                size: 1,
                ..f.digest.clone()
            },
            MediaDigest {
                sha256: "0".repeat(64),
                ..f.digest.clone()
            },
        ] {
            let mut failed = f
                .target()
                .copy_from(&f.parent, OsStr::new("input"), &digest)
                .unwrap_err()
                .owned;
            assert_eq!(failed.cleanup_after_helper_quiescence(), Cleanup::Complete);
        }
        let target = f.target();
        let path = target.stage.diagnostic_path();
        fs::rename(&path, path.with_extension("moved")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("payload"), b"replacement").unwrap();
        let mut failed = target
            .copy_from(&f.parent, OsStr::new("input"), &f.digest)
            .unwrap_err()
            .owned;
        assert_eq!(failed.cleanup_after_helper_quiescence(), Cleanup::Refused);
        assert_eq!(fs::read(path.join("payload")).unwrap(), b"replacement");
    }

    #[test]
    fn fresh_descriptor_transport_is_one_shot_and_not_path_retargeted() {
        use std::os::fd::AsRawFd;
        let f = Fixture::new();
        let mut target = f.target();
        let path = target.stage.diagnostic_path().join("payload");
        let transport = target.helper_transport().unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(
            transport.target,
            PathBuf::from(format!(
                "/proc/self/fd/{}",
                transport.descriptor.as_raw_fd()
            ))
        );
        #[cfg(not(target_os = "linux"))]
        assert_eq!(
            transport.target,
            PathBuf::from(format!("/dev/fd/{}", transport.descriptor.as_raw_fd()))
        );
        let owned = transport.descriptor.try_clone_to_owned().unwrap();
        drop(transport);
        assert!(target.helper_transport().is_err());
        fs::rename(&path, path.with_extension("moved")).unwrap();
        fs::write(&path, b"competitor").unwrap();
        let mut file = File::from(owned);
        assert_eq!(file.stream_position().unwrap(), 0);
        assert_eq!(file.metadata().unwrap().len(), 0);
        file.write_all(b"source media").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"competitor");
        assert!(target.prepare_after_successful_helper(&f.digest).is_err());
    }

    #[test]
    fn prepublication_faults_retain_source_and_cleanup_authority() {
        let f = Fixture::new();
        for primitive in [
            MediaPrimitive::PreparationRead,
            MediaPrimitive::PreparationWrite,
            MediaPrimitive::FileSync,
        ] {
            let (result, fired) = run_with_media_hook(
                primitive,
                || Err(io::Error::other("fault")),
                || {
                    f.target()
                        .copy_from(&f.parent, OsStr::new("input"), &f.digest)
                },
            );
            assert!(fired);
            let mut failed = result.unwrap_err().owned;
            assert_eq!(failed.cleanup_after_helper_quiescence(), Cleanup::Complete);
        }
        let mut prepared = f.prepared();
        let (receipt, fired) = run_with_media_hook(
            MediaPrimitive::BeforePublish,
            || Err(io::Error::other("fault")),
            || prepared.publish(&f.destination, OsStr::new("new")),
        );
        assert!(fired);
        assert_eq!(receipt.effect, PublicationEffect::NotPublished);
        assert!(!f.temp.path().join("day/segment/new").exists());
        assert_eq!(prepared.cleanup(), Cleanup::Complete);
    }

    #[test]
    fn postpublication_faults_preserve_effect_and_separate_obligations() {
        for primitive in [
            MediaPrimitive::AfterPublish,
            MediaPrimitive::ParentSync,
            MediaPrimitive::StageUnlink,
            MediaPrimitive::Completion,
        ] {
            let f = Fixture::new();
            let mut prepared = f.prepared();
            let (receipt, fired) = run_with_media_hook(
                primitive,
                || Err(io::Error::other("fault")),
                || prepared.publish(&f.destination, OsStr::new("new")),
            );
            assert!(fired);
            assert_eq!(receipt.effect, PublicationEffect::Published);
            assert_eq!(
                fs::read(f.temp.path().join("day/segment/new")).unwrap(),
                b"source media"
            );
            match primitive {
                MediaPrimitive::ParentSync => {
                    assert_eq!(receipt.parent_durability, Durability::Unproven);
                    assert_eq!(receipt.confirmation, Confirmation::Confirmed);
                }
                MediaPrimitive::StageUnlink => assert_eq!(receipt.cleanup, Cleanup::Pending),
                _ => assert_eq!(receipt.confirmation, Confirmation::Unverified),
            }
            assert_eq!(prepared.cleanup(), Cleanup::Complete);
        }
    }

    #[test]
    fn postpublication_rebinding_never_rolls_back_or_confirms_replacement() {
        for primitive in [MediaPrimitive::AfterPublish, MediaPrimitive::Completion] {
            let f = Fixture::new();
            let mut prepared = f.prepared();
            let segment = f.temp.path().join("day/segment");
            let moved = f.temp.path().join("day/moved");
            let replacement = segment.clone();
            let (receipt, fired) = run_with_media_hook(
                primitive,
                move || {
                    fs::rename(&segment, moved)?;
                    fs::create_dir(&replacement)?;
                    fs::write(replacement.join("new"), b"competitor")
                },
                || prepared.publish(&f.destination, OsStr::new("new")),
            );
            assert!(fired);
            assert_eq!(receipt.effect, PublicationEffect::Published);
            assert_eq!(receipt.confirmation, Confirmation::Unverified);
            assert_eq!(
                fs::read(f.temp.path().join("day/segment/new")).unwrap(),
                b"competitor"
            );
            assert_eq!(
                fs::read(f.temp.path().join("day/moved/new")).unwrap(),
                b"source media"
            );
            assert_eq!(prepared.cleanup(), Cleanup::Complete);
        }
    }

    #[test]
    fn prepared_source_change_and_replaced_leaf_refuse_before_mutation() {
        let f = Fixture::new();
        let mut prepared = f.prepared();
        let path = prepared.target.stage.diagnostic_path().join("payload");
        fs::write(&path, b"changed data").unwrap();
        assert_eq!(
            prepared.publish(&f.destination, OsStr::new("new")).effect,
            PublicationEffect::NotPublished
        );
        fs::rename(&path, path.with_extension("moved")).unwrap();
        fs::write(&path, b"competitor").unwrap();
        assert_eq!(prepared.cleanup(), Cleanup::Refused);
        assert_eq!(fs::read(path).unwrap(), b"competitor");
    }

    #[cfg(target_os = "linux")]
    #[ignore = "requires a separate /dev/shm filesystem"]
    #[test]
    fn cross_volume_refusal_never_copies() {
        let f = Fixture::new();
        let other = tempfile::tempdir_in("/dev/shm").unwrap();
        let root = Arc::new(JournalRoot::open(other.path()).unwrap());
        let destination = MediaDirectory::open(root, Path::new("")).unwrap();
        assert!(
            !destination.identity().same_volume(f.parent.identity()),
            "fixture must span volumes"
        );
        let mut prepared = f.prepared();
        let receipt = prepared.publish(&destination, OsStr::new("new"));
        assert_eq!(receipt.effect, PublicationEffect::NotPublished);
        assert!(!other.path().join("new").exists());
        assert_eq!(prepared.cleanup(), Cleanup::Complete);
    }

    #[test]
    fn modification_at_file_sync_cannot_become_prepared() {
        let f = Fixture::new();
        let target = f.target();
        let path = target.stage.diagnostic_path().join("payload");
        let (result, fired) = run_with_media_hook(
            MediaPrimitive::FileSync,
            move || fs::write(&path, b"changed data"),
            || target.copy_from(&f.parent, OsStr::new("input"), &f.digest),
        );
        assert!(fired);
        let mut failure = result.unwrap_err().owned;
        assert_eq!(failure.cleanup_after_helper_quiescence(), Cleanup::Complete);
    }
}

#[cfg(all(windows, feature = "full-tests"))]
mod windows_native {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::windows::fs::{OpenOptionsExt, symlink_file};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    struct Fixture {
        parent: Arc<MediaDirectory>,
        destination: Arc<MediaDirectory>,
        digest: MediaDigest,
        temp: tempfile::TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = match std::env::var_os("SOLSTONE_MEDIA_TEST_ROOT") {
                Some(path) => tempfile::tempdir_in(path).unwrap(),
                None => tempfile::tempdir().unwrap(),
            };
            fs::create_dir_all(temp.path().join("day/segment")).unwrap();
            fs::write(temp.path().join("day/input"), b"source media").unwrap();
            let root = Arc::new(JournalRoot::open(temp.path()).unwrap());
            let parent = MediaDirectory::open(root.clone(), Path::new("day")).unwrap();
            let destination = MediaDirectory::open(root, Path::new("day/segment")).unwrap();
            let digest = digest_reader(&mut &b"source media"[..]).unwrap();
            Self {
                parent,
                destination,
                digest,
                temp,
            }
        }
        fn target(&self) -> OwnedMediaTarget {
            MediaStage::create(self.parent.clone())
                .unwrap()
                .create_target()
                .unwrap()
        }
        fn prepared(&self) -> PreparedMedia {
            self.target()
                .copy_from(&self.parent, OsStr::new("input"), &self.digest)
                .unwrap()
        }
        fn live(&self, name: &str) -> PathBuf {
            self.temp.path().join("day/segment").join(name)
        }
    }
    fn assert_shared(error: io::Error) {
        assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error:?}");
    }

    #[test]
    fn local_publication_confirms_full_identity_and_distinct_already_present() {
        let f = Fixture::new();
        let target = f.target();
        assert!(target.available_bytes().unwrap() > 0);
        let (capacity, fired) = run_with_media_hook(
            MediaPrimitive::CapacityQuery,
            || Err(io::Error::other("query failed")),
            || target.available_bytes(),
        );
        assert!(fired && capacity.is_err());
        let mut prepared = target
            .copy_from(&f.parent, OsStr::new("input"), &f.digest)
            .unwrap();
        let id = prepared.identity();
        let stage = prepared.target.stage.diagnostic_path();
        let r = prepared.publish(&f.destination, OsStr::new("audio.flac"));
        assert_eq!(r.effect, PublicationEffect::Published);
        assert_eq!(r.confirmation, Confirmation::Confirmed);
        assert_eq!(r.file_durability, Durability::Synced);
        assert_eq!(r.parent_durability, Durability::Unproven);
        assert_eq!(r.cleanup, Cleanup::Complete);
        assert_eq!(r.identity, Some(id));
        assert!(!stage.exists());
        assert_eq!(
            f.destination
                .observe_file(OsStr::new("audio.flac"))
                .unwrap()
                .unwrap()
                .identity,
            id
        );
        eprintln!("WINDOWS_MEDIA_ID source={id:?} final={:?}", r.identity);
        assert_shared(fs::write(f.live("audio.flac"), b"competitor").unwrap_err());
        assert_shared(fs::remove_file(f.live("audio.flac")).unwrap_err());
        let mut identical = f.prepared();
        assert_ne!(identical.identity(), id);
        let r = identical.publish(&f.destination, OsStr::new("audio.flac"));
        assert_eq!(r.effect, PublicationEffect::AlreadyPresent);
        assert_eq!(r.confirmation, Confirmation::Confirmed);
        assert_eq!(r.identity, Some(id));
        assert_eq!(r.cleanup, Cleanup::Complete);
        drop(identical);
        drop(prepared);
        fs::write(f.live("audio.flac"), b"competitor").unwrap(); // inverse write
        fs::remove_file(f.live("audio.flac")).unwrap(); // inverse delete
    }

    #[test]
    fn occupied_plain_directory_and_reparse_preserve_competitors() {
        let f = Fixture::new();
        fs::write(f.live("different"), b"competitor").unwrap();
        fs::create_dir(f.live("directory")).unwrap();
        symlink_file(f.temp.path().join("day/input"), f.live("symlink")).unwrap();
        for name in ["different", "directory", "symlink"] {
            let mut prepared = f.prepared();
            let r = prepared.publish(&f.destination, OsStr::new(name));
            assert_eq!(r.effect, PublicationEffect::Conflict, "{name}: {r:?}");
            assert_eq!(prepared.cleanup(), Cleanup::Complete);
        }
        assert_eq!(fs::read(f.live("different")).unwrap(), b"competitor");
        assert!(
            fs::symlink_metadata(f.live("symlink"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(f.live("directory").is_dir());
    }

    #[test]
    fn retained_source_and_ancestors_exclude_mutation_with_inverse_controls() {
        let f = Fixture::new();
        let input = f.temp.path().join("day/input");
        let input_check = input.clone();
        let (result, fired) = run_with_media_hook(
            MediaPrimitive::PreparationWrite,
            move || {
                assert_shared(fs::write(&input_check, b"competitor").unwrap_err());
                assert_shared(fs::remove_file(&input_check).unwrap_err());
                assert_shared(
                    fs::rename(&input_check, input_check.with_extension("moved")).unwrap_err(),
                );
                Ok(())
            },
            || {
                f.target()
                    .copy_from(&f.parent, OsStr::new("input"), &f.digest)
            },
        );
        assert!(fired);
        let prepared = result.unwrap();
        fs::write(&input, b"competitor").unwrap();
        fs::rename(&input, input.with_extension("moved")).unwrap();
        fs::remove_file(input.with_extension("moved")).unwrap();
        let stage = prepared.target.stage.diagnostic_path();
        let payload = stage.join("payload");
        assert_shared(fs::write(&payload, b"competitor").unwrap_err());
        assert_shared(fs::rename(&payload, payload.with_extension("moved")).unwrap_err());
        for path in [
            f.temp.path().to_path_buf(),
            f.temp.path().join("day"),
            f.live(""),
            stage.clone(),
        ] {
            assert_shared(fs::rename(&path, path.with_extension("moved")).unwrap_err());
        }
        // Drop closes ownership without unlinking. The exact previously refused
        // payload write/rename/delete and stage rename now succeed.
        drop(prepared);
        fs::write(&payload, b"competitor").unwrap();
        fs::rename(&payload, payload.with_extension("moved")).unwrap();
        fs::remove_file(payload.with_extension("moved")).unwrap();
        fs::rename(&stage, stage.with_extension("moved")).unwrap();
        fs::remove_dir(stage.with_extension("moved")).unwrap();
        let stage = MediaStage::create(f.parent.clone()).unwrap();
        let stage_path = stage.diagnostic_path();
        drop(stage);
        fs::rename(&stage_path, stage_path.with_extension("moved")).unwrap();
        fs::remove_dir(stage_path.with_extension("moved")).unwrap();
        let root_path = f.temp.path().to_path_buf();
        drop(f.destination);
        drop(f.parent);
        fs::rename(root_path.join("day/segment"), root_path.join("day/moved")).unwrap();
        fs::rename(root_path.join("day"), root_path.join("moved")).unwrap();
        let renamed = root_path.with_extension("moved");
        fs::rename(&root_path, &renamed).unwrap();
        fs::rename(renamed, root_path).unwrap();
    }

    #[test]
    fn missing_destination_and_wrong_digest_never_publish_or_enable_helper() {
        let f = Fixture::new();
        let root = Arc::new(JournalRoot::open(f.temp.path()).unwrap());
        assert!(MediaDirectory::open(root, Path::new("day/missing")).is_err());
        assert!(!f.temp.path().join("day/missing").exists());
        for digest in [
            MediaDigest {
                size: 1,
                ..f.digest.clone()
            },
            MediaDigest {
                sha256: "0".repeat(64),
                ..f.digest.clone()
            },
        ] {
            let mut failure = f
                .target()
                .copy_from(&f.parent, OsStr::new("input"), &digest)
                .unwrap_err()
                .owned;
            assert_eq!(failure.cleanup_after_helper_quiescence(), Cleanup::Complete);
        }
        let mut target = f.target();
        assert_eq!(
            target.helper_transport().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!target.helper_taken);
        let leaf = target.stage.leaf.as_mut().unwrap();
        assert_eq!(leaf.file.metadata().unwrap().len(), 0);
        assert_eq!(leaf.file.stream_position().unwrap(), 0);
        assert_eq!(target.cleanup_after_helper_quiescence(), Cleanup::Complete);
    }

    #[test]
    fn native_faults_preserve_effect_and_never_dispose_live_renamed_handle() {
        let f = Fixture::new();
        for primitive in [
            MediaPrimitive::PreparationRead,
            MediaPrimitive::PreparationWrite,
            MediaPrimitive::FileSync,
        ] {
            let (result, fired) = run_with_media_hook(
                primitive,
                || Err(io::Error::other("fault")),
                || {
                    f.target()
                        .copy_from(&f.parent, OsStr::new("input"), &f.digest)
                },
            );
            assert!(fired);
            let mut failure = result.unwrap_err().owned;
            assert_eq!(failure.cleanup_after_helper_quiescence(), Cleanup::Complete);
            assert!(!f.live("new").exists());
        }
        for primitive in [
            MediaPrimitive::BeforePublish,
            MediaPrimitive::AfterPublish,
            MediaPrimitive::ParentSync,
            MediaPrimitive::StageRemove,
            MediaPrimitive::Completion,
        ] {
            let mut prepared = f.prepared();
            let name = format!("{primitive:?}");
            let (r, fired) = run_with_media_hook(
                primitive,
                || Err(io::Error::other("fault")),
                || prepared.publish(&f.destination, OsStr::new(&name)),
            );
            assert!(fired);
            if primitive == MediaPrimitive::BeforePublish {
                assert_eq!(r.effect, PublicationEffect::NotPublished);
                assert!(!f.live(&name).exists());
            } else {
                assert_eq!(r.effect, PublicationEffect::Published);
                assert_eq!(fs::read(f.live(&name)).unwrap(), b"source media");
                assert_shared(fs::remove_file(f.live(&name)).unwrap_err());
            }
            assert_eq!(prepared.cleanup(), Cleanup::Complete);
            if primitive != MediaPrimitive::BeforePublish {
                assert_eq!(fs::read(f.live(&name)).unwrap(), b"source media");
            }
        }
    }

    #[test]
    fn cleanup_waits_for_physical_absence_and_preserves_nonempty_stage() {
        let f = Fixture::new();
        let mut target = f.target();
        let stage = target.stage.diagnostic_path();
        let foreign = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(stage.join("payload"))
            .unwrap();
        assert_eq!(target.cleanup_after_helper_quiescence(), Cleanup::Pending);
        assert!(!target.stage.removed);
        drop(foreign);
        assert_eq!(target.cleanup_after_helper_quiescence(), Cleanup::Complete);
        assert!(!stage.exists());
        let mut target = f.target();
        let stage = target.stage.diagnostic_path();
        fs::write(stage.join("foreign"), b"preserve").unwrap();
        assert_eq!(target.cleanup_after_helper_quiescence(), Cleanup::Pending);
        assert_eq!(fs::read(stage.join("foreign")).unwrap(), b"preserve");
        fs::remove_file(stage.join("foreign")).unwrap();
        assert_eq!(target.cleanup_after_helper_quiescence(), Cleanup::Complete);
    }

    #[test]
    #[ignore = "requires SOLSTONE_MEDIA_OTHER_ROOT on a separate Windows volume"]
    fn cross_volume_has_no_copy_fallback() {
        let f = Fixture::new();
        let other =
            tempfile::tempdir_in(std::env::var_os("SOLSTONE_MEDIA_OTHER_ROOT").unwrap()).unwrap();
        let root = Arc::new(JournalRoot::open(other.path()).unwrap());
        let destination = MediaDirectory::open(root, Path::new("")).unwrap();
        assert!(!destination.identity().same_volume(f.parent.identity()));
        let mut prepared = f.prepared();
        let r = prepared.publish(&destination, OsStr::new("new"));
        assert_eq!(r.effect, PublicationEffect::NotPublished);
        assert!(!other.path().join("new").exists());
        assert_eq!(prepared.cleanup(), Cleanup::Complete);
        drop(prepared);
        drop(destination);
    }
}
