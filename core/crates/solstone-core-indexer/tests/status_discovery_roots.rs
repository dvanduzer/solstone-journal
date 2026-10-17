// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use solstone_core_indexer::discovery::{discover_index_status_files, discover_indexable_files};

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct WorkingDirectory(PathBuf);
impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore working directory");
    }
}

// This binary has one test, so its working-directory change is process-local.
#[test]
fn status_preserves_relative_and_escaped_prefix_results() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Root(std::env::temp_dir().join(format!(
        "solstone-status-discovery-roots-{}-{unique}",
        std::process::id()
    )));
    fs::create_dir(&root.0).unwrap();
    let literal = root.0.join("literal");
    write(&literal);
    let expected = discover_indexable_files(&literal).unwrap();
    assert!(expected.contains_key("config/actions/20260101.jsonl"));
    assert_eq!(discover_index_status_files(&literal).unwrap(), expected);

    {
        let _cwd = WorkingDirectory(std::env::current_dir().unwrap());
        std::env::set_current_dir(&literal).unwrap();
        let original = discover_indexable_files(Path::new("."));
        #[cfg(unix)]
        assert!(
            original.is_err(),
            "the relative-root control must exercise an error"
        );
        assert_same(original, discover_index_status_files(Path::new(".")));
    }

    let parent = root.0.join("parent");
    let escaped = parent.join("root[meta]");
    write(&escaped);
    let expected = discover_indexable_files(&escaped).unwrap();
    assert!(!expected.is_empty());
    assert_eq!(discover_index_status_files(&escaped).unwrap(), expected);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(PathBuf, u32);
        impl Drop for Restore {
            fn drop(&mut self) {
                fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1)).unwrap();
            }
        }
        let saved = fs::metadata(&parent).unwrap().permissions().mode();
        let _restore = Restore(parent.clone(), saved);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o111)).unwrap();
        if fs::read_dir(&parent).is_err() {
            let original = discover_indexable_files(&escaped);
            assert!(
                original.is_err(),
                "the permission control must exercise an error"
            );
            assert_same(original, discover_index_status_files(&escaped));
        }
    }
}

fn write(root: &Path) {
    let directory = root.join("config/actions");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("20260101.jsonl"), "{}\n").unwrap();
}

fn assert_same<T: std::fmt::Debug + PartialEq>(
    original: Result<T, solstone_core_indexer::discovery::DiscoveryError>,
    optimized: Result<T, solstone_core_indexer::discovery::DiscoveryError>,
) {
    match (original, optimized) {
        (Ok(left), Ok(right)) => assert_eq!(left, right),
        (Err(left), Err(right)) => {
            assert_eq!(
                std::mem::discriminant(&left),
                std::mem::discriminant(&right)
            );
            assert_eq!(left.to_string(), right.to_string());
        }
        (left, right) => {
            panic!("different discovery outcomes: original {left:?}, optimized {right:?}")
        }
    }
}
