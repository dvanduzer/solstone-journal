// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Actual joined S3 transport under the original teardown admission and budget.

use std::collections::BTreeSet;

use crate::teardown::owned::{
    RemoteTeardownPhase, TeardownOperation, TeardownProtocolRefusal, TeardownRemoteRefusal,
};
use crate::teardown_admission::{CarrierTeardownAdmission, TeardownReader};

use super::{DELETE_OBJECT_BATCH_SIZE, S3Credentials, Signer, base64, strict};

impl From<strict::ProtocolError> for TeardownRemoteRefusal {
    fn from(error: strict::ProtocolError) -> Self {
        use strict::ProtocolError as Error;
        Self::Protocol(match error {
            Error::Malformed => TeardownProtocolRefusal::Malformed,
            Error::Limit => TeardownProtocolRefusal::Limit,
            Error::Namespace => TeardownProtocolRefusal::Namespace,
            Error::Pagination => TeardownProtocolRefusal::Pagination,
            Error::Acknowledgment => TeardownProtocolRefusal::Acknowledgment,
        })
    }
}

struct OwnedClient<'a, 'reader> {
    operation: &'a mut TeardownOperation,
    admission: &'a CarrierTeardownAdmission,
    reader: &'a mut TeardownReader<'reader>,
    credentials: &'a S3Credentials,
    limit: strict::InventoryLimit,
}

impl OwnedClient<'_, '_> {
    fn request(
        &self,
        method: &str,
        key: Option<&str>,
        query: Vec<(String, String)>,
        body: Vec<u8>,
        extra: Vec<(String, String)>,
    ) -> Result<Vec<u8>, TeardownRemoteRefusal> {
        self.operation
            .remaining()
            .map_err(TeardownRemoteRefusal::Operation)?;
        self.admission
            .revalidate()
            .map_err(TeardownRemoteRefusal::Admission)?;
        let binding = self
            .admission
            .configuration()
            .binding()
            .ok_or(TeardownRemoteRefusal::BindingUnavailable)?;
        // Signing has no transport; execution always uses the bounded joined entry.
        let date = self.operation.date()?;
        let signer = Signer {
            credentials: self.credentials,
            amz_date: &date,
        };
        let request = signer.signed_request(method, &binding.bucket, key, query, body, extra);
        let response = self.operation.request(request);
        self.admission
            .revalidate()
            .map_err(TeardownRemoteRefusal::Admission)?;
        let response = response?;
        if !(200..300).contains(&response.status) {
            return Err(TeardownRemoteRefusal::HttpStatus(response.status));
        }
        Ok(response.body)
    }

    fn objects(&mut self) -> Result<Vec<String>, TeardownRemoteRefusal> {
        let prefix = &self
            .admission
            .configuration()
            .binding()
            .ok_or(TeardownRemoteRefusal::BindingUnavailable)?
            .prefix;
        let mut keys = BTreeSet::new();
        let mut tokens = BTreeSet::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".into(), "2".into()),
                ("prefix".into(), prefix.clone()),
            ];
            if let Some(token) = &continuation {
                query.push(("continuation-token".into(), token.clone()));
            }
            let body = self.request("GET", None, query, vec![], vec![])?;
            let page = strict::objects(&body, prefix)?;
            for key in page.keys {
                self.limit.add(&[&key])?;
                if !keys.insert(key) {
                    return Err(strict::ProtocolError::Pagination.into());
                }
            }
            match page.next {
                Some(token) => {
                    self.limit.add(&[&token])?;
                    if !tokens.insert(token.clone()) {
                        return Err(strict::ProtocolError::Pagination.into());
                    }
                    continuation = Some(token);
                }
                None => return Ok(keys.into_iter().collect()),
            }
        }
    }

    fn uploads(&mut self) -> Result<Vec<(String, String)>, TeardownRemoteRefusal> {
        let prefix = &self
            .admission
            .configuration()
            .binding()
            .ok_or(TeardownRemoteRefusal::BindingUnavailable)?
            .prefix;
        let mut uploads = BTreeSet::new();
        let mut markers = BTreeSet::new();
        let mut current: Option<(String, String)> = None;
        loop {
            let mut query = vec![
                ("uploads".into(), "".into()),
                ("prefix".into(), prefix.clone()),
            ];
            if let Some((key, id)) = &current {
                query.push(("key-marker".into(), key.clone()));
                query.push(("upload-id-marker".into(), id.clone()));
            }
            let body = self.request("GET", None, query, vec![], vec![])?;
            let page = strict::uploads(&body, prefix)?;
            for (key, id) in page.uploads {
                self.limit.add(&[&key, &id])?;
                if !uploads.insert((key, id)) {
                    return Err(strict::ProtocolError::Pagination.into());
                }
            }
            match page.next {
                Some(marker) => {
                    self.limit.add(&[&marker.0, &marker.1])?;
                    if !marker.0.starts_with(prefix) || !markers.insert(marker.clone()) {
                        return Err(strict::ProtocolError::Pagination.into());
                    }
                    current = Some(marker);
                }
                None => return Ok(uploads.into_iter().collect()),
            }
        }
    }

    fn before_delete(&mut self) -> Result<(), TeardownRemoteRefusal> {
        self.operation
            .remaining()
            .map_err(TeardownRemoteRefusal::Operation)?;
        self.admission
            .check(self.reader)
            .map_err(TeardownRemoteRefusal::Admission)?;
        self.operation
            .remaining()
            .map_err(TeardownRemoteRefusal::Operation)?;
        Ok(())
    }
}

pub(crate) fn wipe(
    operation: &mut TeardownOperation,
    admission: &CarrierTeardownAdmission,
    reader: &mut TeardownReader<'_>,
    credentials: &S3Credentials,
) -> Result<(), TeardownRemoteRefusal> {
    let uri = credentials
        .endpoint
        .parse::<ureq::http::Uri>()
        .map_err(|_| TeardownRemoteRefusal::Protocol(TeardownProtocolRefusal::Namespace))?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
        || !matches!(uri.path(), "" | "/")
        || uri.query().is_some()
    {
        return Err(TeardownRemoteRefusal::Protocol(
            TeardownProtocolRefusal::Namespace,
        ));
    }
    let mut client = OwnedClient {
        operation,
        admission,
        reader,
        credentials,
        limit: strict::InventoryLimit::default(),
    };
    client.operation.report_mut().phase = RemoteTeardownPhase::Inventory;
    let keys = client.objects()?;
    for batch in keys.chunks(DELETE_OBJECT_BATCH_SIZE) {
        let body = strict::delete_body(batch)?;
        let md5 = base64(&md5::compute(&body).0);
        client.before_delete()?;
        let report = client.operation.report_mut();
        report.phase = RemoteTeardownPhase::Deleting;
        report.delete_attempts += 1;
        let response = client.request(
            "POST",
            None,
            vec![("delete".into(), "".into())],
            body,
            vec![("Content-MD5".into(), md5)],
        )?;
        strict::deleted(&response, batch)?;
        client.operation.report_mut().delete_confirmations += 1;
    }
    let uploads = client.uploads()?;
    for (key, id) in uploads {
        client.before_delete()?;
        let report = client.operation.report_mut();
        report.phase = RemoteTeardownPhase::Aborting;
        report.abort_attempts += 1;
        client.request(
            "DELETE",
            Some(&key),
            vec![("uploadId".into(), id)],
            vec![],
            vec![],
        )?;
        client.operation.report_mut().abort_confirmations += 1;
    }
    client.operation.report_mut().phase = RemoteTeardownPhase::Confirming;
    if !client.objects()?.is_empty() {
        return Err(TeardownRemoteRefusal::Residual);
    }
    client.operation.report_mut().objects_empty = true;
    if !client.uploads()?.is_empty() {
        return Err(TeardownRemoteRefusal::Residual);
    }
    client.operation.report_mut().uploads_empty = true;
    client.before_delete()?;
    Ok(())
}
