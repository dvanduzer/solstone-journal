// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! SQLite claim index for indexed audio archive members.
//!
//! Stored in `imports/audio-archive-claims.sqlite`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};

use super::AudioArchiveError;
use super::identity::MemberIdentity;
use super::plan::PlannedMember;
use super::record::AudioArchiveRecord;

const CLAIM_DB_FILENAME: &str = "audio-archive-claims.sqlite";
pub const DEFAULT_WRITE_BUSY_TIMEOUT: Duration = Duration::from_millis(2000);

pub fn claim_db_path(journal_root: &Path) -> PathBuf {
    journal_root.join("imports").join(CLAIM_DB_FILENAME)
}

/// Read-only check for members currently claimed by living runs.
///
/// Missing database file returns empty and must not create the file.
/// If `current_import_id` is provided, claims owned by that run are ignored.
pub fn find_held_members(
    journal_root: &Path,
    current_import_id: Option<&str>,
    members: &[PlannedMember],
    busy_timeout: Duration,
) -> Result<HashSet<MemberIdentity>, AudioArchiveError> {
    let path = claim_db_path(journal_root);
    if !path.exists() {
        return Ok(HashSet::new());
    }

    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| map_read_sqlite(&path, error))?;

    conn.busy_timeout(busy_timeout)
        .map_err(|error| map_read_sqlite(&path, error))?;

    let table_exists: bool = match conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='audio_archive_claim';",
        [],
        |_| Ok(true),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => false,
        Err(error) => {
            return Err(map_read_sqlite(&path, error));
        }
    };
    if !table_exists {
        return Ok(HashSet::new());
    }

    let mut stmt = conn
        .prepare(
            "SELECT import_id, sha256 FROM audio_archive_claim WHERE byte_len = ? AND sha256 = ? AND occurrence = ?;",
        )
        .map_err(|error| map_read_sqlite(&path, error))?;

    let mut held = HashSet::new();

    for member in members {
        if member.disposition != super::plan::MemberDisposition::Included {
            continue;
        }

        let mut rows = stmt
            .query(params![
                member.identity.byte_len as i64,
                member.identity.sha256,
                member.identity.occurrence
            ])
            .map_err(|error| map_read_sqlite(&path, error))?;

        if let Some(row) = rows.next().map_err(|error| map_read_sqlite(&path, error))? {
            let row_import_id: String =
                row.get(0).map_err(|error| map_read_sqlite(&path, error))?;
            let sha256: String = row.get(1).map_err(|error| map_read_sqlite(&path, error))?;

            if !is_valid_sha256(&sha256)
                || crate::timestamp::validate_timestamp(&row_import_id).is_err()
            {
                return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                    "invalid claim record: sha256='{sha256}', import_id='{row_import_id}' in {}",
                    path.display()
                )));
            }

            if let Some(curr_id) = current_import_id
                && row_import_id == curr_id
            {
                continue;
            }

            let run_dir = journal_root.join("imports").join(&row_import_id);
            if run_dir.is_dir() {
                let archive_json = run_dir.join("audio-archive.json");
                if archive_json.exists() {
                    let bytes = fs::read(&archive_json).map_err(|e| {
                        AudioArchiveError::ClaimDatabaseCorrupt(format!(
                            "{}: unreadable record: {e}",
                            archive_json.display()
                        ))
                    })?;
                    if serde_json::from_slice::<AudioArchiveRecord>(&bytes).is_err() {
                        return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                            "{}: invalid audio archive record",
                            archive_json.display()
                        )));
                    }
                }
                held.insert(member.identity.clone());
            }
        }
    }

    Ok(held)
}

/// Transactional insertion of claims for all members in a complete tile.
pub fn insert_tile_claims(
    journal_root: &Path,
    import_id: &str,
    member_identities: &[MemberIdentity],
    busy_timeout: Duration,
) -> Result<(), AudioArchiveError> {
    if member_identities.is_empty() {
        return Ok(());
    }

    let path = claim_db_path(journal_root);
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        solstone_core_journal_io::create_directory_with_mode(parent, 0o700)
            .map_err(|e| AudioArchiveError::Io(std::io::Error::other(e.to_string())))?;
    }

    let was_missing = !path.exists();

    let mut conn = Connection::open(&path).map_err(|error| {
        AudioArchiveError::ClaimDatabaseCorrupt(format!("{}: {error}", path.display()))
    })?;

    #[cfg(unix)]
    if was_missing {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }

    conn.busy_timeout(busy_timeout).map_err(map_sqlite)?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audio_archive_claim (
            byte_len INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            occurrence TEXT NOT NULL,
            import_id TEXT NOT NULL,
            PRIMARY KEY (byte_len, sha256, occurrence)
        );",
    )
    .map_err(map_sqlite)?;

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;

    {
        let mut insert_stmt = tx
            .prepare(
                "INSERT INTO audio_archive_claim (byte_len, sha256, occurrence, import_id)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT DO NOTHING;",
            )
            .map_err(map_sqlite)?;

        let mut select_stmt = tx
            .prepare(
                "SELECT import_id FROM audio_archive_claim WHERE byte_len = ? AND sha256 = ? AND occurrence = ?;",
            )
            .map_err(map_sqlite)?;

        let mut update_stmt = tx
            .prepare(
                "UPDATE audio_archive_claim SET import_id = ? WHERE byte_len = ? AND sha256 = ? AND occurrence = ?;",
            )
            .map_err(map_sqlite)?;

        for identity in member_identities {
            insert_stmt
                .execute(params![
                    identity.byte_len as i64,
                    identity.sha256,
                    identity.occurrence,
                    import_id
                ])
                .map_err(map_sqlite)?;

            let existing_id: String = select_stmt
                .query_row(
                    params![
                        identity.byte_len as i64,
                        identity.sha256,
                        identity.occurrence
                    ],
                    |row| row.get(0),
                )
                .map_err(map_sqlite)?;

            if existing_id != import_id {
                if crate::timestamp::validate_timestamp(&existing_id).is_err() {
                    return Err(AudioArchiveError::ClaimDatabaseCorrupt(format!(
                        "claim database contains invalid import_id '{existing_id}'"
                    )));
                }
                let other_dir = journal_root.join("imports").join(&existing_id);
                if other_dir.is_dir() {
                    return Err(AudioArchiveError::MemberHeldByLivingRun {
                        import_id: existing_id,
                    });
                }
                update_stmt
                    .execute(params![
                        import_id,
                        identity.byte_len as i64,
                        identity.sha256,
                        identity.occurrence
                    ])
                    .map_err(map_sqlite)?;
            }
        }
    }

    tx.commit().map_err(map_sqlite)?;
    Ok(())
}

fn map_read_sqlite(path: &Path, error: rusqlite::Error) -> AudioArchiveError {
    match map_sqlite(error) {
        AudioArchiveError::ClaimDatabaseBusy(msg) => AudioArchiveError::ClaimDatabaseBusy(msg),
        other => AudioArchiveError::ClaimDatabaseCorrupt(format!("{}: {other}", path.display())),
    }
}

fn map_sqlite(error: rusqlite::Error) -> AudioArchiveError {
    if let rusqlite::Error::SqliteFailure(err, _) = &error
        && (err.code == rusqlite::ffi::ErrorCode::DatabaseBusy
            || err.code == rusqlite::ffi::ErrorCode::DatabaseLocked)
    {
        return AudioArchiveError::ClaimDatabaseBusy(error.to_string());
    }
    AudioArchiveError::Sqlite(error)
}

fn is_valid_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
