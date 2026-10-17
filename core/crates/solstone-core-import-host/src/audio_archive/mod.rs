// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Host audio archive implementation: manifest parsing, folder scanning,
//! real Ogg Opus bitstream packet slicing, and importer CLI dispatch.

pub mod dispatch;
mod folder;
mod manifest;
mod slicer;

pub use dispatch::{
    AudioArchiveDispatchOptions, compute_archive_identity, run_audio_archive,
    run_audio_archive_members, run_audio_archive_status, run_audio_archive_with,
};
pub use folder::scan_folder_members;
pub use manifest::read_manifest_members;
pub use slicer::HostAudioArchiveMaterializer;
