// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;

use solstone_core_backup::archive_admission::ArchiveCancellation;
use solstone_core_offload::teardown::{
    OwnedTeardownOutcome, TeardownRunRefusal, run_owned_teardown,
};

use super::{CliRun, ProductionClock};

pub(super) const PRELAUNCH_HELD: &str =
    "backup teardown is temporarily unavailable. no backup changes were made.";

/// The complete native outcome remains alive while output is borrowed.
#[must_use = "Retain the whole teardown result through printing and exit"]
pub struct OwnedTeardownCliRun {
    outcome: OwnedTeardownOutcome,
    output: CliRun,
}

impl std::fmt::Debug for OwnedTeardownCliRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedTeardownCliRun")
            .field("phase", &self.outcome.phase())
            .field("exit_code", &self.output.exit_code)
            .finish_non_exhaustive()
    }
}

impl OwnedTeardownCliRun {
    fn receive(outcome: OwnedTeardownOutcome) -> Self {
        let output = render(&outcome);
        Self { outcome, output }
    }

    pub fn outcome(&self) -> &OwnedTeardownOutcome {
        &self.outcome
    }

    pub fn output(&self) -> (&str, &str, i32) {
        (
            &self.output.stdout,
            &self.output.stderr,
            self.output.exit_code,
        )
    }
}

pub(super) fn run(journal: &Path) -> OwnedTeardownCliRun {
    OwnedTeardownCliRun::receive(run_owned_teardown(
        journal,
        Arc::new(ProductionClock),
        ArchiveCancellation::new(),
        env!("CARGO_PKG_VERSION"),
    ))
}

fn reason(refusal: Option<&TeardownRunRefusal>) -> &'static str {
    match refusal {
        Some(TeardownRunRefusal::NamespaceCooperationHeld) => "namespace_cooperation_held",
        Some(TeardownRunRefusal::UnsupportedPlatform) => "platform_unavailable",
        Some(TeardownRunRefusal::UnsupportedDestination) => "destination_unavailable",
        Some(TeardownRunRefusal::Root(_)) => "journal_unavailable",
        Some(TeardownRunRefusal::Budget(_)) => "deadline_unavailable",
        Some(TeardownRunRefusal::Admission(_)) => "admission_refused",
        Some(TeardownRunRefusal::CarrierPreparation) => "preparation_refused",
        Some(TeardownRunRefusal::Remote(_)) => "remote_refused",
        Some(TeardownRunRefusal::Clear) => "configuration_refused",
        Some(TeardownRunRefusal::Binding) => "binding_refused",
        #[cfg(unix)]
        Some(TeardownRunRefusal::Finish(_)) => "finish_refused",
        Some(TeardownRunRefusal::CarrierShared) => "carrier_shared",
        Some(TeardownRunRefusal::RetirementUncertain) => "retirement_uncertain",
        None => "none",
    }
}

fn render(outcome: &OwnedTeardownOutcome) -> CliRun {
    let (status, exit_code) = match outcome {
        OwnedTeardownOutcome::Finished(_) => ("completed", 0),
        OwnedTeardownOutcome::Held(_) => ("held", 1),
        OwnedTeardownOutcome::Unknown(_) => ("unknown", 1),
    };
    let mut stdout = format!(
        "teardown_status={status} reason={}\n",
        reason(outcome.refusal())
    );
    if let Some(remote) = outcome.remote() {
        writeln!(stdout, "delete_attempts={} delete_confirmations={} abort_attempts={} abort_confirmations={} objects_empty={} uploads_empty={}",
            remote.delete_attempts(), remote.delete_confirmations(), remote.abort_attempts(),
            remote.abort_confirmations(), remote.objects_empty(), remote.uploads_empty()).unwrap();
    }
    let attempts = outcome.attempts();
    writeln!(
        stdout,
        "configuration_attempted={} binding_attempted={} retirement_attempted={}",
        attempts.clear(),
        attempts.binding(),
        attempts.retirement()
    )
    .unwrap();
    if let Some(publication) = outcome.publication() {
        writeln!(stdout, "configuration_published=true durability_uncertain={} observation_uncertain={} confirmation_refused={}",
            publication.durability_uncertainty.is_some(), publication.observation_uncertainty.is_some(),
            publication.confirmation_refusal.is_some()).unwrap();
    }
    #[cfg(unix)]
    if let Some(binding) = outcome.binding() {
        writeln!(stdout, "binding_receipt_available=true removal_attempted={} removal_complete={} confirmation_refused={}",
            binding.attempted(), binding.complete(), binding.confirmation().is_err()).unwrap();
    }
    if let Some(retirement) = outcome.retirement() {
        writeln!(
            stdout,
            "retirement_effect={:?} root_durability={:?} confirmation={:?} failure_count={}",
            retirement.effect(),
            retirement.root_durability(),
            retirement.confirmation(),
            retirement.failures().len()
        )
        .unwrap();
    }
    let stderr = if exit_code == 0 {
        String::new()
    } else if outcome.phase() == solstone_core_offload::teardown::TeardownRunPhase::Prelaunch {
        format!("{PRELAUNCH_HELD}\n")
    } else {
        "backup teardown did not finish. keep recovery files and review the recorded effects before retrying.\n".into()
    };
    CliRun {
        stdout,
        stderr,
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CliOutcome, classify_tool_resolution, owned_teardown_options, run_cli};
    use solstone_core_offload::teardown::TeardownRunPhase;

    #[test]
    fn off_yes_retains_whole_prelaunch_result_without_root_or_tools() {
        for args in [
            vec!["off".into(), "--yes".into()],
            vec![
                "--verbose".into(),
                "off".into(),
                "--yes".into(),
                "--yes".into(),
                "--".into(),
            ],
        ] {
            assert_eq!(classify_tool_resolution(&args), None);
            let run = run_cli(&args, Path::new("/does-not-exist/teardown-cli"));
            let CliOutcome::Teardown(owned) = &run else {
                panic!("whole teardown outcome required")
            };
            let native = owned.outcome();
            assert_eq!(native.phase(), TeardownRunPhase::Prelaunch);
            assert!(matches!(
                native.refusal(),
                Some(
                    TeardownRunRefusal::NamespaceCooperationHeld
                        | TeardownRunRefusal::UnsupportedPlatform
                )
            ));
            assert!(native.journal_root().is_none());
            assert!(native.teardown_lease().is_none());
            assert!(native.remote().is_none());
            assert!(native.publication().is_none());
            assert!(native.retirement().is_none());
            assert!(!native.attempts().clear());
            assert!(!native.attempts().binding());
            assert!(!native.attempts().retirement());
            assert_ne!(run.output().2, 0);
            assert!(run.output().1.contains(PRELAUNCH_HELD));
        }
    }

    #[test]
    fn off_help_unconfirmed_and_invalid_options_do_not_select_owned_execution() {
        for args in [
            vec!["off".into()],
            vec!["off".into(), "--help".into()],
            vec!["off".into(), "--wat".into()],
            vec!["off".into(), "--".into(), "--yes".into()],
        ] {
            assert!(!owned_teardown_options(&args));
            assert_eq!(classify_tool_resolution(&args), None);
        }
    }

    #[test]
    fn whole_cli_result_is_send_static() {
        fn assert_closed<T: Send + 'static>() {}
        assert_closed::<OwnedTeardownCliRun>();
        assert_closed::<CliOutcome>();
    }
}
