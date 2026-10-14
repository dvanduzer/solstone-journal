// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Conversation threads seen during an activity, with each distinct message
//! settled to one sender across every capture of it.

use std::collections::BTreeMap;

use crate::live::ActivityWindow;
use crate::names::{PersonRef, normalize};
use crate::people::Owner;
use crate::source::SegmentSource;

pub(crate) type ThreadKey = (String, String);

/// Who a source line belongs to, as code (never the model) decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Party {
    Owner,
    Person,
    /// Anyone else, with the label the source shows for them.
    Other(String),
}

/// One distinct message (by normalized text) of a thread, across captures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadMessage {
    /// The first capture's text.
    pub(crate) text: String,
    /// The first segment the message appears in.
    pub(crate) segment: String,
    senders: Vec<String>,
    from_me: Vec<bool>,
    /// Each capture's visible stamp with the frame's capture time.
    timestamps: Vec<(String, Option<u32>)>,
}

impl ThreadMessage {
    /// The first visible timestamp.
    pub(crate) fn timestamp(&self) -> Option<&str> {
        self.timestamps.first().map(|(stamp, _)| stamp.as_str())
    }

    pub(crate) fn is_live(&self, window: Option<&ActivityWindow>) -> bool {
        window.is_some_and(|window| {
            self.timestamps
                .iter()
                .any(|(stamp, captured_at)| window.is_live(stamp, *captured_at))
        })
    }

    /// The sender most captures show; the earliest of a tie.
    pub(crate) fn sender(&self) -> &str {
        let mut counts = BTreeMap::<&str, usize>::new();
        for sender in &self.senders {
            *counts.entry(sender.as_str()).or_default() += 1;
        }
        let best = counts.values().copied().max().unwrap_or_default();
        self.senders
            .iter()
            .find(|sender| counts[sender.as_str()] == best)
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// The `from_me` majority among captures that carry it; a tie or no
    /// capture with the field is unknown.
    fn sent_by_owner(&self) -> Option<bool> {
        let mine = self.from_me.iter().filter(|value| **value).count();
        let theirs = self.from_me.len() - mine;
        (mine != theirs).then_some(mine > theirs)
    }

    /// Settle who sent this message. `from_me` decides the owner's side when
    /// present; otherwise the sender's name does. In a 1:1 thread every
    /// message not from the owner is the person's.
    pub(crate) fn party(&self, person: &PersonRef, owner: &Owner, one_to_one: bool) -> Party {
        let sender = self.sender();
        let is_owner = self
            .sent_by_owner()
            .unwrap_or_else(|| owner.is_sender(sender));
        if is_owner {
            Party::Owner
        } else if one_to_one || person.is_sender(sender) {
            Party::Person
        } else {
            Party::Other(sender.to_owned())
        }
    }
}

/// One (app, thread) seen in a conversation view during the activity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Thread {
    pub(crate) key: ThreadKey,
    pub(crate) messages: Vec<ThreadMessage>,
    /// Conversation-view frames showing this thread.
    pub(crate) frames: usize,
    /// Segments with a frame of this thread, in activity order.
    pub(crate) segments: Vec<String>,
}

impl Thread {
    pub(crate) fn title_names(&self, person: &PersonRef) -> bool {
        person.names(&self.key.1)
    }
}

/// Every conversation-view thread in first-seen order, and how many
/// conversation-view frames the activity holds. Inbox and list views never
/// count.
pub(crate) fn conversation_threads(sources: &[SegmentSource]) -> (Vec<Thread>, usize) {
    let mut threads: Vec<Thread> = Vec::new();
    let mut seen: BTreeMap<(ThreadKey, String), usize> = BTreeMap::new();
    let mut total = 0;
    for source in sources {
        for frame in source.frames.iter().filter(|frame| frame.is_conversation()) {
            total += 1;
            let key = frame.key();
            let index = match threads.iter().position(|thread| thread.key == key) {
                Some(index) => index,
                None => {
                    threads.push(Thread {
                        key: key.clone(),
                        messages: Vec::new(),
                        frames: 0,
                        segments: Vec::new(),
                    });
                    threads.len() - 1
                }
            };
            let thread = &mut threads[index];
            thread.frames += 1;
            if !thread.segments.contains(&source.segment) {
                thread.segments.push(source.segment.clone());
            }
            for message in &frame.messages {
                let folded = normalize(&message.text);
                if folded.is_empty() {
                    continue;
                }
                let slot = *seen.entry((key.clone(), folded)).or_insert_with(|| {
                    thread.messages.push(ThreadMessage {
                        text: message.text.clone(),
                        segment: source.segment.clone(),
                        senders: Vec::new(),
                        from_me: Vec::new(),
                        timestamps: Vec::new(),
                    });
                    thread.messages.len() - 1
                });
                let distinct = &mut thread.messages[slot];
                distinct.senders.push(message.sender.clone());
                distinct.from_me.extend(message.from_me);
                distinct.timestamps.extend(
                    message
                        .timestamp
                        .clone()
                        .map(|stamp| (stamp, frame.captured_at)),
                );
            }
        }
    }
    (threads, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{FrameMessage, MessagingFrame};

    fn message(sender: &str, from_me: Option<bool>, stamp: &str, text: &str) -> FrameMessage {
        FrameMessage {
            sender: sender.to_owned(),
            from_me,
            timestamp: (!stamp.is_empty()).then(|| stamp.to_owned()),
            text: text.to_owned(),
        }
    }

    fn frame(thread: &str, view: &str, messages: Vec<FrameMessage>) -> MessagingFrame {
        MessagingFrame {
            captured_at: None,
            app: "Signal".to_owned(),
            thread: thread.to_owned(),
            view: view.to_owned(),
            messages,
        }
    }

    fn person() -> PersonRef {
        PersonRef::new("jane_doe", "Jane Doe", &[])
    }

    fn owner() -> Owner {
        Owner::new(Some("sam_rivera".to_owned()), "Sam Rivera", &[])
    }

    #[test]
    fn threads_dedupe_messages_and_ignore_inbox_views() {
        let sources = vec![
            SegmentSource {
                segment: "150000_300".to_owned(),
                frames: vec![
                    frame(
                        "Jane Doe",
                        "conversation",
                        vec![message("Jane Doe", None, "", "Lunch at noon?")],
                    ),
                    frame(
                        "Inbox",
                        "inbox",
                        vec![message("Jane Doe", None, "", "Lunch at noon?")],
                    ),
                ],
                ..SegmentSource::default()
            },
            SegmentSource {
                segment: "150500_300".to_owned(),
                frames: vec![frame(
                    "Jane Doe",
                    "conversation",
                    vec![
                        message("Jane Doe", None, "", "lunch at NOON?"),
                        message("Me", None, "", "Yes!"),
                    ],
                )],
                ..SegmentSource::default()
            },
        ];
        let (threads, total) = conversation_threads(&sources);
        assert_eq!(total, 2);
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.frames, 2);
        assert_eq!(thread.segments, vec!["150000_300", "150500_300"]);
        let texts = thread
            .messages
            .iter()
            .map(|message| (message.text.as_str(), message.segment.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![("Lunch at noon?", "150000_300"), ("Yes!", "150500_300")]
        );
    }

    #[test]
    fn from_me_majority_settles_the_owner_side() {
        let distinct = ThreadMessage {
            text: "On my way".to_owned(),
            segment: "150000_300".to_owned(),
            // The capture names the person on the owner's own bubble twice.
            senders: vec!["Jane Doe".into(), "Jane Doe".into(), "Me".into()],
            from_me: vec![true, true, false],
            timestamps: Vec::new(),
        };
        assert_eq!(distinct.party(&person(), &owner(), false), Party::Owner);
    }

    #[test]
    fn absent_from_me_falls_back_to_the_majority_sender() {
        let mut distinct = ThreadMessage {
            text: "See you".to_owned(),
            segment: "150000_300".to_owned(),
            senders: vec!["Jane Doe".into(), "Jane Doe".into(), "Me".into()],
            from_me: Vec::new(),
            timestamps: Vec::new(),
        };
        assert_eq!(distinct.party(&person(), &owner(), false), Party::Person);
        distinct.senders = vec!["You".into()];
        assert_eq!(distinct.party(&person(), &owner(), false), Party::Owner);
        distinct.senders = vec!["Lee Chen".into()];
        assert_eq!(
            distinct.party(&person(), &owner(), false),
            Party::Other("Lee Chen".into())
        );
        // In a 1:1 thread, anyone but the owner is the person.
        assert_eq!(distinct.party(&person(), &owner(), true), Party::Person);
        // A tie in from_me is unknown, so the sender decides.
        distinct.from_me = vec![true, false];
        distinct.senders = vec!["Sam Rivera".into()];
        assert_eq!(distinct.party(&person(), &owner(), false), Party::Owner);
    }
}
