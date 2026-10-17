// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Locating nominated content in a unit and copying it with bounded context.
//!
//! Every position here counts Unicode scalar values, so the budgets below are
//! visible characters. Positions become UTF-8 byte offsets only when a passage
//! leaves this module.

use std::collections::{BTreeSet, HashMap, HashSet};

/// The line every rendered passage starts with.
pub const PASSAGE_PREFIX: &str = "Verbatim from source:\n";
/// The most visible characters one unit's passages may render, prefixes included.
pub const MAX_RENDERED_CHARS: usize = 3840;
/// The most nearby context added to either edge of a located passage.
pub const EDGE_CONTEXT_CHARS: usize = 256;

const WINDOW_STEP: usize = 80;
const WINDOW_WIDTHS: [usize; 3] = [320, 640, 960];
const MIN_COVERAGE: f64 = 0.55;
const LENGTH_PENALTY: f64 = 0.00015;
// The locator budgets a passage as a quoted string behind a 22-character label,
// the rendering it was validated with; that is never looser than the copy.
const QUOTED_LABEL_CHARS: usize = 22;

const STOP: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "but",
    "if",
    "then",
    "to",
    "of",
    "for",
    "from",
    "in",
    "on",
    "at",
    "by",
    "with",
    "as",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "do",
    "does",
    "did",
    "have",
    "has",
    "had",
    "i",
    "you",
    "we",
    "they",
    "he",
    "she",
    "me",
    "my",
    "your",
    "our",
    "their",
    "not",
    "no",
    "can",
    "could",
    "would",
    "should",
    "will",
    "may",
    "might",
    "must",
    "text",
    "displayed",
    "captured",
    "source",
    "reports",
    "reported",
    "requests",
    "requested",
    "asks",
    "asked",
    "says",
    "said",
    "notes",
    "note",
    "states",
    "stated",
    "contains",
    "shows",
    "shown",
    "describes",
    "described",
    "mentions",
    "mentioned",
    "explicitly",
    "user",
    "assistant",
    "message",
    "content",
    "fact",
    "facts",
];

/// A unit's text indexed by character, with the byte offset of every position.
pub(crate) struct Source<'a> {
    text: &'a str,
    chars: Vec<char>,
    bytes: Vec<usize>,
}

impl<'a> Source<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        let mut chars = Vec::new();
        let mut bytes = Vec::new();
        for (offset, character) in text.char_indices() {
            bytes.push(offset);
            chars.push(character);
        }
        bytes.push(text.len());
        Self { text, chars, bytes }
    }

    pub(crate) fn len(&self) -> usize {
        self.chars.len()
    }

    pub(crate) fn slice(&self, start: usize, end: usize) -> &'a str {
        &self.text[self.bytes[start]..self.bytes[end]]
    }

    pub(crate) fn byte(&self, position: usize) -> usize {
        self.bytes[position]
    }
}

pub(crate) fn prefix_chars() -> usize {
    PASSAGE_PREFIX.chars().count()
}

/// The whole unit as one passage, when it fits the rendered budget.
pub(crate) fn whole(source: &Source) -> Option<(usize, usize)> {
    (prefix_chars() + source.len() <= MAX_RENDERED_CHARS).then_some((0, source.len()))
}

fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if is_word(character) {
            current.push(character);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn tokens(text: &str) -> Vec<String> {
    words(&caseless::default_case_fold_str(text))
        .into_iter()
        .filter(|word| word.chars().count() > 2 && !STOP.contains(&word.as_str()))
        .collect()
}

/// Locate each nomination in the unit from local lexical evidence alone. The
/// accepted windows are merged where they touch; a nomination that names fewer
/// than two distinctive source terms, matches no window well enough, or would
/// overrun the budget is counted as rejected.
pub(crate) fn locate(source: &Source, notes: &[String]) -> (Vec<(usize, usize)>, usize) {
    let all_words = words(&caseless::default_case_fold_str(source.text));
    let mut frequency: HashMap<&str, usize> = HashMap::new();
    for word in &all_words {
        *frequency.entry(word.as_str()).or_default() += 1;
    }
    let length = source.len();
    let windows = (0..length)
        .step_by(WINDOW_STEP)
        .flat_map(|start| {
            WINDOW_WIDTHS
                .iter()
                .map(move |width| (start, length.min(start + width)))
        })
        .collect::<Vec<_>>();
    let bags = windows
        .iter()
        .map(|(start, end)| {
            tokens(source.slice(*start, *end))
                .into_iter()
                .collect::<HashSet<_>>()
        })
        .collect::<Vec<_>>();
    let mut intervals: Vec<(usize, usize)> = Vec::new();
    let mut rejected = 0;
    for note in notes {
        // Sorted terms keep the floating-point sums identical on every run.
        let terms = tokens(note)
            .into_iter()
            .filter(|term| frequency.contains_key(term.as_str()))
            .collect::<BTreeSet<_>>();
        let weights = terms
            .iter()
            .map(|term| {
                let count = frequency[term.as_str()] as f64;
                (term, (1.0 + all_words.len() as f64 / (1.0 + count)).ln())
            })
            .collect::<Vec<_>>();
        let total = weights.iter().fold(0.0, |sum, (_, weight)| sum + weight);
        if terms.len() < 2 || total == 0.0 {
            rejected += 1;
            continue;
        }
        let mut best: Option<(f64, f64, usize, usize)> = None;
        for ((start, end), bag) in windows.iter().zip(&bags) {
            let hit = weights.iter().fold(0.0, |sum, (term, weight)| {
                if bag.contains(term.as_str()) {
                    sum + weight
                } else {
                    sum
                }
            });
            if hit / total < MIN_COVERAGE {
                continue;
            }
            // Favor complete lexical coverage, then the shorter local passage.
            let candidate = (
                hit / total - LENGTH_PENALTY * (end - start) as f64,
                hit,
                *start,
                *end,
            );
            if best.is_none_or(|current| greater(candidate, current)) {
                best = Some(candidate);
            }
        }
        let Some((_, _, start, end)) = best else {
            rejected += 1;
            continue;
        };
        let mut candidate = intervals.clone();
        candidate.push((start, end));
        let quoted = merge(&candidate)
            .iter()
            .map(|(start, end)| quoted_chars(source, *start, *end))
            .sum::<usize>();
        if quoted > MAX_RENDERED_CHARS {
            rejected += 1;
            continue;
        }
        intervals = candidate;
    }
    (merge(&intervals), rejected)
}

fn greater(left: (f64, f64, usize, usize), right: (f64, f64, usize, usize)) -> bool {
    left.0
        .total_cmp(&right.0)
        .then(left.1.total_cmp(&right.1))
        .then(left.2.cmp(&right.2))
        .then(left.3.cmp(&right.3))
        .is_gt()
}

/// Sorted intervals, with touching or overlapping ones joined.
pub(crate) fn merge(intervals: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted = intervals.to_vec();
    sorted.sort_unstable();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (start, end) in sorted {
        match groups.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => groups.push((start, end)),
        }
    }
    groups
}

fn quoted_chars(source: &Source, start: usize, end: usize) -> usize {
    let escaped = source.chars[start..end]
        .iter()
        .map(|character| match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            control if (*control as u32) < 0x20 => 6,
            _ => 1,
        })
        .sum::<usize>();
    QUOTED_LABEL_CHARS + 2 + escaped
}

/// The nearest sentence or line boundaries around a located span, each taken
/// only when it is within the edge budget, then any clipped word completed.
pub(crate) fn edges(source: &Source, start: usize, end: usize) -> (usize, usize) {
    let boundaries = boundaries(source);
    let before = boundaries
        .iter()
        .copied()
        .filter(|boundary| *boundary <= start)
        .max()
        .unwrap_or(0);
    let after = boundaries
        .iter()
        .copied()
        .filter(|boundary| *boundary >= end)
        .min()
        .unwrap_or(source.len());
    let mut left = if start - before <= EDGE_CONTEXT_CHARS {
        before
    } else {
        start
    };
    let mut right = if after - end <= EDGE_CONTEXT_CHARS {
        after
    } else {
        end
    };
    let chars = &source.chars;
    while left > 0
        && start - left < EDGE_CONTEXT_CHARS
        && left < chars.len()
        && is_word(chars[left - 1])
        && is_word(chars[left])
    {
        left -= 1;
    }
    while right < chars.len()
        && right - end < EDGE_CONTEXT_CHARS
        && right > 0
        && is_word(chars[right - 1])
        && is_word(chars[right])
    {
        right += 1;
    }
    (left, right)
}

/// Line ends, and the end of a sentence mark (with an optional closing quote)
/// together with the whitespace after it.
fn boundaries(source: &Source) -> BTreeSet<usize> {
    let chars = &source.chars;
    let mut found = BTreeSet::from([0, chars.len()]);
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\n' {
            found.insert(index + 1);
            index += 1;
            continue;
        }
        if matches!(chars[index], '.' | '!' | '?') {
            let mut next = index + 1;
            if next < chars.len()
                && matches!(chars[next], '"' | '\'')
                && chars.get(next + 1).is_some_and(|c| c.is_whitespace())
            {
                next += 1;
            }
            if chars.get(next).is_some_and(|c| c.is_whitespace()) {
                while chars.get(next).is_some_and(|c| c.is_whitespace()) {
                    next += 1;
                }
                found.insert(next);
                index = next;
                continue;
            }
        }
        index += 1;
    }
    found
}

/// Widen each located group to its nearby boundaries while the unit's rendered
/// passages still fit, keeping every located group inside some passage.
pub(crate) fn expand(source: &Source, groups: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let rendered = |ranges: &[(usize, usize)]| {
        unique(ranges)
            .iter()
            .map(|(start, end)| prefix_chars() + end - start)
            .sum::<usize>()
    };
    let mut ranges = groups.to_vec();
    for (index, (start, end)) in groups.iter().enumerate() {
        let mut attempt = ranges.clone();
        attempt[index] = edges(source, *start, *end);
        if rendered(&attempt) <= MAX_RENDERED_CHARS {
            ranges = attempt;
        }
    }
    let ranges = unique(&ranges);
    debug_assert!(rendered(&ranges) <= MAX_RENDERED_CHARS);
    debug_assert!(groups.iter().all(|(start, end)| {
        ranges
            .iter()
            .any(|(left, right)| left <= start && end <= right)
    }));
    ranges
}

fn unique(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut seen = HashSet::new();
    ranges
        .iter()
        .copied()
        .filter(|range| seen.insert(*range))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clipped_word_is_completed_and_the_nearby_sentence_joins_within_budget() {
        let text = "Ari requests a backup before restarting.\nBea reports tests passed.\n";
        let source = Source::new(text);
        let start = text.find("backup").unwrap() + 2;
        let end = text.find("restarting").unwrap() + 3;
        let (left, right) = edges(&source, start, end);
        assert_eq!(
            source.slice(left, right),
            "Ari requests a backup before restarting.\n"
        );
    }

    #[test]
    fn context_beyond_the_edge_budget_is_not_taken() {
        let text = format!(
            "Prelude {}meaningfulword {}. Next sentence.",
            "word ".repeat(150),
            "detail ".repeat(230)
        );
        let source = Source::new(&text);
        let start = text.find("meaningfulword").unwrap() + 4;
        let end = text.find(". Next").unwrap() - 2;
        let (left, right) = edges(&source, start, end);
        assert_eq!(left, text.find("meaningfulword").unwrap());
        assert!(start - left <= EDGE_CONTEXT_CHARS && right - end <= EDGE_CONTEXT_CHARS);
        assert!(right > text.find(". Next").unwrap());
    }

    #[test]
    fn locating_is_lexical_and_rejects_a_nomination_without_two_source_terms() {
        let text = "Ari requests a backup before restarting the queue worker. ".repeat(80);
        let source = Source::new(&text);
        let (groups, rejected) = locate(
            &source,
            &[
                "Ari requests a backup before restarting".to_owned(),
                "zebra".to_owned(),
                "The team celebrated a launch".to_owned(),
            ],
        );
        assert_eq!(rejected, 2);
        assert_eq!(groups.len(), 1);
        assert!(source.slice(groups[0].0, groups[0].1).contains("backup"));
    }

    #[test]
    fn multibyte_positions_slice_on_character_boundaries() {
        let text = "Café ß 🪨 — Ari requests a backup.";
        let source = Source::new(text);
        let start = text.chars().position(|c| c == 'A').unwrap();
        assert_eq!(source.slice(start, source.len()), "Ari requests a backup.");
        assert_eq!(&text[source.byte(start)..], "Ari requests a backup.");
    }
}
