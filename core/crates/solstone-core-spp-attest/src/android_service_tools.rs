// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! App-owned Android native service-tool admission, manifest verification,
//! and process-local tool registry.
//!
//! Android applications do not use the desktop installed package layout
//! (`bin/`, `lib/`, `share/`). Instead, app-owned service tools are delivered
//! and admitted through `solstone.android-service-tools.v1`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use solstone_core_installed_payload::{
    COMPILED_VERSION, PRODUCT, digest_matches, read_member, resolve_member, validate_relative,
};

pub const ANDROID_SERVICE_TOOLS_SCHEMA: &str = "solstone.android-service-tools.v1";
pub const ANDROID_SERVICE_TOOLS_TARGET: &str = "android-aarch64-app";
pub const ANDROID_RESTIC_MEMBER: &str = "lib/solstone-restic/restic";
pub const ANDROID_RCLONE_MEMBER: &str = "lib/solstone-rclone/rclone";
pub const ANDROID_NVATTEST_MEMBER: &str = "lib/solstone-nvattest/bin/nvattest";
pub const ANDROID_CA_BUNDLE_MEMBER: &str = "lib/solstone-nvattest/share/ca/ca-bundle.pem";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AndroidServiceToolRole {
    Restic,
    Rclone,
}

impl AndroidServiceToolRole {
    #[must_use]
    pub const fn manifest_relative_path(self) -> &'static str {
        match self {
            Self::Restic => ANDROID_RESTIC_MEMBER,
            Self::Rclone => ANDROID_RCLONE_MEMBER,
        }
    }

    #[must_use]
    pub const fn expected_version(self) -> &'static str {
        match self {
            Self::Restic => solstone_core_backup::SUPPORTED_RESTIC_VERSION,
            Self::Rclone => "1.73.1",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidServiceToolsManifest {
    pub schema: String,
    pub product: String,
    pub version: String,
    pub target: String,
    pub files: Vec<AndroidServiceToolFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidServiceToolFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<AndroidServiceToolRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AndroidServiceToolsRefusal {
    #[error("wrong schema: expected {expected}, found {found}")]
    WrongSchema { expected: String, found: String },
    #[error("wrong product: expected {expected}, found {found}")]
    WrongProduct { expected: String, found: String },
    #[error("wrong version: expected {expected}, found {found}")]
    WrongVersion { expected: String, found: String },
    #[error("wrong target: expected {expected}, found {found}")]
    WrongTarget { expected: String, found: String },
    #[error("manifest invalid: {detail}")]
    ManifestInvalid { detail: String },
    #[error("unsafe path: {path}")]
    UnsafePath { path: String },
    #[error("member missing: {path}")]
    MemberMissing { path: String },
    #[error("member changed: {path}")]
    MemberChanged { path: String },
    #[error("member unreadable: {path}")]
    MemberUnreadable { path: String },
    #[error("role version mismatch for {path}: expected {expected}, found {found}")]
    RoleVersionMismatch {
        path: String,
        expected: String,
        found: String,
    },
    #[error("role missing: {role:?}")]
    RoleMissing { role: AndroidServiceToolRole },
    #[error("duplicate role: {role:?}")]
    DuplicateRole { role: AndroidServiceToolRole },
    #[error("load unrepresentable: DT_NEEDED {needed} without {absent} (tag {tag})")]
    LoadUnrepresentable {
        needed: &'static str,
        tag: &'static str,
        absent: &'static str,
    },
}

pub fn verifier_representation_status() -> Result<(), AndroidServiceToolsRefusal> {
    Err(AndroidServiceToolsRefusal::LoadUnrepresentable {
        needed: "libnvat.so",
        tag: "DT_NEEDED",
        absent: "DT_RUNPATH",
    })
}

pub fn render_android_service_tools_manifest(
    files: &[AndroidServiceToolFile],
) -> Result<Vec<u8>, AndroidServiceToolsRefusal> {
    validate_manifest_files(files)?;

    let mut restic_found = false;
    let mut rclone_found = false;
    for file in files {
        match file.role {
            Some(AndroidServiceToolRole::Restic) => restic_found = true,
            Some(AndroidServiceToolRole::Rclone) => rclone_found = true,
            None => {}
        }
    }
    if !restic_found {
        return Err(AndroidServiceToolsRefusal::RoleMissing {
            role: AndroidServiceToolRole::Restic,
        });
    }
    if !rclone_found {
        return Err(AndroidServiceToolsRefusal::RoleMissing {
            role: AndroidServiceToolRole::Rclone,
        });
    }

    let manifest = AndroidServiceToolsManifest {
        schema: ANDROID_SERVICE_TOOLS_SCHEMA.to_owned(),
        product: PRODUCT.to_owned(),
        version: COMPILED_VERSION.to_owned(),
        target: ANDROID_SERVICE_TOOLS_TARGET.to_owned(),
        files: files.to_vec(),
    };
    serde_json::to_vec_pretty(&manifest).map_err(|e| AndroidServiceToolsRefusal::ManifestInvalid {
        detail: e.to_string(),
    })
}

fn validate_manifest_files(
    files: &[AndroidServiceToolFile],
) -> Result<(), AndroidServiceToolsRefusal> {
    let mut previous = "";
    let mut folded = BTreeSet::new();
    let mut seen_roles = BTreeSet::new();

    for file in files {
        validate_relative(&file.path).map_err(|_| AndroidServiceToolsRefusal::UnsafePath {
            path: file.path.clone(),
        })?;
        if file.path.as_str() <= previous {
            return Err(AndroidServiceToolsRefusal::UnsafePath {
                path: file.path.clone(),
            });
        }
        previous = &file.path;
        if !folded.insert(file.path.to_ascii_lowercase()) {
            return Err(AndroidServiceToolsRefusal::UnsafePath {
                path: file.path.clone(),
            });
        }
        if file.sha256.len() != 64 || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AndroidServiceToolsRefusal::ManifestInvalid {
                detail: format!("invalid sha256 for {}", file.path),
            });
        }
        if let Some(role) = file.role {
            if !seen_roles.insert(role) {
                return Err(AndroidServiceToolsRefusal::DuplicateRole { role });
            }
            if file.path != role.manifest_relative_path() {
                return Err(AndroidServiceToolsRefusal::ManifestInvalid {
                    detail: format!(
                        "role {:?} expected at {}, but mapped to {}",
                        role,
                        role.manifest_relative_path(),
                        file.path
                    ),
                });
            }
            if file
                .version
                .as_deref()
                .is_some_and(|ver| ver != role.expected_version())
            {
                return Err(AndroidServiceToolsRefusal::RoleVersionMismatch {
                    path: file.path.clone(),
                    expected: role.expected_version().to_owned(),
                    found: file.version.clone().unwrap_or_default(),
                });
            }
        }
    }
    Ok(())
}

fn map_installed_refusal(
    relative: &str,
    refusal: &solstone_core_installed_payload::InstalledPayloadRefusal,
) -> AndroidServiceToolsRefusal {
    if refusal.code == solstone_core_installed_payload::code::MEMBER_MISSING {
        AndroidServiceToolsRefusal::MemberMissing {
            path: relative.to_owned(),
        }
    } else if refusal.code == solstone_core_installed_payload::code::MEMBER_CHANGED {
        AndroidServiceToolsRefusal::MemberChanged {
            path: relative.to_owned(),
        }
    } else if refusal.code == solstone_core_installed_payload::code::UNSAFE_PATH {
        AndroidServiceToolsRefusal::UnsafePath {
            path: relative.to_owned(),
        }
    } else {
        AndroidServiceToolsRefusal::MemberUnreadable {
            path: relative.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AndroidServiceToolsAdmitted {
    root: PathBuf,
    manifest: AndroidServiceToolsManifest,
    resolved_tools: BTreeMap<AndroidServiceToolRole, PathBuf>,
}

impl AndroidServiceToolsAdmitted {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn manifest(&self) -> &AndroidServiceToolsManifest {
        &self.manifest
    }

    #[must_use]
    pub fn tool_path(&self, role: AndroidServiceToolRole) -> Option<&Path> {
        self.resolved_tools.get(&role).map(PathBuf::as_path)
    }

    pub fn file_path(&self, relative_path: &str) -> Result<PathBuf, AndroidServiceToolsRefusal> {
        resolve_member(&self.root, relative_path)
            .map_err(|e| map_installed_refusal(relative_path, &e))
    }

    pub fn revalidate(&self) -> Result<(), AndroidServiceToolsRefusal> {
        for file in &self.manifest.files {
            let path = self.file_path(&file.path)?;
            let bytes = read_member(&path, &file.path)
                .map_err(|e| map_installed_refusal(&file.path, &e))?;
            if !digest_matches(&bytes, file.bytes, &file.sha256) {
                return Err(AndroidServiceToolsRefusal::MemberChanged {
                    path: file.path.clone(),
                });
            }
        }
        Ok(())
    }
}

pub fn admit_android_service_tools(
    root: &Path,
    manifest_bytes: &[u8],
) -> Result<AndroidServiceToolsAdmitted, AndroidServiceToolsRefusal> {
    let manifest: AndroidServiceToolsManifest =
        serde_json::from_slice(manifest_bytes).map_err(|e| {
            AndroidServiceToolsRefusal::ManifestInvalid {
                detail: e.to_string(),
            }
        })?;

    if manifest.schema != ANDROID_SERVICE_TOOLS_SCHEMA {
        return Err(AndroidServiceToolsRefusal::WrongSchema {
            expected: ANDROID_SERVICE_TOOLS_SCHEMA.to_owned(),
            found: manifest.schema,
        });
    }
    if manifest.product != PRODUCT {
        return Err(AndroidServiceToolsRefusal::WrongProduct {
            expected: PRODUCT.to_owned(),
            found: manifest.product,
        });
    }
    if manifest.version != COMPILED_VERSION {
        return Err(AndroidServiceToolsRefusal::WrongVersion {
            expected: COMPILED_VERSION.to_owned(),
            found: manifest.version,
        });
    }
    if manifest.target != ANDROID_SERVICE_TOOLS_TARGET {
        return Err(AndroidServiceToolsRefusal::WrongTarget {
            expected: ANDROID_SERVICE_TOOLS_TARGET.to_owned(),
            found: manifest.target,
        });
    }

    validate_manifest_files(&manifest.files)?;

    let mut resolved_tools = BTreeMap::new();
    for file in &manifest.files {
        let full_path =
            resolve_member(root, &file.path).map_err(|e| map_installed_refusal(&file.path, &e))?;
        let bytes = read_member(&full_path, &file.path)
            .map_err(|e| map_installed_refusal(&file.path, &e))?;
        if !digest_matches(&bytes, file.bytes, &file.sha256) {
            return Err(AndroidServiceToolsRefusal::MemberChanged {
                path: file.path.clone(),
            });
        }
        if let Some(role) = file.role {
            resolved_tools.insert(role, full_path);
        }
    }

    if !resolved_tools.contains_key(&AndroidServiceToolRole::Restic) {
        return Err(AndroidServiceToolsRefusal::RoleMissing {
            role: AndroidServiceToolRole::Restic,
        });
    }
    if !resolved_tools.contains_key(&AndroidServiceToolRole::Rclone) {
        return Err(AndroidServiceToolsRefusal::RoleMissing {
            role: AndroidServiceToolRole::Rclone,
        });
    }

    Ok(AndroidServiceToolsAdmitted {
        root: root.to_path_buf(),
        manifest,
        resolved_tools,
    })
}

static INSTALLED_TOOLS: OnceLock<AndroidServiceToolsAdmitted> = OnceLock::new();

pub fn install_admitted_android_service_tools(
    tools: AndroidServiceToolsAdmitted,
) -> Result<(), AndroidServiceToolsRefusal> {
    INSTALLED_TOOLS
        .set(tools)
        .map_err(|_| AndroidServiceToolsRefusal::ManifestInvalid {
            detail: "android service tools already installed in process".to_owned(),
        })
}

#[must_use]
pub fn get_admitted_android_service_tools() -> Option<&'static AndroidServiceToolsAdmitted> {
    INSTALLED_TOOLS.get()
}

#[must_use]
pub fn get_admitted_android_service_tool(role: AndroidServiceToolRole) -> Option<PathBuf> {
    INSTALLED_TOOLS
        .get()
        .and_then(|tools| tools.tool_path(role).map(Path::to_path_buf))
}

pub fn revalidate_installed_android_service_tools() -> Result<(), AndroidServiceToolsRefusal> {
    if let Some(tools) = INSTALLED_TOOLS.get() {
        tools.revalidate()
    } else {
        Err(AndroidServiceToolsRefusal::MemberMissing {
            path: "android service tools not installed in process".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solstone_core_installed_payload::sha256_hex;
    use std::fs;
    use tempfile::tempdir;

    fn make_test_files(dir: &Path) -> (Vec<AndroidServiceToolFile>, Vec<u8>) {
        let restic_path = dir.join(ANDROID_RESTIC_MEMBER);
        let rclone_path = dir.join(ANDROID_RCLONE_MEMBER);
        fs::create_dir_all(restic_path.parent().unwrap()).unwrap();
        fs::create_dir_all(rclone_path.parent().unwrap()).unwrap();

        let restic_bytes = b"\x7fELFfake-restic-binary";
        let rclone_bytes = b"\x7fELFfake-rclone-binary";
        fs::write(&restic_path, restic_bytes).unwrap();
        fs::write(&rclone_path, rclone_bytes).unwrap();

        let files = vec![
            AndroidServiceToolFile {
                path: ANDROID_RCLONE_MEMBER.to_owned(),
                bytes: rclone_bytes.len() as u64,
                sha256: sha256_hex(rclone_bytes),
                role: Some(AndroidServiceToolRole::Rclone),
                version: Some("1.73.1".to_owned()),
            },
            AndroidServiceToolFile {
                path: ANDROID_RESTIC_MEMBER.to_owned(),
                bytes: restic_bytes.len() as u64,
                sha256: sha256_hex(restic_bytes),
                role: Some(AndroidServiceToolRole::Restic),
                version: Some(solstone_core_backup::SUPPORTED_RESTIC_VERSION.to_owned()),
            },
        ];
        let manifest_bytes = render_android_service_tools_manifest(&files).unwrap();
        (files, manifest_bytes)
    }

    #[test]
    fn verifier_representation_status_fails_closed_with_exact_strings() {
        let err = verifier_representation_status().unwrap_err();
        match err {
            AndroidServiceToolsRefusal::LoadUnrepresentable {
                needed,
                tag,
                absent,
            } => {
                assert_eq!(needed, "libnvat.so");
                assert_eq!(tag, "DT_NEEDED");
                assert_eq!(absent, "DT_RUNPATH");
            }
            other => panic!("unexpected refusal: {other:?}"),
        }
    }

    #[test]
    fn manifest_render_and_admit_roundtrip() {
        let temp = tempdir().unwrap();
        let (_files, manifest_bytes) = make_test_files(temp.path());

        let admitted = admit_android_service_tools(temp.path(), &manifest_bytes).unwrap();
        assert_eq!(admitted.root(), temp.path());
        assert_eq!(
            admitted.tool_path(AndroidServiceToolRole::Restic).unwrap(),
            temp.path().join(ANDROID_RESTIC_MEMBER)
        );
        assert_eq!(
            admitted.tool_path(AndroidServiceToolRole::Rclone).unwrap(),
            temp.path().join(ANDROID_RCLONE_MEMBER)
        );
        admitted.revalidate().unwrap();
    }

    #[test]
    fn admit_refuses_wrong_schema_product_version_target() {
        let temp = tempdir().unwrap();
        let (_files, manifest_bytes) = make_test_files(temp.path());
        let mut value: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();

        // Wrong schema
        value["schema"] = "solstone.other.v1".into();
        let err = admit_android_service_tools(temp.path(), &serde_json::to_vec(&value).unwrap())
            .unwrap_err();
        assert!(matches!(
            err,
            AndroidServiceToolsRefusal::WrongSchema { .. }
        ));

        // Wrong product
        value["schema"] = ANDROID_SERVICE_TOOLS_SCHEMA.into();
        value["product"] = "other-product".into();
        let err = admit_android_service_tools(temp.path(), &serde_json::to_vec(&value).unwrap())
            .unwrap_err();
        assert!(matches!(
            err,
            AndroidServiceToolsRefusal::WrongProduct { .. }
        ));

        // Wrong version
        value["product"] = PRODUCT.into();
        value["version"] = "99.99.99".into();
        let err = admit_android_service_tools(temp.path(), &serde_json::to_vec(&value).unwrap())
            .unwrap_err();
        assert!(matches!(
            err,
            AndroidServiceToolsRefusal::WrongVersion { .. }
        ));

        // Wrong target
        value["version"] = COMPILED_VERSION.into();
        value["target"] = "linux-x86_64".into();
        let err = admit_android_service_tools(temp.path(), &serde_json::to_vec(&value).unwrap())
            .unwrap_err();
        assert!(matches!(
            err,
            AndroidServiceToolsRefusal::WrongTarget { .. }
        ));
    }

    #[test]
    fn admit_refuses_missing_or_changed_member() {
        let temp = tempdir().unwrap();
        let (_files, manifest_bytes) = make_test_files(temp.path());

        // Tamper with restic
        let restic_path = temp.path().join(ANDROID_RESTIC_MEMBER);
        fs::write(&restic_path, b"tampered-content").unwrap();

        let err = admit_android_service_tools(temp.path(), &manifest_bytes).unwrap_err();
        assert_eq!(
            err,
            AndroidServiceToolsRefusal::MemberChanged {
                path: ANDROID_RESTIC_MEMBER.to_owned()
            }
        );

        // Delete restic
        fs::remove_file(&restic_path).unwrap();
        let err = admit_android_service_tools(temp.path(), &manifest_bytes).unwrap_err();
        assert_eq!(
            err,
            AndroidServiceToolsRefusal::MemberMissing {
                path: ANDROID_RESTIC_MEMBER.to_owned()
            }
        );
    }

    #[test]
    fn revalidate_catches_tampering_after_admission() {
        let temp = tempdir().unwrap();
        let (_files, manifest_bytes) = make_test_files(temp.path());
        let admitted = admit_android_service_tools(temp.path(), &manifest_bytes).unwrap();
        admitted.revalidate().unwrap();

        let rclone_path = temp.path().join(ANDROID_RCLONE_MEMBER);
        fs::write(&rclone_path, b"tampered-after-admit").unwrap();
        assert_eq!(
            admitted.revalidate().unwrap_err(),
            AndroidServiceToolsRefusal::MemberChanged {
                path: ANDROID_RCLONE_MEMBER.to_owned()
            }
        );
    }

    #[test]
    fn manifest_validation_refuses_missing_required_roles() {
        let files = vec![AndroidServiceToolFile {
            path: ANDROID_RESTIC_MEMBER.to_owned(),
            bytes: 10,
            sha256: "0".repeat(64),
            role: Some(AndroidServiceToolRole::Restic),
            version: Some(solstone_core_backup::SUPPORTED_RESTIC_VERSION.to_owned()),
        }];
        let err = render_android_service_tools_manifest(&files).unwrap_err();
        assert_eq!(
            err,
            AndroidServiceToolsRefusal::RoleMissing {
                role: AndroidServiceToolRole::Rclone
            }
        );
    }

    #[test]
    fn manifest_validation_refuses_role_version_mismatch() {
        let files = vec![
            AndroidServiceToolFile {
                path: ANDROID_RCLONE_MEMBER.to_owned(),
                bytes: 10,
                sha256: "0".repeat(64),
                role: Some(AndroidServiceToolRole::Rclone),
                version: Some("0.0.1".to_owned()),
            },
            AndroidServiceToolFile {
                path: ANDROID_RESTIC_MEMBER.to_owned(),
                bytes: 10,
                sha256: "0".repeat(64),
                role: Some(AndroidServiceToolRole::Restic),
                version: Some(solstone_core_backup::SUPPORTED_RESTIC_VERSION.to_owned()),
            },
        ];
        let err = render_android_service_tools_manifest(&files).unwrap_err();
        assert!(matches!(
            err,
            AndroidServiceToolsRefusal::RoleVersionMismatch { .. }
        ));
    }
}
