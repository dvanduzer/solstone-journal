// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Who a piece of text names: one person's name, aliases and OCR misreadings.

use serde_json::Value;
use solstone_core_entity::JournalEntity;

/// A bare first name this short is too common to tie a thread to someone.
const WEAK_FIRST_NAME_CHARS: usize = 5;
/// Names at least this long tolerate a capture misreading.
const FUZZY_MIN_CHARS: usize = 6;
const FUZZY_MAX_EDITS: usize = 2;

/// One person as the matcher reads them: an entity's name and every alias.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonRef {
    pub entity_id: String,
    pub name: String,
    pub aliases: Vec<String>,
}

impl PersonRef {
    /// Build from a name and its aliases. The first name of a full name of two
    /// or more words is added as an alias.
    pub fn new(entity_id: impl Into<String>, name: impl Into<String>, aka: &[String]) -> Self {
        let name = name.into().trim().to_owned();
        let mut aliases = Vec::new();
        let mut seen = vec![normalize(&name)];
        let tokens = name.split_whitespace().collect::<Vec<_>>();
        let first = (tokens.len() >= 2).then(|| tokens[0].to_owned());
        for alias in aka.iter().cloned().chain(first) {
            let folded = normalize(&alias);
            if !folded.is_empty() && !seen.contains(&folded) {
                seen.push(folded);
                aliases.push(alias.trim().to_owned());
            }
        }
        Self {
            entity_id: entity_id.into(),
            name,
            aliases,
        }
    }

    pub fn from_entity(entity: &JournalEntity) -> Self {
        let aka = entity
            .value
            .get("aka")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let name = entity
            .value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Self::new(entity.id.clone(), name, &aka)
    }

    /// Whether a thread title or sender names this person. A bare short first
    /// name never does; a long name survives up to two misread characters.
    pub fn names(&self, text: &str) -> bool {
        let text = normalize(text);
        if text.is_empty() {
            return false;
        }
        self.structural_forms().any(|form| {
            contains_words(&text, &form)
                || (form.chars().count() >= FUZZY_MIN_CHARS
                    && !ends_in_initial(&form)
                    && fuzzy_contains(&text, &form))
        })
    }

    /// Whether a message's sender label is this person. Chat apps label a
    /// contact by first name alone, so a label that is exactly one of the
    /// person's names (a bare first name included) counts; anything longer
    /// must name them structurally.
    pub fn is_sender(&self, sender: &str) -> bool {
        let label = normalize(sender);
        !label.is_empty() && (self.forms().any(|form| form == label) || self.names(sender))
    }

    /// Whether spoken text mentions this person, a bare first name included.
    pub fn mentioned_in(&self, text: &str) -> bool {
        let text = normalize(text);
        !text.is_empty() && self.forms().any(|form| contains_words(&text, &form))
    }

    fn forms(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(&self.name)
            .chain(&self.aliases)
            .map(|form| normalize(form))
            .filter(|form| !form.is_empty())
    }

    /// Every form except a short first name standing for a longer full name,
    /// plus the first name with the initial of a later name part ("dana v",
    /// "Jane D."),
    /// which is how many chat apps title a contact.
    fn structural_forms(&self) -> impl Iterator<Item = String> + '_ {
        let full = normalize(&self.name);
        let weak = full
            .split_once(' ')
            .map(|(first, _)| first.to_owned())
            .filter(|first| first.chars().count() <= WEAK_FIRST_NAME_CHARS);
        let tokens = full.split(' ').collect::<Vec<_>>();
        // "Dana Van Holt" is shown as "dana v" or "dana h": the first name
        // with the initial of any later part of the name.
        let initialed = tokens
            .iter()
            .skip(1)
            .filter_map(|part| part.chars().next())
            .map(|initial| format!("{} {initial}", tokens[0]))
            .collect::<Vec<_>>();
        self.forms()
            .filter(move |form| weak.as_deref() != Some(form.as_str()))
            .chain(initialed)
    }
}

/// Lowercase, punctuation to spaces, whitespace collapsed.
pub fn normalize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_lowercase().collect::<String>()
            } else {
                " ".to_owned()
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_words(text: &str, form: &str) -> bool {
    format!(" {text} ").contains(&format!(" {form} "))
}

/// A form ending in a single-letter word is an initial, which must match exactly.
fn ends_in_initial(form: &str) -> bool {
    form.rsplit(' ')
        .next()
        .is_some_and(|word| word.chars().count() == 1)
}

/// Whether some run of as many words as `form` has lies within the edit
/// budget of it: a misread name inside a longer title ("Signal - Aneshk Bharti").
fn fuzzy_contains(text: &str, form: &str) -> bool {
    let words = text.split(' ').collect::<Vec<_>>();
    let width = form.split(' ').count();
    words.len() >= width
        && words
            .windows(width)
            .any(|run| edit_distance(&run.join(" "), form) <= FUZZY_MAX_EDITS)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (row, left_char) in left.chars().enumerate() {
        let mut current = vec![row + 1; right.len() + 1];
        for (column, right_char) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != *right_char);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        previous = current;
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jane() -> PersonRef {
        PersonRef::new("jane_doe", "Jane Doe", &["JD".to_owned()])
    }

    #[test]
    fn normalization_folds_case_punctuation_and_spacing() {
        assert_eq!(normalize("  Jane   O'Doe!! "), "jane o doe");
        assert_eq!(normalize("—"), "");
    }

    #[test]
    fn first_name_alias_is_derived_only_from_multi_word_names() {
        assert_eq!(jane().aliases, vec!["JD".to_owned(), "Jane".to_owned()]);
        assert!(PersonRef::new("x", "Cher", &[]).aliases.is_empty());
    }

    #[test]
    fn whole_word_names_and_aliases_match() {
        let person = jane();
        assert!(person.names("Chat with Jane Doe"));
        assert!(person.names("jd"));
        assert!(!person.names("Janet Doering"));
        assert!(!person.names(""));
    }

    #[test]
    fn ocr_misreadings_within_two_edits_match_long_names() {
        let person = PersonRef::new("arjun_mehra", "Arjun Mehra", &[]);
        assert!(person.names("Arjnu Mehra"));
        assert!(person.names("Arjun Mehraa"));
        assert!(!person.names("Arthur Mendez"));
        // A misreading inside a longer thread title still matches.
        assert!(person.names("Signal - Arjnu Mehra"));
        assert!(person.names("Chats - Arjun Mehar"));
        assert!(!person.names("Signal - Arthur Mendez"));
        // A short alias never takes the fuzzy path.
        let short = PersonRef::new("bo", "Bo Kim", &["Bok".to_owned()]);
        assert!(!short.names("Bob"));
    }

    #[test]
    fn a_bare_short_first_name_is_spoken_but_not_structural() {
        let person = jane();
        assert!(!person.names("Jane"));
        assert!(person.mentioned_in("did Jane send the deck?"));
        assert!(!person.mentioned_in("Janeway is on"));
        let long = PersonRef::new("x", "Roberta Lane", &[]);
        assert!(long.names("Roberta"));
    }

    #[test]
    fn first_name_and_last_initial_is_structural() {
        let person = PersonRef::new("x", "Dana Van Holt", &[]);
        assert!(person.names("dana h"));
        assert!(person.names("dana v"));
        assert!(person.names("Conversation with Dana H."));
        assert!(!person.names("dana"));
        assert!(!person.names("dana k"));
    }

    #[test]
    fn an_exact_first_name_sender_label_is_the_person() {
        let person = jane();
        assert!(person.is_sender("Jane"));
        assert!(person.is_sender("jane doe"));
        assert!(!person.is_sender("Jane's mom"));
        assert!(!person.is_sender("Janet"));
        assert!(!person.is_sender(""));
    }
}
