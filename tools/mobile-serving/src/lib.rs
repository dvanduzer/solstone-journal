// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! App-owned orchestration of the production router and authenticated device door.

use axum::{
    body::{Body, to_bytes},
    http::{Request, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use solstone_core_convey_shell::{
    EmbeddedConveyHandle, bind_embedded_device_door, bind_embedded_device_door_with_payload,
};
use solstone_core_journal_io::JournalRoot;
use std::{net::SocketAddrV4, path::PathBuf};

pub struct OwnerResponse {
    pub status: u16,
    pub body: String,
}

/// Retain this value in the app's ordinary worker, alongside its private root.
/// Owner requests must be admitted by private in-process or app-owned IPC.
pub struct MobileServing {
    handle: EmbeddedConveyHandle,
    runtime: tokio::runtime::Runtime,
    root: JournalRoot,
}

impl MobileServing {
    pub fn start(root: PathBuf, address: SocketAddrV4) -> Result<Self, String> {
        Self::start_using(root, address, None)
    }

    pub fn start_with_payload(
        root: PathBuf,
        address: SocketAddrV4,
        payload_root: PathBuf,
    ) -> Result<Self, String> {
        Self::start_using(root, address, Some(payload_root))
    }

    fn start_using(
        root: PathBuf,
        address: SocketAddrV4,
        payload_root: Option<PathBuf>,
    ) -> Result<Self, String> {
        if address.ip().is_unspecified() || address.ip().is_multicast() || address.port() == 0 {
            return Err("an explicit interface address and nonzero port are required".into());
        }
        let root = JournalRoot::open(&root).map_err(|e| e.to_string())?;
        root.revalidate_canonical_binding()
            .map_err(|e| e.to_string())?;
        solstone_core_journal_config_write::persist_direct_door_port(
            root.canonical_path(),
            address.port(),
        )
        .map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let handle = runtime
            .block_on(async {
                match payload_root {
                    Some(payload) => {
                        bind_embedded_device_door_with_payload(
                            root.canonical_path().to_path_buf(),
                            address,
                            payload,
                        )
                        .await
                    }
                    None => {
                        bind_embedded_device_door(root.canonical_path().to_path_buf(), address)
                            .await
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            handle,
            runtime,
            root,
        })
    }

    pub fn status(&self) -> Value {
        json!({"root_current": self.root.revalidate_canonical_binding().is_ok(),
            "door_address": self.handle.live_door_addr().map(|a| a.to_string())})
    }

    /// Dispatch only a request already admitted by the app's private owner boundary.
    pub fn request_owner(
        &self,
        method: &str,
        uri: &str,
        body: &str,
    ) -> Result<OwnerResponse, String> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|e| e.to_string())?;
        let uri: axum::http::Uri = uri
            .parse()
            .map_err(|e: axum::http::uri::InvalidUri| e.to_string())?;
        if uri.scheme().is_some() || uri.authority().is_some() || !uri.path().starts_with('/') {
            return Err("owner request requires an origin-form path".into());
        }
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .map_err(|e| e.to_string())?;
        self.runtime.block_on(async {
            let response = self.handle.request_owner(request).await;
            let status = response.status().as_u16();
            let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .map_err(|e| e.to_string())?;
            let body = String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?;
            Ok(OwnerResponse { status, body })
        })
    }

    pub fn index(&self) -> Result<Value, String> {
        self.root
            .revalidate_canonical_binding()
            .map_err(|e| e.to_string())?;
        let report =
            solstone_core_indexer_store::scan::scan_journal(self.root.canonical_path(), false)
                .map_err(|e| e.to_string())?;
        if report.failed != 0 {
            return Err(format!("index scan failed: {:?}", report.failures));
        }
        Ok(
            json!({"indexed":report.indexed,"removed":report.removed,"skipped":report.skipped,
            "failed":report.failed,"warnings":report.warnings}),
        )
    }

    pub fn shutdown(&self) {
        self.handle.shutdown();
    }
}

impl Drop for MobileServing {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synchronous_app_start_setup_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let payload = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../core/payload");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = match probe.local_addr().unwrap() {
            std::net::SocketAddr::V4(v) => v,
            _ => unreachable!(),
        };
        drop(probe);
        assert!(
            MobileServing::start_with_payload(
                root.path().to_path_buf(),
                address,
                root.path().to_path_buf()
            )
            .is_err()
        );
        let server =
            MobileServing::start_with_payload(root.path().to_path_buf(), address, payload).unwrap();
        assert_eq!(server.status()["door_address"], Value::Null);
        assert_eq!(
            server
                .request_owner("GET", "/init/mark", "")
                .unwrap()
                .status,
            200
        );
        assert_eq!(
            server
                .request_owner("POST", "/init/mark/lock", "{}")
                .unwrap()
                .status,
            200
        );
        let finalized = server
            .request_owner("POST", "/init/finalize", "{}")
            .unwrap();
        assert_eq!(finalized.status, 200, "{}", finalized.body);
        assert_eq!(server.status()["door_address"], address.to_string());
        server.shutdown();
        assert_eq!(server.status()["door_address"], Value::Null);
    }
}
