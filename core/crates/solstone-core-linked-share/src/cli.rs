// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! `solstone journal share`: read-only matching and packaging, JSON out.

use std::ffi::OsString;
use std::path::Path;

use serde_json::json;

use crate::matching::{ActivityRef, match_activity};
use crate::names::PersonRef;
use crate::people::{Owner, resolve_person};
use crate::summary::build_package;
use crate::talents::TalentCaller;

pub const SHARE_USAGE: &str = "usage: solstone journal share [-h] {match,package} ...\n";
pub const MATCH_USAGE: &str =
    "usage: solstone journal share match [-h] --person PERSON --day YYYYMMDD [--facet FACET]\n";
pub const PACKAGE_USAGE: &str = "usage: solstone journal share package [-h] --person PERSON --day YYYYMMDD --facet FACET --activity ID\n";

const SHARE_HELP: &str = concat!(
    "usage: solstone journal share [-h] {match,package} ...\n\n",
    "Find activities shared with a linked person and preview what would be shared.\n",
    "Both subcommands only read the journal.\n\n",
    "commands:\n",
    "    match     List the day's activities that match a person, one JSON line each\n",
    "    package   Print the scoped summary package for one matching activity\n",
);
const MATCH_HELP: &str = concat!(
    "usage: solstone journal share match [-h] --person PERSON --day YYYYMMDD [--facet FACET]\n\n",
    "options:\n",
    "  --person PERSON   entity id, or a unique name or alias of a person\n",
    "  --day YYYYMMDD    the day to read\n",
    "  --facet FACET     only this facet (default: every facet)\n",
);
const PACKAGE_HELP: &str = concat!(
    "usage: solstone journal share package [-h] --person PERSON --day YYYYMMDD --facet FACET --activity ID\n\n",
    "options:\n",
    "  --person PERSON   entity id, or a unique name or alias of a person\n",
    "  --day YYYYMMDD    the activity's day\n",
    "  --facet FACET     the activity's facet\n",
    "  --activity ID     the activity id\n",
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ShareCommand {
    Help(&'static str),
    Usage {
        usage: &'static str,
        error: String,
    },
    Match {
        person: String,
        day: String,
        facet: Option<String>,
    },
    Package {
        person: String,
        day: String,
        facet: String,
        activity: String,
    },
}

/// Whether the arguments name a subcommand that reads the journal. Help and
/// usage errors read nothing and need no journal or model.
pub fn reads_journal(args: &[OsString]) -> bool {
    matches!(
        parse(args),
        ShareCommand::Match { .. } | ShareCommand::Package { .. }
    )
}

pub(crate) fn parse(args: &[OsString]) -> ShareCommand {
    let Some(args) = args
        .iter()
        .map(|arg| arg.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    else {
        return usage(SHARE_USAGE, "arguments must be valid UTF-8");
    };
    let Some((command, rest)) = args.split_first() else {
        return usage(SHARE_USAGE, "a subcommand is required");
    };
    let (help, usage_text) = match command.as_str() {
        "-h" | "--help" => return ShareCommand::Help(SHARE_HELP),
        "match" => (MATCH_HELP, MATCH_USAGE),
        "package" => (PACKAGE_HELP, PACKAGE_USAGE),
        other => return usage(SHARE_USAGE, &format!("unknown subcommand '{other}'")),
    };
    let mut person = None;
    let mut day = None;
    let mut facet = None;
    let mut activity = None;
    let mut values = rest.iter();
    while let Some(flag) = values.next() {
        let slot = match flag.as_str() {
            "-h" | "--help" => return ShareCommand::Help(help),
            "--person" => &mut person,
            "--day" => &mut day,
            "--facet" => &mut facet,
            "--activity" if command == "package" => &mut activity,
            other => return usage(usage_text, &format!("unrecognized argument '{other}'")),
        };
        match values.next() {
            Some(value) if !value.is_empty() && slot.is_none() => *slot = Some(value.clone()),
            Some(_) if slot.is_some() => {
                return usage(usage_text, &format!("{flag} given more than once"));
            }
            _ => return usage(usage_text, &format!("{flag} needs a value")),
        }
    }
    let Some(person) = person else {
        return usage(usage_text, "--person is required");
    };
    let Some(day) = day.filter(|day| solstone_core_format::segment::is_date_key(day)) else {
        return usage(usage_text, "--day YYYYMMDD is required");
    };
    if command == "match" {
        return ShareCommand::Match { person, day, facet };
    }
    let Some(facet) = facet else {
        return usage(usage_text, "--facet is required");
    };
    let Some(activity) = activity else {
        return usage(usage_text, "--activity is required");
    };
    ShareCommand::Package {
        person,
        day,
        facet,
        activity,
    }
}

fn usage(usage: &'static str, error: &str) -> ShareCommand {
    ShareCommand::Usage {
        usage,
        error: error.to_owned(),
    }
}

/// Run one `share` invocation. Nothing here writes to the journal.
pub fn run_cli(args: &[OsString], journal: &Path, talents: &dyn TalentCaller) -> CliRun {
    match parse(args) {
        ShareCommand::Help(text) => finished(text.to_owned(), String::new(), 0),
        ShareCommand::Usage { usage, error } => finished(
            String::new(),
            format!("{usage}solstone journal share: error: {error}\n"),
            2,
        ),
        ShareCommand::Match { person, day, facet } => {
            match run_match(journal, &person, &day, facet.as_deref(), talents) {
                Ok(stdout) => finished(stdout, String::new(), 0),
                Err(error) => failed(&error),
            }
        }
        ShareCommand::Package {
            person,
            day,
            facet,
            activity,
        } => match run_package(journal, &person, &day, &facet, &activity, talents) {
            Ok(stdout) => finished(stdout, String::new(), 0),
            Err(error) => failed(&error),
        },
    }
}

fn finished(stdout: String, stderr: String, exit_code: i32) -> CliRun {
    CliRun {
        stdout,
        stderr,
        exit_code,
    }
}

fn failed(error: &str) -> CliRun {
    finished(
        String::new(),
        format!("solstone journal share: {error}\n"),
        1,
    )
}

fn subjects(journal: &Path, person: &str) -> Result<(PersonRef, Owner), String> {
    Ok((resolve_person(journal, person)?, Owner::load(journal)?))
}

fn run_match(
    journal: &Path,
    person: &str,
    day: &str,
    facet: Option<&str>,
    talents: &dyn TalentCaller,
) -> Result<String, String> {
    let (person, owner) = subjects(journal, person)?;
    let facets = match facet {
        Some(facet) => vec![facet.to_owned()],
        None => solstone_core_facets::list_declared_facet_names(journal)
            .map_err(|error| format!("could not list facets: {error}"))?,
    };
    let mut stdout = String::new();
    for facet in facets {
        let records = solstone_core_facets::load_activity_records(journal, &facet, day, false)
            .map_err(|error| format!("could not read {facet} activities for {day}: {error}"))?;
        for record in records {
            let Some(matched) =
                match_activity(journal, &facet, day, &record, &person, &owner, talents)
            else {
                continue;
            };
            let activity = ActivityRef::from_record(&record);
            let line = json!({
                "day": day,
                "facet": facet,
                "activity_id": activity.id,
                "activity_type": activity.activity_type,
                "basis": matched.basis,
                "shape": matched.shape,
                "segments": matched.segments,
                "threads": matched.threads,
            });
            stdout.push_str(&line.to_string());
            stdout.push('\n');
        }
    }
    Ok(stdout)
}

fn run_package(
    journal: &Path,
    person: &str,
    day: &str,
    facet: &str,
    activity: &str,
    talents: &dyn TalentCaller,
) -> Result<String, String> {
    let (person, owner) = subjects(journal, person)?;
    let record = solstone_core_facets::get_activity_record(journal, facet, day, activity)
        .map_err(|error| format!("could not read {facet} activities for {day}: {error}"))?
        .filter(|record| {
            !record
                .get("hidden")
                .is_some_and(solstone_core_facets::activity_value_truthy)
        })
        .ok_or_else(|| format!("no activity '{activity}' in {facet} on {day}"))?;
    let package = build_package(journal, facet, day, &record, &person, &owner, talents)
        .ok_or_else(|| format!("activity '{activity}' does not match {}", person.name))?;
    let mut stdout = serde_json::to_string(&package).map_err(|error| error.to_string())?;
    stdout.push('\n');
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn match_and_package_arguments_parse() {
        assert_eq!(
            parse(&args(&[
                "match", "--person", "jane_doe", "--day", "20261013"
            ])),
            ShareCommand::Match {
                person: "jane_doe".into(),
                day: "20261013".into(),
                facet: None
            }
        );
        assert_eq!(
            parse(&args(&[
                "package",
                "--day",
                "20261013",
                "--facet",
                "work",
                "--activity",
                "a1",
                "--person",
                "Jane Doe"
            ])),
            ShareCommand::Package {
                person: "Jane Doe".into(),
                day: "20261013".into(),
                facet: "work".into(),
                activity: "a1".into()
            }
        );
    }

    #[test]
    fn help_and_usage_errors_need_no_journal() {
        assert!(!reads_journal(&args(&["--help"])));
        assert!(!reads_journal(&args(&["match", "-h"])));
        assert!(!reads_journal(&args(&[
            "package", "--person", "x", "--help"
        ])));
        assert!(!reads_journal(&args(&["--nonsense"])));
        assert!(!reads_journal(&args(&["match"])));
        assert!(reads_journal(&args(&[
            "match", "--person", "x", "--day", "20261013"
        ])));
    }

    #[test]
    fn malformed_arguments_are_usage_errors() {
        let refused = |values: &[&str]| matches!(parse(&args(values)), ShareCommand::Usage { .. });
        assert!(refused(&[]));
        assert!(refused(&["send"]));
        assert!(refused(&["match", "--day", "20261013"]));
        assert!(refused(&["match", "--person", "x", "--day", "2026-10-13"]));
        assert!(refused(&[
            "match",
            "--person",
            "x",
            "--day",
            "20261013",
            "--activity",
            "a"
        ]));
        assert!(refused(&[
            "match", "--person", "x", "--person", "y", "--day", "20261013"
        ]));
        assert!(refused(&["match", "--person"]));
        assert!(refused(&[
            "package", "--person", "x", "--day", "20261013", "--facet", "w"
        ]));
        let run = run_cli(&args(&["match"]), Path::new(""), &|_: &str, _: &str| {
            Err("unused".to_owned())
        });
        assert_eq!(run.exit_code, 2);
        assert!(run.stderr.starts_with(MATCH_USAGE), "{}", run.stderr);
    }
}
