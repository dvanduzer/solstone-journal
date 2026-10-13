// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Shared media mechanics; callers own path admission, scheduling and errors.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

#[derive(Debug, PartialEq, Eq)]
pub enum ParsedRange {
    Valid { start: usize, end: usize },
    Unsatisfiable,
    Ignore,
}

/// Parse the legacy single-range grammar for a nonempty file.
pub fn parse_range(value: &str, total: usize) -> ParsedRange {
    let Some(spec) = value.strip_prefix("bytes=") else {
        return ParsedRange::Ignore;
    };
    if spec.contains(',') {
        return ParsedRange::Ignore;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return ParsedRange::Ignore;
    };
    if start.is_empty() {
        let Ok(length) = end.parse::<usize>() else {
            return ParsedRange::Ignore;
        };
        if length == 0 {
            return ParsedRange::Unsatisfiable;
        }
        let start = total.saturating_sub(length);
        return ParsedRange::Valid {
            start,
            end: total - 1,
        };
    }
    let Ok(start) = start.parse::<usize>() else {
        return ParsedRange::Ignore;
    };
    if start >= total {
        return ParsedRange::Unsatisfiable;
    }
    let end = if end.is_empty() {
        total - 1
    } else {
        let Ok(end) = end.parse::<usize>() else {
            return ParsedRange::Ignore;
        };
        end.min(total - 1)
    };
    if end < start {
        ParsedRange::Unsatisfiable
    } else {
        ParsedRange::Valid { start, end }
    }
}

pub fn media_response(
    status: StatusCode,
    mimetype: &str,
    path: &Path,
    bytes: Vec<u8>,
    content_range: Option<String>,
) -> Response {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mimetype)
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename={filename}"),
        )
        .header(header::CACHE_CONTROL, "public, max-age=300");
    if let Some(content_range) = content_range {
        response = response
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_RANGE, content_range);
    }
    response
        .body(Body::from(bytes))
        .expect("media response builds")
}

pub fn mime_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "flac" => Some("audio/flac"),
        "opus" => Some("audio/opus"),
        "ogg" => Some("audio/ogg"),
        "m4a" => Some("audio/mp4"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "webm" => Some("video/webm"),
        "mp4" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "tiff" => Some("image/tiff"),
        _ => None,
    }
}

/// Read exactly the selected interval from an already open source.
/// The caller observes length before parsing; later growth is not included and
/// truncation within this interval is an UnexpectedEof rather than partial media.
pub fn read_bytes(
    reader: &mut (impl Read + Seek),
    start: usize,
    length: usize,
) -> std::io::Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(start as u64))?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    #[test]
    fn range_grammar_preserves_suffix_open_invalid_and_multiple_ranges() {
        use ParsedRange::{Ignore, Unsatisfiable, Valid};
        for (value, expected) in [
            ("bytes=1-2", Valid { start: 1, end: 2 }),
            ("bytes=1-99", Valid { start: 1, end: 4 }),
            ("bytes=3-", Valid { start: 3, end: 4 }),
            ("bytes=-2", Valid { start: 3, end: 4 }),
            ("bytes=-99", Valid { start: 0, end: 4 }),
            ("bytes=-0", Unsatisfiable),
            ("bytes=5-", Unsatisfiable),
            ("bytes=5-invalid", Unsatisfiable),
            ("bytes=3-1", Unsatisfiable),
            ("bytes=0-1,3-4", Ignore),
            ("Bytes=0-1", Ignore),
            ("items=0-1", Ignore),
            ("bytes= 0-1", Ignore),
            ("bytes=0-invalid", Ignore),
            ("bytes=-", Ignore),
            ("bytes=garbage", Ignore),
            ("bytes=18446744073709551616-", Ignore),
            ("bytes=-18446744073709551616", Ignore),
            ("bytes=0-18446744073709551616", Ignore),
        ] {
            assert_eq!(parse_range(value, 5), expected, "{value}");
        }
    }

    #[test]
    fn reads_only_selected_bytes_and_refuses_a_short_interval() {
        struct Counted {
            file: Cursor<Vec<u8>>,
            read: usize,
        }
        impl Read for Counted {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let length = self.file.read(buf)?;
                self.read += length;
                Ok(length)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.file.seek(position)
            }
        }
        let mut file = Counted {
            file: Cursor::new(vec![7; 4096]),
            read: 0,
        };
        assert_eq!(read_bytes(&mut file, 200, 3).unwrap(), [7; 3]);
        assert_eq!(file.read, 3);
        assert_eq!(file.file.position(), 203);
        assert_eq!(
            read_bytes(&mut file, 4095, 2).unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn seek_and_read_failures_propagate() {
        struct Failing(bool);
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(ErrorKind::PermissionDenied))
            }
        }
        impl Seek for Failing {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
                if self.0 {
                    Ok(0)
                } else {
                    Err(std::io::Error::from(ErrorKind::Unsupported))
                }
            }
        }
        assert_eq!(
            read_bytes(&mut Failing(false), 0, 1).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
        assert_eq!(
            read_bytes(&mut Failing(true), 0, 1).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
    }
}
