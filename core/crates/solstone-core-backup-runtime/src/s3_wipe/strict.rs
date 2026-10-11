// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Bounded protocol decoding for the owning teardown executor.

use std::collections::BTreeSet;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000;
const MAX_DEPTH: usize = 32;
const S3_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProtocolError {
    Malformed,
    Limit,
    Namespace,
    Pagination,
    Acknowledgment,
}

#[derive(Default)]
pub(super) struct InventoryLimit {
    bytes: usize,
    records: usize,
}

impl InventoryLimit {
    pub(super) fn add(&mut self, values: &[&str]) -> Result<(), ProtocolError> {
        let bytes = values.iter().try_fold(self.bytes, |total, value| {
            total.checked_add(value.len()).ok_or(ProtocolError::Limit)
        })?;
        let records = self.records.checked_add(1).ok_or(ProtocolError::Limit)?;
        if bytes > MAX_PAYLOAD || records > MAX_RECORDS {
            return Err(ProtocolError::Limit);
        }
        self.bytes = bytes;
        self.records = records;
        Ok(())
    }
}

#[derive(Default)]
struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn optional(&self, name: &str) -> Result<Option<&Node>, ProtocolError> {
        let mut fields = self.children.iter().filter(|node| node.name == name);
        let value = fields.next();
        if fields.next().is_some() {
            return Err(ProtocolError::Malformed);
        }
        Ok(value)
    }

    fn required(&self, name: &str) -> Result<&Node, ProtocolError> {
        self.optional(name)?.ok_or(ProtocolError::Malformed)
    }

    fn scalar(&self) -> Result<&str, ProtocolError> {
        if !self.children.is_empty() {
            return Err(ProtocolError::Malformed);
        }
        Ok(&self.text)
    }

    fn fields(&self, allowed: &[&str]) -> Result<(), ProtocolError> {
        if !self.text.chars().all(xml_whitespace)
            || self
                .children
                .iter()
                .any(|node| !allowed.contains(&node.name.as_str()))
        {
            return Err(ProtocolError::Malformed);
        }
        Ok(())
    }

    fn truncated(&self) -> Result<bool, ProtocolError> {
        match self
            .required("IsTruncated")?
            .scalar()?
            .trim_matches(xml_whitespace)
        {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(ProtocolError::Malformed),
        }
    }

    fn prefix(&self, prefix: &str) -> Result<(), ProtocolError> {
        if self
            .optional("Prefix")?
            .is_some_and(|node| node.scalar() != Ok(prefix))
        {
            return Err(ProtocolError::Namespace);
        }
        Ok(())
    }

    fn next(&self, name: &str, truncated: bool) -> Result<Option<String>, ProtocolError> {
        let value = self.optional(name)?.map(Node::scalar).transpose()?;
        match (truncated, value) {
            (true, Some(value)) if !value.is_empty() => Ok(Some(value.to_owned())),
            (false, None | Some("")) => Ok(None),
            _ => Err(ProtocolError::Pagination),
        }
    }
}

fn legal_xml(value: &str) -> bool {
    value.chars().all(|ch| matches!(ch, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}'))
}

fn xml_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r')
}

fn start(event: &BytesStart<'_>, root: bool) -> Result<Node, ProtocolError> {
    let name = std::str::from_utf8(event.name().as_ref())
        .map_err(|_| ProtocolError::Malformed)?
        .to_owned();
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(ProtocolError::Malformed);
    }
    for attribute in event.attributes() {
        let attribute = attribute.map_err(|_| ProtocolError::Malformed)?;
        if !root || attribute.key.as_ref() != b"xmlns" {
            return Err(ProtocolError::Malformed);
        }
        let value =
            std::str::from_utf8(attribute.value.as_ref()).map_err(|_| ProtocolError::Malformed)?;
        if quick_xml::escape::unescape(value).map_err(|_| ProtocolError::Malformed)? != S3_NAMESPACE
        {
            return Err(ProtocolError::Namespace);
        }
    }
    Ok(Node {
        name,
        ..Node::default()
    })
}

fn document(body: &[u8], expected: &str) -> Result<Node, ProtocolError> {
    if body.len() > MAX_BODY {
        return Err(ProtocolError::Limit);
    }
    let input = std::str::from_utf8(body).map_err(|_| ProtocolError::Malformed)?;
    if !legal_xml(input) {
        return Err(ProtocolError::Malformed);
    }
    let mut reader = Reader::from_str(input);
    reader.config_mut().check_comments = true;
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    let mut nodes = 0usize;
    let mut declaration = false;
    loop {
        match reader.read_event().map_err(|_| ProtocolError::Malformed)? {
            Event::Start(event) | Event::Empty(event) => {
                // Empty events are expanded by the reader configuration below.
                nodes = nodes.checked_add(1).ok_or(ProtocolError::Limit)?;
                if nodes > MAX_RECORDS || stack.len() >= MAX_DEPTH {
                    return Err(ProtocolError::Limit);
                }
                if stack.is_empty() && root.is_some() {
                    return Err(ProtocolError::Malformed);
                }
                let node = start(&event, stack.is_empty())?;
                if stack.is_empty() && node.name != expected {
                    return Err(ProtocolError::Malformed);
                }
                stack.push(node);
            }
            Event::End(_) => {
                let node = stack.pop().ok_or(ProtocolError::Malformed)?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else if root.replace(node).is_some() {
                    return Err(ProtocolError::Malformed);
                }
            }
            Event::Text(event) => {
                let text = event
                    .xml10_content()
                    .map_err(|_| ProtocolError::Malformed)?;
                if text.contains("]]>") {
                    return Err(ProtocolError::Malformed);
                }
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&text);
                } else if !text.chars().all(xml_whitespace) {
                    return Err(ProtocolError::Malformed);
                }
            }
            Event::CData(event) => {
                let text = event
                    .xml10_content()
                    .map_err(|_| ProtocolError::Malformed)?;
                stack
                    .last_mut()
                    .ok_or(ProtocolError::Malformed)?
                    .text
                    .push_str(&text);
            }
            Event::GeneralRef(event) => {
                let reference = event.decode().map_err(|_| ProtocolError::Malformed)?;
                let text = match event
                    .resolve_char_ref()
                    .map_err(|_| ProtocolError::Malformed)?
                {
                    Some(ch) => ch.to_string(),
                    None => quick_xml::escape::resolve_predefined_entity(&reference)
                        .ok_or(ProtocolError::Malformed)?
                        .to_owned(),
                };
                if !legal_xml(&text) {
                    return Err(ProtocolError::Malformed);
                }
                stack
                    .last_mut()
                    .ok_or(ProtocolError::Malformed)?
                    .text
                    .push_str(&text);
            }
            Event::Decl(event) => {
                if declaration
                    || nodes != 0
                    || root.is_some()
                    || event
                        .version()
                        .map_err(|_| ProtocolError::Malformed)?
                        .as_ref()
                        != b"1.0"
                {
                    return Err(ProtocolError::Malformed);
                }
                if let Some(encoding) = event.encoding() {
                    let encoding = encoding.map_err(|_| ProtocolError::Malformed)?;
                    if !encoding.eq_ignore_ascii_case(b"utf-8") {
                        return Err(ProtocolError::Malformed);
                    }
                }
                declaration = true;
            }
            Event::Comment(_) => {}
            Event::DocType(_) | Event::PI(_) => return Err(ProtocolError::Malformed),
            Event::Eof => {
                if !stack.is_empty() {
                    return Err(ProtocolError::Malformed);
                }
                return root.ok_or(ProtocolError::Malformed);
            }
        }
    }
}

pub(super) struct ObjectPage {
    pub keys: Vec<String>,
    pub next: Option<String>,
}

pub(super) fn objects(body: &[u8], prefix: &str) -> Result<ObjectPage, ProtocolError> {
    let root = document(body, "ListBucketResult")?;
    root.fields(&[
        "Name",
        "Prefix",
        "KeyCount",
        "MaxKeys",
        "IsTruncated",
        "NextContinuationToken",
        "ContinuationToken",
        "StartAfter",
        "Contents",
    ])?;
    root.prefix(prefix)?;
    let truncated = root.truncated()?;
    let next = root.next("NextContinuationToken", truncated)?;
    let mut keys = Vec::new();
    let mut seen = BTreeSet::new();
    for item in root.children.iter().filter(|node| node.name == "Contents") {
        item.fields(&[
            "Key",
            "LastModified",
            "ETag",
            "ChecksumAlgorithm",
            "ChecksumType",
            "Size",
            "StorageClass",
            "Owner",
            "RestoreStatus",
        ])?;
        let key = item.required("Key")?.scalar()?;
        if key.is_empty() || !key.starts_with(prefix) {
            return Err(ProtocolError::Namespace);
        }
        if !seen.insert(key) {
            return Err(ProtocolError::Malformed);
        }
        keys.push(key.to_owned());
    }
    Ok(ObjectPage { keys, next })
}

pub(super) struct UploadPage {
    pub uploads: Vec<(String, String)>,
    pub next: Option<(String, String)>,
}

pub(super) fn uploads(body: &[u8], prefix: &str) -> Result<UploadPage, ProtocolError> {
    let root = document(body, "ListMultipartUploadsResult")?;
    root.fields(&[
        "Bucket",
        "KeyMarker",
        "UploadIdMarker",
        "NextKeyMarker",
        "NextUploadIdMarker",
        "Prefix",
        "MaxUploads",
        "IsTruncated",
        "Upload",
    ])?;
    root.prefix(prefix)?;
    let truncated = root.truncated()?;
    let next = root
        .next("NextKeyMarker", truncated)?
        .zip(root.next("NextUploadIdMarker", truncated)?);
    let mut uploads = Vec::new();
    let mut seen = BTreeSet::new();
    for item in root.children.iter().filter(|node| node.name == "Upload") {
        item.fields(&[
            "Key",
            "UploadId",
            "Initiator",
            "Owner",
            "StorageClass",
            "Initiated",
            "ChecksumAlgorithm",
            "ChecksumType",
        ])?;
        let key = item.required("Key")?.scalar()?;
        let id = item.required("UploadId")?.scalar()?;
        if key.is_empty() || !key.starts_with(prefix) || id.is_empty() {
            return Err(ProtocolError::Namespace);
        }
        if !seen.insert((key, id)) {
            return Err(ProtocolError::Malformed);
        }
        uploads.push((key.to_owned(), id.to_owned()));
    }
    Ok(UploadPage { uploads, next })
}

pub(super) fn delete_body(keys: &[String]) -> Result<Vec<u8>, ProtocolError> {
    let mut body = String::from("<Delete>");
    for key in keys {
        body.push_str("<Object><Key>");
        // Numeric CR keeps XML end-of-line normalization from changing the key.
        let escaped = quick_xml::escape::escape(key).replace('\r', "&#13;");
        if body
            .len()
            .checked_add(escaped.len())
            .and_then(|length| length.checked_add(32))
            .is_none_or(|length| length > MAX_BODY)
        {
            return Err(ProtocolError::Limit);
        }
        body.push_str(&escaped);
        body.push_str("</Key></Object>");
    }
    body.push_str("</Delete>");
    Ok(body.into_bytes())
}

pub(super) fn deleted(body: &[u8], keys: &[String]) -> Result<(), ProtocolError> {
    let root = document(body, "DeleteResult")?;
    root.fields(&["Deleted", "Error"])?;
    let expected = keys.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if expected.len() != keys.len() {
        return Err(ProtocolError::Acknowledgment);
    }
    let mut seen = BTreeSet::new();
    for item in &root.children {
        match item.name.as_str() {
            "Deleted" => {
                item.fields(&["Key", "VersionId", "DeleteMarker", "DeleteMarkerVersionId"])
            }
            "Error" => item.fields(&["Key", "Code", "Message", "VersionId"]),
            _ => Err(ProtocolError::Malformed),
        }?;
        let key = item.required("Key")?.scalar()?;
        if !expected.contains(key)
            || !seen.insert(key)
            || item.name == "Error" && item.required("Code")?.scalar()? != "NoSuchKey"
        {
            return Err(ProtocolError::Acknowledgment);
        }
    }
    if seen != expected {
        return Err(ProtocolError::Acknowledgment);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
