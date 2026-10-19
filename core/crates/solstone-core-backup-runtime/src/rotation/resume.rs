// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Linux-compiled decoder for recovery-add resume metadata.

use serde::Deserialize;

#[derive(Debug, PartialEq, Eq)]
pub enum ResumeDecodeError {
    InvalidJson,
    WrongSchema,
    WrongKind,
    EmptyPassword,
    PasswordTooLong,
    BadKeyId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryAddRecordedResume {
    schema: String,
    kind: String,
    recovery_password: String,
    #[serde(default)]
    key_id: Option<String>,
}

fn valid_key_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn decode_recovery_add_resume(bytes: &[u8]) -> Result<String, ResumeDecodeError> {
    let payload: RecoveryAddRecordedResume =
        serde_json::from_slice(bytes).map_err(|_| ResumeDecodeError::InvalidJson)?;

    if payload.schema != "solstone.backup-metadata.v1" {
        return Err(ResumeDecodeError::WrongSchema);
    }
    if payload.kind != "recovery-add-recorded" {
        return Err(ResumeDecodeError::WrongKind);
    }
    if payload.recovery_password.is_empty() {
        return Err(ResumeDecodeError::EmptyPassword);
    }
    if payload.recovery_password.len() > 256 {
        return Err(ResumeDecodeError::PasswordTooLong);
    }
    if payload
        .key_id
        .as_deref()
        .is_some_and(|key_id| !valid_key_id(key_id))
    {
        return Err(ResumeDecodeError::BadKeyId);
    }

    Ok(payload.recovery_password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_valid_without_key_id() {
        let json = br#"{"schema":"solstone.backup-metadata.v1","kind":"recovery-add-recorded","recovery_password":"candidate-secret"}"#;
        assert_eq!(
            decode_recovery_add_resume(json).unwrap(),
            "candidate-secret"
        );
    }

    #[test]
    fn test_decode_valid_with_key_id() {
        let valid_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let json = format!(
            r#"{{"schema":"solstone.backup-metadata.v1","kind":"recovery-add-recorded","recovery_password":"candidate-secret","key_id":"{valid_id}"}}"#
        );
        assert_eq!(
            decode_recovery_add_resume(json.as_bytes()).unwrap(),
            "candidate-secret"
        );
    }

    #[test]
    fn test_decode_wrong_schema() {
        let json = br#"{"schema":"solstone.backup-metadata.v2","kind":"recovery-add-recorded","recovery_password":"candidate-secret"}"#;
        assert_eq!(
            decode_recovery_add_resume(json).unwrap_err(),
            ResumeDecodeError::WrongSchema
        );
    }

    #[test]
    fn test_decode_wrong_kind() {
        let json = br#"{"schema":"solstone.backup-metadata.v1","kind":"wrong-kind","recovery_password":"candidate-secret"}"#;
        assert_eq!(
            decode_recovery_add_resume(json).unwrap_err(),
            ResumeDecodeError::WrongKind
        );
    }

    #[test]
    fn test_decode_unknown_fields() {
        let json = br#"{"schema":"solstone.backup-metadata.v1","kind":"recovery-add-recorded","recovery_password":"candidate-secret","extra":"forbidden"}"#;
        assert_eq!(
            decode_recovery_add_resume(json).unwrap_err(),
            ResumeDecodeError::InvalidJson
        );
    }

    #[test]
    fn test_decode_empty_password() {
        let json = br#"{"schema":"solstone.backup-metadata.v1","kind":"recovery-add-recorded","recovery_password":""}"#;
        assert_eq!(
            decode_recovery_add_resume(json).unwrap_err(),
            ResumeDecodeError::EmptyPassword
        );
    }

    #[test]
    fn test_decode_password_too_long() {
        let long_pw = "a".repeat(257);
        let json = format!(
            r#"{{"schema":"solstone.backup-metadata.v1","kind":"recovery-add-recorded","recovery_password":"{long_pw}"}}"#
        );
        assert_eq!(
            decode_recovery_add_resume(json.as_bytes()).unwrap_err(),
            ResumeDecodeError::PasswordTooLong
        );
    }

    #[test]
    fn test_decode_bad_key_id() {
        for bad_id in &[
            "short",
            "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF", // uppercase
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg", // non-hex
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0", // 65 chars
        ] {
            let json = format!(
                r#"{{"schema":"solstone.backup-metadata.v1","kind":"recovery-add-recorded","recovery_password":"candidate-secret","key_id":"{bad_id}"}}"#
            );
            assert_eq!(
                decode_recovery_add_resume(json.as_bytes()).unwrap_err(),
                ResumeDecodeError::BadKeyId,
                "expected BadKeyId for {bad_id}"
            );
        }
    }
}
