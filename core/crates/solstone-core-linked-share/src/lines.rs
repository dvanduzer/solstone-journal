// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Id-tagged source lines with code-settled parties.

use crate::messages::{Party, Thread, ThreadKey};
use crate::names::PersonRef;
use crate::people::Owner;
use crate::source::{SegmentSource, Utterance, Voice};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceLine {
    pub(crate) id: String,
    pub(crate) party: Party,
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) timestamp: Option<String>,
}

impl SourceLine {
    pub(crate) fn render(&self) -> String {
        match &self.timestamp {
            Some(stamp) => format!("[{}] {}: {} ({stamp})", self.id, self.label, self.text),
            None => format!("[{}] {}: {}", self.id, self.label, self.text),
        }
    }
}

/// Who spoke a line and how it is labelled: the owner, the person only when
/// the voice label resolves to the person's entity, or anyone else by entity
/// name or diarization number.
pub(crate) fn utterance_party(
    utterance: &Utterance,
    person: &PersonRef,
    owner: &Owner,
) -> (Party, String) {
    match &utterance.voice {
        Voice::Owner => (Party::Owner, owner.name.clone()),
        Voice::Entity { id, .. } if *id == person.entity_id => (Party::Person, person.name.clone()),
        Voice::Entity { name, .. } => (Party::Other(name.clone()), name.clone()),
        Voice::Unidentified => {
            let label = match utterance.speaker {
                Some(number) => format!("Speaker {number}"),
                None => "Speaker".to_owned(),
            };
            (Party::Other(label.clone()), label)
        }
    }
}

/// Every utterance of one segment, ids `u1..`.
pub(crate) fn conversation_lines(
    source: &SegmentSource,
    person: &PersonRef,
    owner: &Owner,
) -> Vec<SourceLine> {
    source
        .utterances
        .iter()
        .enumerate()
        .map(|(index, utterance)| {
            let (party, label) = utterance_party(utterance, person, owner);
            SourceLine {
                id: format!("u{}", index + 1),
                party,
                label,
                text: utterance.text.clone(),
                timestamp: None,
            }
        })
        .collect()
}

/// The matched threads' distinct messages first seen in `segment`, ids
/// `m1..`. Nothing else on screen is included.
pub(crate) fn message_lines(
    threads: &[Thread],
    matched: &[ThreadKey],
    segment: &str,
    person: &PersonRef,
    owner: &Owner,
) -> Vec<SourceLine> {
    let mut lines = Vec::new();
    for thread in threads
        .iter()
        .filter(|thread| matched.contains(&thread.key))
    {
        let one_to_one = thread.title_names(person);
        for message in thread
            .messages
            .iter()
            .filter(|message| message.segment == segment)
        {
            let party = message.party(person, owner, one_to_one);
            let label = match &party {
                Party::Owner => owner.name.clone(),
                Party::Person => person.name.clone(),
                Party::Other(sender) if sender.is_empty() => "Unknown sender".to_owned(),
                Party::Other(sender) => sender.clone(),
            };
            lines.push(SourceLine {
                id: format!("m{}", lines.len() + 1),
                party,
                label,
                text: message.text.clone(),
                timestamp: message.timestamp().map(str::to_owned),
            });
        }
    }
    lines
}
