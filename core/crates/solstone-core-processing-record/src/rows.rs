// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Streaming reads of a JSONL file's first two nonblank rows.
//!
//! Each row is judged on its own bytes, read whole and in constant memory: keys
//! are compared and values skipped, so neither a long row nor a character cut by
//! the end of what the caller let through can spoil another row.

use std::fmt;
use std::io::{self, BufRead, Read};

use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

/// The header key a processing record lives under.
const RECORD_KEY: &str = "_solstone_processing";

/// What one of a JSONL stream's first two nonblank rows says.
#[derive(Clone, Debug, PartialEq)]
pub enum LeadingRow {
    /// Exactly one JSON object on its own line.
    Object {
        /// Whether the probed key is one of its top-level keys.
        has_key: bool,
        /// Its `_solstone_processing` value, when that is an object.
        record: Option<Value>,
    },
    /// Not one JSON object, including a row cut off where the reader ends.
    Unparsed,
    /// Bytes that are not UTF-8. A character cut off where the reader ends is not
    /// this: that row is only `Unparsed`.
    NotUtf8,
}

/// Read the first two nonblank rows from `reader`, probing each for `key`.
///
/// Reads at most two rows and stops. Bound what is read by bounding `reader`.
/// Errors are the reader's own; malformed rows are reported per row instead.
pub fn read_leading_rows(
    mut reader: impl BufRead,
    key: Option<&str>,
) -> io::Result<Vec<LeadingRow>> {
    let mut rows = Vec::with_capacity(2);
    while rows.len() < 2 && skip_blank_lines(&mut reader)? {
        let mut line = LineReader {
            inner: &mut reader,
            ended: false,
            utf8: Utf8::default(),
            not_utf8: false,
        };
        let parsed = read_row(&mut line, key);
        io::copy(&mut line, &mut io::sink())?;
        rows.push(if line.not_utf8 {
            LeadingRow::NotUtf8
        } else {
            match parsed {
                Ok(Some(row)) => row,
                Ok(None) => LeadingRow::Unparsed,
                Err(error) => return Err(error),
            }
        });
    }
    Ok(rows)
}

/// Parse one line as exactly one object; `None` when it is not one.
fn read_row(line: &mut impl Read, key: Option<&str>) -> io::Result<Option<LeadingRow>> {
    let mut deserializer = serde_json::Deserializer::from_reader(line);
    let row = RowProbe { key }
        .deserialize(&mut deserializer)
        .and_then(|row| deserializer.end().map(|()| row));
    match row {
        Ok(row) => Ok(Some(row)),
        Err(error) if error.is_io() => Err(error.into()),
        Err(_) => Ok(None),
    }
}

/// Consume JSON whitespace, including blank lines; false at end of input.
fn skip_blank_lines(reader: &mut impl BufRead) -> io::Result<bool> {
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(false);
        }
        let blank = buffer
            .iter()
            .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            .count();
        let more = blank < buffer.len();
        reader.consume(blank);
        if more {
            return Ok(true);
        }
    }
}

/// One line of the underlying reader, ending before its newline (which it
/// consumes), checking as it goes that the line is UTF-8.
struct LineReader<'a, R> {
    inner: &'a mut R,
    ended: bool,
    utf8: Utf8,
    not_utf8: bool,
}

impl<R: BufRead> Read for LineReader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.ended || out.is_empty() {
            return Ok(0);
        }
        let buffer = self.inner.fill_buf()?;
        // Search only what this call can return: the caller often reads one
        // byte at a time, and scanning the whole buffer each time is quadratic.
        let window = &buffer[..buffer.len().min(out.len())];
        let (take, newline) = match window.iter().position(|byte| *byte == b'\n') {
            Some(at) => (at, true),
            None => (window.len(), false),
        };
        let at_end = buffer.is_empty();
        out[..take].copy_from_slice(&buffer[..take]);
        self.inner.consume(take + usize::from(newline));
        for byte in &out[..take] {
            self.not_utf8 |= !self.utf8.accept(*byte);
        }
        // A character still open at a newline is broken; at the end of the
        // input it was only cut off, and the row cannot parse anyway.
        self.not_utf8 |= newline && self.utf8.open();
        self.ended = newline || at_end;
        Ok(take)
    }
}

/// Incremental UTF-8 validation, one byte at a time.
#[derive(Default)]
struct Utf8 {
    /// Continuation bytes still owed by the current character.
    owed: u8,
    /// The range the next continuation byte must fall in.
    low: u8,
    high: u8,
}

impl Utf8 {
    fn accept(&mut self, byte: u8) -> bool {
        if self.owed > 0 {
            if !(self.low..=self.high).contains(&byte) {
                self.owed = 0;
                return false;
            }
            self.owed -= 1;
            (self.low, self.high) = (0x80, 0xBF);
            return true;
        }
        let (owed, low, high) = match byte {
            0x00..=0x7F => return true,
            0xC2..=0xDF => (1, 0x80, 0xBF),
            0xE0 => (2, 0xA0, 0xBF),
            0xE1..=0xEC | 0xEE..=0xEF => (2, 0x80, 0xBF),
            0xED => (2, 0x80, 0x9F),
            0xF0 => (3, 0x90, 0xBF),
            0xF1..=0xF3 => (3, 0x80, 0xBF),
            0xF4 => (3, 0x80, 0x8F),
            _ => return false,
        };
        (self.owed, self.low, self.high) = (owed, low, high);
        true
    }

    fn open(&self) -> bool {
        self.owed > 0
    }
}

/// Visits a row's top-level keys, keeping only the processing record.
struct RowProbe<'k> {
    key: Option<&'k str>,
}

impl<'de> DeserializeSeed<'de> for RowProbe<'_> {
    type Value = LeadingRow;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<LeadingRow, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for RowProbe<'_> {
    type Value = LeadingRow;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSONL row object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<LeadingRow, A::Error> {
        let mut has_key = false;
        let mut record = None;
        while let Some(seen) = map.next_key_seed(KeyProbe { key: self.key })? {
            has_key |= seen.probed;
            if seen.record {
                // Last one wins, as when the row is parsed whole.
                record = Some(map.next_value::<Value>()?).filter(Value::is_object);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(LeadingRow::Object { has_key, record })
    }
}

struct SeenKey {
    probed: bool,
    record: bool,
}

/// Classifies one key without keeping it.
struct KeyProbe<'k> {
    key: Option<&'k str>,
}

impl<'de> DeserializeSeed<'de> for KeyProbe<'_> {
    type Value = SeenKey;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<SeenKey, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for KeyProbe<'_> {
    type Value = SeenKey;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object key")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<SeenKey, E> {
        Ok(SeenKey {
            probed: self.key == Some(key),
            record: key == RECORD_KEY,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{LeadingRow, Utf8, read_leading_rows};

    fn rows(bytes: &[u8]) -> Vec<LeadingRow> {
        read_leading_rows(bytes, Some("start")).expect("a slice cannot fail to read")
    }

    fn object(has_key: bool) -> LeadingRow {
        LeadingRow::Object {
            has_key,
            record: None,
        }
    }

    #[test]
    fn rows_are_judged_one_line_at_a_time() {
        assert_eq!(rows(b""), []);
        assert_eq!(rows(b"\n \r\n\t\n"), []);
        assert_eq!(
            rows(b"\n{}\n\n{\"start\":0}\n{\"x\":1}\n"),
            [object(false), object(true)]
        );
        assert_eq!(
            rows(b"not-json\n{\"start\":0}"),
            [LeadingRow::Unparsed, object(true)]
        );
        assert_eq!(
            rows(b"{\"start\":0} x\n{\"start\":0}{}\n"),
            [LeadingRow::Unparsed, LeadingRow::Unparsed]
        );
        assert_eq!(
            rows(b"[\"start\"]\n{\"a\":{\"start\":0}}\n"),
            [LeadingRow::Unparsed, object(false)]
        );
        // A string broken by a newline does not swallow the next row.
        assert_eq!(
            rows(b"{\"a\":\"cut\n{\"start\":0}\n"),
            [LeadingRow::Unparsed, object(true)]
        );
        assert_eq!(rows(b"{\"start\":0,\"a\":\"cut"), [LeadingRow::Unparsed]);
    }

    #[test]
    fn the_record_is_kept_only_when_it_is_an_object() {
        let row = |text: &str| rows(text.as_bytes()).remove(0);
        assert_eq!(
            row("{\"_solstone_processing\":{\"state\":\"empty\"},\"start\":0}\n"),
            LeadingRow::Object {
                has_key: true,
                record: Some(json!({"state": "empty"})),
            }
        );
        assert_eq!(row("{\"_solstone_processing\":\"empty\"}\n"), object(false));
        assert_eq!(
            row("{\"_solstone_processing\":{},\"_solstone_processing\":1}\n"),
            object(false)
        );
    }

    #[test]
    fn broken_utf8_is_named_but_a_character_cut_at_the_end_is_not() {
        assert_eq!(
            rows(b"\xff\n{\"start\":0}\n"),
            [LeadingRow::NotUtf8, object(true)]
        );
        assert_eq!(rows(b"{\"a\":\"\xff\"}\n"), [LeadingRow::NotUtf8]);
        assert_eq!(rows(b"{\"a\":\"\xc3\"}\n"), [LeadingRow::NotUtf8]);
        assert_eq!(rows(b"{}\xc3\n"), [LeadingRow::NotUtf8]);
        assert_eq!(
            rows(b"{}\n{\"start\":\"\xf0\x9f"),
            [object(false), LeadingRow::Unparsed]
        );
        assert_eq!(
            rows("{\"a\":\"\u{e9}\u{1F600}\"}\n".as_bytes()),
            [object(false)]
        );
    }

    #[test]
    fn utf8_validation_agrees_with_the_standard_library() {
        let check = |bytes: &[u8]| {
            let mut utf8 = Utf8::default();
            let mut valid = true;
            for byte in bytes {
                valid &= utf8.accept(*byte);
            }
            assert_eq!(
                valid && !utf8.open(),
                std::str::from_utf8(bytes).is_ok(),
                "{bytes:x?}"
            );
        };
        let edges = [
            0x00, 0x41, 0x7F, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC0, 0xFF,
        ];
        for first in 0..=255_u8 {
            check(&[first]);
            for second in 0..=255_u8 {
                check(&[first, second]);
                for third in edges {
                    check(&[first, second, third]);
                    for fourth in edges {
                        check(&[first, second, third, fourth]);
                    }
                }
            }
        }
    }
}
