// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Pinned input revisions and source-archive digests for mobile tools.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolInputPin {
    pub revision: &'static str,
    pub archive_sha256: &'static str,
}

struct MobileInputPins {
    restic: ToolInputPin,
    rclone: ToolInputPin,
    nvattest: ToolInputPin,
}

static PINS: OnceLock<MobileInputPins> = OnceLock::new();

fn leak_pin(revision: &str, archive_sha256: &str) -> ToolInputPin {
    ToolInputPin {
        revision: Box::leak(revision.to_string().into_boxed_str()),
        archive_sha256: Box::leak(archive_sha256.to_string().into_boxed_str()),
    }
}

fn pin_from(inputs: &serde_json::Value, name: &str) -> ToolInputPin {
    let entry = inputs
        .get(name)
        .unwrap_or_else(|| panic!("inputs.{name} missing from inputs.json"));
    let revision = entry
        .get("revision")
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("inputs.{name}.revision missing"));
    let archive_sha256 = entry
        .get("sha256")
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("inputs.{name}.sha256 missing"));
    leak_pin(revision, archive_sha256)
}

fn get_pins() -> &'static MobileInputPins {
    PINS.get_or_init(|| {
        let text = include_str!("../../../distribution/mobile/inputs.json");
        let val: serde_json::Value =
            serde_json::from_str(text).expect("inputs.json must parse as json");
        let inputs = val
            .get("inputs")
            .expect("inputs key missing from inputs.json");
        MobileInputPins {
            restic: pin_from(inputs, "restic"),
            rclone: pin_from(inputs, "rclone"),
            nvattest: pin_from(inputs, "nvattest"),
        }
    })
}

#[must_use]
pub fn restic_input_pin() -> ToolInputPin {
    get_pins().restic
}

#[must_use]
pub fn rclone_input_pin() -> ToolInputPin {
    get_pins().rclone
}

/// Mobile nvattest source-archive pin. `archive_sha256` is the source archive,
/// not a built `libnvat.so` or `nvattest` binary.
#[must_use]
pub fn nvattest_input_pin() -> ToolInputPin {
    get_pins().nvattest
}

/// Reads `share/ca/ca-bundle.pem` from the repository's pinned linux-aarch64 archive.
///
/// Used for test fixtures to verify against the true compiled pin without embedding 7MB.
pub fn read_test_ca_bundle_pem() -> Result<Vec<u8>, std::io::Error> {
    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let archive_path = repo_root
        .join("core/models/assets/nvattest/libnvat-linux-aarch64-1.2.2-sol.7-archive.tar.xz");
    let file = std::fs::File::open(archive_path)?;
    let mut decompressed = Vec::new();
    lzma_rs::xz_decompress(&mut std::io::BufReader::new(file), &mut decompressed)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut archive = tar::Archive::new(&decompressed[..]);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() == "share/ca/ca-bundle.pem" {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes)?;
            return Ok(bytes);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "ca-bundle.pem not found in libnvat-linux-aarch64 archive",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mobile_inputs_matches_include_str() {
        let text = include_str!("../../../distribution/mobile/inputs.json");
        let val: serde_json::Value = serde_json::from_str(text).unwrap();
        let restic = &val["inputs"]["restic"];
        let rclone = &val["inputs"]["rclone"];
        let nvattest = &val["inputs"]["nvattest"];
        let restic_pin = restic_input_pin();
        let rclone_pin = rclone_input_pin();
        let nvattest_pin = nvattest_input_pin();
        assert_eq!(restic_pin.revision, restic["revision"].as_str().unwrap());
        assert_eq!(
            restic_pin.archive_sha256,
            restic["sha256"].as_str().unwrap()
        );
        assert_eq!(rclone_pin.revision, rclone["revision"].as_str().unwrap());
        assert_eq!(
            rclone_pin.archive_sha256,
            rclone["sha256"].as_str().unwrap()
        );
        assert_eq!(
            nvattest_pin.revision,
            nvattest["revision"].as_str().unwrap()
        );
        assert_eq!(
            nvattest_pin.archive_sha256,
            nvattest["sha256"].as_str().unwrap()
        );
    }
}
