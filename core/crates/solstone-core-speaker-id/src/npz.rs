// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! NPY member construction and NPZ verification for transcript embeddings.

use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::Path;

use solstone_core_npy::write_npy;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const MEMBER_NAMES: [&str; 4] = [
    "embeddings.npy",
    "encoder.npy",
    "statement_ids.npy",
    "durations_s.npy",
];

pub(crate) struct NpzMembers {
    pub(crate) embeddings: Vec<u8>,
    pub(crate) encoder: Vec<u8>,
    pub(crate) statement_ids: Vec<u8>,
    pub(crate) durations_s: Vec<u8>,
}

impl NpzMembers {
    pub(crate) fn build(
        embeddings: &[f32],
        rows: usize,
        statement_ids: &[i32],
        durations_s: &[f32],
        encoder: &str,
    ) -> Self {
        let embedding_bytes = f32_bytes(embeddings);
        let statement_id_bytes = i32_bytes(statement_ids);
        let duration_bytes = f32_bytes(durations_s);
        let encoder_bytes = unicode_scalar_bytes(encoder);
        let width = encoder.chars().count();
        let width = usize::max(width, 1);
        Self {
            embeddings: write_npy("<f4", &format!("({rows}, 256)"), &embedding_bytes),
            encoder: write_npy(&format!("<U{width}"), "()", &encoder_bytes),
            statement_ids: write_npy("<i4", &format!("({rows},)"), &statement_id_bytes),
            durations_s: write_npy("<f4", &format!("({rows},)"), &duration_bytes),
        }
    }

    pub(crate) fn archive(&self) -> Result<Vec<u8>, String> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in self.iter() {
            writer
                .start_file(name, options)
                .map_err(|error| error.to_string())?;
            writer.write_all(bytes).map_err(|error| error.to_string())?;
        }
        writer
            .finish()
            .map(Cursor::into_inner)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn verify_at(&self, path: &Path) -> Result<(), String> {
        let file = File::open(path).map_err(|error| error.to_string())?;
        let archive = ZipArchive::new(file).map_err(|error| error.to_string())?;
        let (mut archive, names) =
            crate::zip_names::prove_open_zip(archive).map_err(|error| error.to_string())?;
        if names.len() != MEMBER_NAMES.len() {
            return Err("NPZ has an unexpected member count".to_owned());
        }
        let mut actual_names = names.clone();
        actual_names.sort_unstable();
        let mut expected_names = MEMBER_NAMES.map(str::to_owned).to_vec();
        expected_names.sort_unstable();
        if actual_names != expected_names {
            return Err("NPZ members do not match the transcript sidecar contract".to_owned());
        }
        for (name, expected) in self.iter() {
            let index = names
                .iter()
                .position(|proved| proved == name)
                .ok_or_else(|| format!("NPZ member {name} is missing"))?;
            let mut member = archive.by_index(index).map_err(|error| error.to_string())?;
            let mut actual = Vec::new();
            member
                .read_to_end(&mut actual)
                .map_err(|error| error.to_string())?;
            if actual != expected {
                return Err(format!("NPZ member {name} did not round-trip"));
            }
        }
        Ok(())
    }

    fn iter(&self) -> [(&'static str, &[u8]); 4] {
        [
            (MEMBER_NAMES[0], &self.embeddings),
            (MEMBER_NAMES[1], &self.encoder),
            (MEMBER_NAMES[2], &self.statement_ids),
            (MEMBER_NAMES[3], &self.durations_s),
        ]
    }
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(values));
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn i32_bytes(values: &[i32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(values));
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn unicode_scalar_bytes(value: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(value.chars().count() * std::mem::size_of::<u32>());
    for character in value.chars() {
        bytes.extend_from_slice(&(character as u32).to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip_names::fixtures::{Member, build_zip, colliding, extra_7075};

    #[test]
    fn verify_at_literal_success_and_collision_is_archive_error() {
        let members = NpzMembers::build(&[0.0; 256], 1, &[1], &[1.0], "enc");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.npz");
        std::fs::write(&path, members.archive().unwrap()).unwrap();
        members.verify_at(&path).expect("literal success");

        let bad = dir.path().join("bad.npz");
        std::fs::write(&bad, colliding("embeddings.npy", &members.embeddings)).unwrap();
        let error = members.verify_at(&bad).expect_err("collision");
        assert!(error.contains("collision") || error.contains("unproved") || !error.is_empty());

        let unflagged = dir.path().join("unflagged.npz");
        std::fs::write(
            &unflagged,
            build_zip(&[Member {
                name: b"embeddings.npy".to_vec(),
                flags: 0,
                extra: extra_7075(b"embeddings.npy", b"embeddings.npy", Some(0)),
                data: members.embeddings.clone(),
            }]),
        )
        .unwrap();
        assert!(members.verify_at(&unflagged).is_err());
    }
}
