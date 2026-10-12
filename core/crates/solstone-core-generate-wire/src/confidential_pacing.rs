// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! A journal's older confidential calls share one slot and leave the engine rest.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use solstone_core_journal_config::work_priority::{WorkBand, WorkPriority};
use solstone_core_local::admission::acquire_local_slot;

use crate::{ConfidentialResult, EndpointFailure};

fn rest_after(elapsed: Duration, failed: bool) -> Duration {
    elapsed.max(Duration::from_secs(if failed { 60 } else { 1 }))
}

fn failure(detail: impl Into<String>) -> ConfidentialResult {
    ConfidentialResult::Failed(EndpointFailure {
        reason_code: Some("local_capacity_exhausted".into()),
        detail: Some(detail.into()),
    })
}

fn now_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| format!("confidential background clock unavailable: {error}"))
}

pub(crate) fn run(
    journal: &Path,
    priority: WorkPriority,
    timeout: Duration,
    generate: impl FnOnce(Duration) -> ConfidentialResult,
) -> ConfidentialResult {
    if priority.band < WorkBand::Import {
        return generate(timeout);
    }
    let waiting = Instant::now();
    let root = journal.join("health/confidential-background");
    // This is a separate slot pool. Foreground calls never wait on it.
    let _permit = match acquire_local_slot(&root, 1, Some(timeout), false, priority) {
        Ok(permit) => permit,
        Err(error) => return failure(format!("confidential background admission: {error:?}")),
    };
    let next_call = root.join("next-call.json");
    let next_ms: u64 = match std::fs::read(&next_call) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                return failure(format!("confidential background pace unreadable: {error}"));
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return failure(format!("confidential background pace unreadable: {error}")),
    };
    let now = match now_ms() {
        Ok(now) => now,
        Err(error) => return failure(error),
    };
    let rest = Duration::from_millis(next_ms.saturating_sub(now));
    let Some(_) = timeout.checked_sub(waiting.elapsed()).filter(|v| *v > rest) else {
        return failure("confidential background wait exhausted the request timeout");
    };
    if !rest.is_zero() {
        std::thread::sleep(rest);
    }
    let Some(remaining) = timeout
        .checked_sub(waiting.elapsed())
        .filter(|v| !v.is_zero())
    else {
        return failure("confidential background wait exhausted the request timeout");
    };
    let started = Instant::now();
    let result = generate(remaining);
    let delay = rest_after(
        started.elapsed(),
        !matches!(result, ConfidentialResult::Generated(_)),
    );
    let next = match now_ms() {
        Ok(now) => now.saturating_add(delay.as_millis() as u64),
        Err(error) => return failure(error),
    };
    // Keep the cooldown across worker exits. The slot protects its read and write;
    // atomic publication keeps an interrupted write from stranding older work.
    if let Err(error) = solstone_core_journal_io::write_text(
        &next_call,
        &next.to_string(),
        solstone_core_journal_io::AtomicWriteOptions::default(),
    ) {
        return failure(format!(
            "confidential background pace could not be saved: {error}"
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_calls_rest_in_proportion_to_latency_and_back_off_on_failure() {
        assert_eq!(
            rest_after(Duration::from_millis(20), false),
            Duration::from_secs(1)
        );
        assert_eq!(
            rest_after(Duration::from_secs(17), false),
            Duration::from_secs(17)
        );
        assert_eq!(
            rest_after(Duration::from_secs(2), true),
            Duration::from_secs(60)
        );
        assert_eq!(
            rest_after(Duration::from_secs(90), true),
            Duration::from_secs(90)
        );
    }
}
