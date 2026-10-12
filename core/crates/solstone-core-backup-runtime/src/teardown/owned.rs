// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Original teardown budget and joined remote execution. No local finish authority.

use std::sync::Arc;
use std::time::{Duration, Instant};

use solstone_core_backup::archive_admission::{
    AdmissionError, ArchiveBudget, ArchiveCancellation, Clock,
};

use crate::hosted_runtime::{HostedCredsUnavailable, HttpError};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::hosted_runtime::{HttpRequest, HttpResponse};
use crate::teardown_admission::{
    CarrierTeardownAdmission, TeardownAdmissionRefusal, TeardownReader,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteTeardownPhase {
    NotStarted,
    Broker,
    Inventory,
    Deleting,
    Aborting,
    Confirming,
    Wiped,
    BindingSuperseded,
}

/// Attempted requests and confirmed responses are independent observations.
/// This report contains no namespace, credentials, response bytes or local authority.
#[derive(Debug)]
pub struct RemoteTeardownReport {
    pub(crate) phase: RemoteTeardownPhase,
    pub(crate) broker_attempted: bool,
    pub(crate) delete_attempts: u64,
    pub(crate) delete_confirmations: u64,
    pub(crate) abort_attempts: u64,
    pub(crate) abort_confirmations: u64,
    pub(crate) objects_empty: bool,
    pub(crate) uploads_empty: bool,
}

impl RemoteTeardownReport {
    pub fn phase(&self) -> RemoteTeardownPhase {
        self.phase
    }
    pub fn broker_attempted(&self) -> bool {
        self.broker_attempted
    }
    pub fn delete_attempts(&self) -> u64 {
        self.delete_attempts
    }
    pub fn delete_confirmations(&self) -> u64 {
        self.delete_confirmations
    }
    pub fn abort_attempts(&self) -> u64 {
        self.abort_attempts
    }
    pub fn abort_confirmations(&self) -> u64 {
        self.abort_confirmations
    }
    pub fn objects_empty(&self) -> bool {
        self.objects_empty
    }
    pub fn uploads_empty(&self) -> bool {
        self.uploads_empty
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeardownProtocolRefusal {
    Malformed,
    Limit,
    Namespace,
    Pagination,
    Acknowledgment,
}

#[derive(Debug)]
pub enum TeardownRemoteRefusal {
    Unsupported,
    Operation(AdmissionError),
    Admission(TeardownAdmissionRefusal),
    BindingUnavailable,
    Timestamp,
    Broker(HostedCredsUnavailable),
    Http(HttpError),
    HttpStatus(u16),
    Protocol(TeardownProtocolRefusal),
    Residual,
    AlreadyAttempted,
}

/// Returned only by the real joined remote path; descriptive, not a cleanup token.
pub enum RemoteTeardownDisposition {
    Wiped,
    BindingSuperseded,
}

/// Retain this original value outside actor unwind and throughout local completion.
/// Its single budget cannot be renewed by progress, another request or credential fetch.
pub struct TeardownOperation {
    clock: Arc<dyn Clock + Send + Sync>,
    budget: ArchiveBudget,
    cancellation: ArchiveCancellation,
    deadline: Instant,
    report: RemoteTeardownReport,
}

impl TeardownOperation {
    pub fn new(
        clock: Arc<dyn Clock + Send + Sync>,
        cancellation: ArchiveCancellation,
    ) -> Result<Self, AdmissionError> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(super::TEARDOWN_TIMEOUT_SECONDS))
            .ok_or(AdmissionError::DeadlineExceeded)?;
        let mut budget = ArchiveBudget::new_cancellable(clock.as_ref(), cancellation.clone())?;
        let now = clock.now_unix();
        budget.remaining(clock.as_ref())?;
        budget.cap_credential_expiry(
            now.checked_add(super::TEARDOWN_TIMEOUT_SECONDS as i64)
                .ok_or(AdmissionError::DeadlineExceeded)?,
        );
        Ok(Self {
            clock,
            budget,
            cancellation,
            deadline,
            report: RemoteTeardownReport {
                phase: RemoteTeardownPhase::NotStarted,
                broker_attempted: false,
                delete_attempts: 0,
                delete_confirmations: 0,
                abort_attempts: 0,
                abort_confirmations: 0,
                objects_empty: false,
                uploads_empty: false,
            },
        })
    }

    pub fn report(&self) -> &RemoteTeardownReport {
        &self.report
    }

    /// Close operation resources and move the original observations unchanged.
    /// This grants no local completion or cleanup authority.
    pub fn into_report(self) -> RemoteTeardownReport {
        self.report
    }
    pub fn cancellation(&self) -> &ArchiveCancellation {
        &self.cancellation
    }

    pub fn remaining(&self) -> Result<Duration, AdmissionError> {
        let wall = self.budget.remaining(self.clock.as_ref())?;
        let monotonic = self.deadline.saturating_duration_since(Instant::now());
        if monotonic.is_zero() {
            return Err(AdmissionError::DeadlineExceeded);
        }
        Ok(wall.min(monotonic))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn date(&self) -> Result<String, TeardownRemoteRefusal> {
        self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
        let date = chrono::DateTime::<chrono::Utc>::from_timestamp(self.clock.now_unix(), 0)
            .ok_or(TeardownRemoteRefusal::Timestamp)?;
        self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
        Ok(date.format("%Y%m%dT%H%M%SZ").to_string())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn request(
        &self,
        mut request: HttpRequest,
    ) -> Result<HttpResponse, TeardownRemoteRefusal> {
        request.timeout = request
            .timeout
            .min(self.remaining().map_err(TeardownRemoteRefusal::Operation)?);
        let response = crate::controlled_http::execute_teardown(&request, &self.cancellation);
        // The actual worker and resolver have joined before any result is inspected.
        self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
        response.map_err(TeardownRemoteRefusal::Http)
    }

    /// Run once under the actual retained admission. The installed coordinator supplies
    /// the sole fresh Catalog/raw reader. No request transport or settlement callback
    /// can be injected here, and successful remote work grants no local finish.
    pub fn run_remote(
        &mut self,
        admission: &CarrierTeardownAdmission,
        reader: &mut TeardownReader<'_>,
        version: &str,
    ) -> Result<RemoteTeardownDisposition, TeardownRemoteRefusal> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use crate::hosted_runtime::{decode_hosted_credentials, hosted_credentials_request};
            if self.report.phase != RemoteTeardownPhase::NotStarted {
                return Err(TeardownRemoteRefusal::AlreadyAttempted);
            }
            self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
            admission
                .check(reader)
                .map_err(TeardownRemoteRefusal::Admission)?;
            let binding = admission
                .configuration()
                .binding()
                .ok_or(TeardownRemoteRefusal::BindingUnavailable)?;
            let request = hosted_credentials_request(binding, "maintenance", version);
            self.report.phase = RemoteTeardownPhase::Broker;
            self.report.broker_attempted = true;
            let response = self.request(request)?;
            admission
                .revalidate()
                .map_err(TeardownRemoteRefusal::Admission)?;
            let credentials = match decode_hosted_credentials(&response) {
                Ok(credentials) => credentials,
                Err(refusal) if refusal.reason_code == "binding_superseded" => {
                    self.report.phase = RemoteTeardownPhase::BindingSuperseded;
                    return Ok(RemoteTeardownDisposition::BindingSuperseded);
                }
                Err(refusal) => return Err(TeardownRemoteRefusal::Broker(refusal)),
            };
            let expiry = chrono::DateTime::parse_from_rfc3339(&credentials.expires_at)
                .map_err(|_| TeardownRemoteRefusal::Timestamp)?
                .timestamp();
            self.budget.cap_credential_expiry(expiry);
            self.remaining().map_err(TeardownRemoteRefusal::Operation)?;
            admission
                .check(reader)
                .map_err(TeardownRemoteRefusal::Admission)?;
            let s3 = crate::s3_wipe::S3Credentials {
                endpoint: credentials.endpoint,
                access_key_id: credentials.access_key_id,
                secret_access_key: credentials.secret_access_key,
                session_token: credentials.session_token,
                region: "auto".into(),
            };
            crate::s3_wipe::owned::wipe(self, admission, reader, &s3)?;
            self.report.phase = RemoteTeardownPhase::Wiped;
            Ok(RemoteTeardownDisposition::Wiped)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (admission, reader, version);
            Err(TeardownRemoteRefusal::Unsupported)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn report_mut(&mut self) -> &mut RemoteTeardownReport {
        &mut self.report
    }
}

#[cfg(test)]
mod tests;
