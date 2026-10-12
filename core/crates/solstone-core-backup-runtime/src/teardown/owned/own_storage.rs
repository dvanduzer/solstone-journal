// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Forget every snapshot in an owner's own restic repository, under the original
//! teardown budget and the same coverage checks as operated storage.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use solstone_core_backup::archive_admission::ArchiveCancellation;

use super::{
    RemoteTeardownDisposition, RemoteTeardownPhase, TeardownOperation, TeardownRemoteRefusal,
};
use crate::runner::{
    ResticResult, ToolOutput, ToolRequest, ToolRunner, reason_for_returncode, run_restic,
};
use crate::teardown_admission::{CarrierTeardownAdmission, TeardownReader};

const RETENTION_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Every restic child honours the teardown's cancellation where the platform can stop it.
struct Cancellable<'a>(&'a ArchiveCancellation);
impl ToolRunner for Cancellable<'_> {
    fn run(&self, request: &ToolRequest<'_>) -> io::Result<ToolOutput> {
        if self.0.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "teardown cancelled",
            ));
        }
        #[cfg(unix)]
        {
            crate::runner::SystemToolRunner.run_cancellable(request, self.0)
        }
        #[cfg(not(unix))]
        {
            crate::runner::SystemToolRunner.run(request)
        }
    }
}

fn snapshot_ids(listing: &ResticResult) -> Option<Vec<String>> {
    let Value::Array(rows) = listing.json.as_ref()? else {
        return None;
    };
    rows.iter()
        .map(|row| {
            row.get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .map(str::to_owned)
        })
        .collect()
}

impl TeardownOperation {
    fn restic(
        &self,
        restic_path: &Path,
        repository: &str,
        password: &str,
        env: &BTreeMap<String, Option<String>>,
        args: &[String],
        json: bool,
    ) -> Result<ResticResult, TeardownRemoteRefusal> {
        let timeout: Duration = self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
        let result = run_restic(
            &Cancellable(&self.cancellation),
            args,
            repository,
            password,
            restic_path,
            Some(env),
            json,
            None,
            Some(timeout),
            &[],
        );
        self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
        let result = result.map_err(|_| TeardownRemoteRefusal::Restic("failed"))?;
        if result.returncode != 0 {
            return Err(TeardownRemoteRefusal::Restic(reason_for_returncode(
                result.returncode,
            )));
        }
        Ok(result)
    }

    /// Operated storage runs the hosted wipe; own storage, with the package's restic,
    /// forgets every snapshot in the owner's repository.
    pub fn run_remote_for(
        &mut self,
        admission: &CarrierTeardownAdmission,
        reader: &mut TeardownReader<'_>,
        version: &str,
        own_storage_restic: Option<&Path>,
    ) -> Result<RemoteTeardownDisposition, TeardownRemoteRefusal> {
        match own_storage_restic {
            Some(restic) => self.run_remote_own_storage(admission, reader, restic),
            None => self.run_remote(admission, reader, version),
        }
    }

    /// List, recheck coverage, forget every snapshot with `--prune`, and confirm
    /// none remain. The repository itself (config and keys) is left in place.
    pub fn run_remote_own_storage(
        &mut self,
        admission: &CarrierTeardownAdmission,
        reader: &mut TeardownReader<'_>,
        restic_path: &Path,
    ) -> Result<RemoteTeardownDisposition, TeardownRemoteRefusal> {
        if self.report.phase != RemoteTeardownPhase::NotStarted {
            return Err(TeardownRemoteRefusal::AlreadyAttempted);
        }
        self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
        admission
            .check(reader)
            .map_err(TeardownRemoteRefusal::Admission)?;
        let configuration = admission.configuration();
        if configuration.binding().is_some() {
            return Err(TeardownRemoteRefusal::DestinationUnavailable);
        }
        let (Some(destination), Some(keys)) = (configuration.destination(), configuration.keys())
        else {
            return Err(TeardownRemoteRefusal::DestinationUnavailable);
        };
        let env = crate::engine::value_env(destination)
            .ok_or(TeardownRemoteRefusal::DestinationUnavailable)?;
        let repository = destination.repository.clone();
        let password = keys.daily_key.clone();
        let list = |operation: &Self| {
            operation
                .restic(
                    restic_path,
                    &repository,
                    &password,
                    &env,
                    &["snapshots".into()],
                    true,
                )
                .and_then(|listing| {
                    snapshot_ids(&listing).ok_or(TeardownRemoteRefusal::Protocol(
                        super::TeardownProtocolRefusal::Malformed,
                    ))
                })
        };
        self.report.phase = RemoteTeardownPhase::Inventory;
        let ids = list(self)?;
        // Read-only and never a refusal: the answer goes to the owner, not the gate.
        let check = self
            .remaining()
            .map_err(TeardownRemoteRefusal::Operation)?
            .min(RETENTION_CHECK_TIMEOUT);
        let amz_date = chrono::DateTime::<chrono::Utc>::from_timestamp(self.clock.now_unix(), 0)
            .ok_or(TeardownRemoteRefusal::Timestamp)?
            .format("%Y%m%dT%H%M%SZ")
            .to_string();
        self.report.storage_retention = Some(super::retention::observe(
            destination,
            &crate::hosted_runtime::UreqHttpTransport,
            &self.cancellation,
            check,
            &amz_date,
        ));
        self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
        admission
            .check(reader)
            .map_err(TeardownRemoteRefusal::Admission)?;
        if !ids.is_empty() {
            self.report.phase = RemoteTeardownPhase::Deleting;
            self.report.delete_attempts += 1;
            let mut args = vec!["forget".to_owned()];
            args.extend(ids);
            args.push("--prune".into());
            self.restic(restic_path, &repository, &password, &env, &args, false)?;
            self.report.delete_confirmations += 1;
            admission
                .check(reader)
                .map_err(TeardownRemoteRefusal::Admission)?;
        }
        self.report.phase = RemoteTeardownPhase::Confirming;
        if !list(self)?.is_empty() {
            return Err(TeardownRemoteRefusal::Residual);
        }
        self.report.objects_empty = true;
        self.report.uploads_empty = true;
        self.report.phase = RemoteTeardownPhase::Wiped;
        Ok(RemoteTeardownDisposition::Wiped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(json: &str) -> ResticResult {
        ResticResult {
            returncode: 0,
            stdout: json.into(),
            stderr: String::new(),
            json: serde_json::from_str(json).ok(),
            argv: vec![],
        }
    }

    #[test]
    fn snapshot_listing_accepts_only_hex_ids() {
        assert_eq!(
            snapshot_ids(&listing(r#"[{"id":"ab12"},{"id":"CD34"}]"#)),
            Some(vec!["ab12".into(), "CD34".into()])
        );
        assert_eq!(snapshot_ids(&listing("[]")), Some(vec![]));
        for malformed in [
            r#"[{"id":"--prune"}]"#,
            r#"[{"id":""}]"#,
            r#"[{}]"#,
            r#"{}"#,
            "nope",
        ] {
            assert_eq!(snapshot_ids(&listing(malformed)), None, "{malformed}");
        }
    }
}
