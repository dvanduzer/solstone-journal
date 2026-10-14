// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! App-owned orchestration of the production router and authenticated device door.

use axum::{
    body::{Body, to_bytes},
    http::{Request, header::CONTENT_TYPE},
};
use serde_json::{Value, json};
use solstone_core_convey_shell::{EmbeddedConveyHandle, bind_embedded_device_door_with_routes};
use solstone_core_journal_io::JournalRoot;
use std::{
    net::SocketAddrV4,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

mod brain;

#[cfg(feature = "audio")]
mod audio;

pub struct OwnerResponse {
    pub status: u16,
    pub body: String,
}

/// Retain this value in the app's ordinary worker, alongside its private root.
/// Owner requests must be admitted by private in-process or app-owned IPC.
pub struct MobileServing {
    #[cfg(feature = "audio")]
    audio: Option<audio::AudioWorker>,
    bus: solstone_core_callosum::CallosumSocketServer,
    handle: EmbeddedConveyHandle,
    runtime: tokio::runtime::Runtime,
    root: Arc<JournalRoot>,
    services: Mutex<Option<AppServices>>,
}

struct AppServices {
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
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
        let root = Arc::new(JournalRoot::open(&root).map_err(|e| e.to_string())?);
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
        // Keep the production ingest notification boundary live. Durable receipt
        // discovery provides restart recovery independently of this transient bus.
        let bus = runtime
            .block_on(solstone_core_callosum::CallosumSocketServer::bind(
                root.canonical_path().join("health/callosum.sock"),
            ))
            .map_err(|e| e.to_string())?;
        let handle = runtime
            .block_on(async {
                bind_embedded_device_door_with_routes(
                    root.canonical_path().to_path_buf(),
                    address,
                    payload_root,
                    solstone_core_mcp_endpoint::owner_routes(root.canonical_path().to_path_buf()),
                )
                .await
            })
            .map_err(|e| e.to_string())?;
        let (shutdown, mut stop) = watch::channel(false);
        let (ready, registered) = oneshot::channel();
        let brain_task = runtime.spawn(brain::run(Arc::clone(&root), stop.clone(), ready));
        let brain_ready = runtime.block_on(async {
            matches!(
                tokio::time::timeout(Duration::from_secs(5), registered).await,
                Ok(Ok(()))
            )
        });
        if !brain_ready {
            let _ = shutdown.send(true);
            runtime.block_on(async {
                let _ = brain_task.await;
                handle.shutdown_and_wait().await;
                bus.stop().await;
            });
            return Err("embedded Brain consumer did not register".into());
        }
        let mcp_root = root.canonical_path().to_path_buf();
        let mut mcp_stop = stop.clone();
        let mcp_task = runtime.spawn(async move {
            loop {
                if *mcp_stop.borrow() {
                    break;
                }
                let result = solstone_core_mcp_endpoint::run_embedded_service(
                    mcp_root.clone(),
                    mcp_stop.clone(),
                )
                .await;
                if *mcp_stop.borrow() {
                    break;
                }
                if let Err(error) = result {
                    log::warn!("embedded agent service stopped: {}", error.class());
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                    _ = mcp_stop.changed() => break,
                }
            }
        });
        let relay_root = root.canonical_path().to_path_buf();
        let relay_task = runtime.spawn(async move {
            loop {
                if *stop.borrow() {
                    break;
                }
                let result = solstone_core_spl::run_embedded_service(
                    relay_root.clone(),
                    solstone_core_spl::Verbosity::Quiet,
                    stop.clone(),
                )
                .await;
                if *stop.borrow() {
                    break;
                }
                if let Err(error) = result {
                    log::warn!("embedded relay service stopped: {}", error.class());
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                    _ = stop.changed() => break,
                }
            }
        });
        Ok(Self {
            #[cfg(feature = "audio")]
            audio: None,
            bus,
            handle,
            runtime,
            root,
            services: Mutex::new(Some(AppServices {
                shutdown,
                tasks: vec![brain_task, relay_task, mcp_task],
            })),
        })
    }

    pub fn status(&self) -> Value {
        #[allow(unused_mut)]
        let mut value = json!({"root_current": self.root.revalidate_canonical_binding().is_ok(),
            "door_address": self.handle.live_door_addr().map(|a| a.to_string())});
        #[cfg(feature = "audio")]
        {
            value["audio"] = self
                .audio
                .as_ref()
                .map_or(json!({"state":"disabled"}), |a| a.status());
        }
        value
    }

    /// Enable the dedicated foreground app's local processing pass. Successful
    /// input never needs a private command to trigger transcription.
    #[cfg(feature = "audio")]
    pub fn enable_audio(
        &mut self,
        models: PathBuf,
        model: PathBuf,
        scratch: PathBuf,
    ) -> Result<(), String> {
        if self.audio.is_some() {
            return Err("audio worker already enabled".into());
        }
        self.audio = Some(audio::AudioWorker::start(
            self.root.canonical_path().to_path_buf(),
            models,
            model,
            scratch,
        )?);
        Ok(())
    }

    #[cfg(feature = "audio")]
    pub fn cancel_audio(&self) -> Result<(), String> {
        self.audio.as_ref().ok_or("audio worker disabled")?.cancel();
        Ok(())
    }

    #[cfg(feature = "audio")]
    pub fn resume_audio(&self) -> Result<(), String> {
        self.audio.as_ref().ok_or("audio worker disabled")?.resume();
        Ok(())
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
        #[cfg(feature = "audio")]
        if let Some(audio) = &self.audio {
            audio.stop();
        }
        // Keep retirement serialized through the final bus stop. A concurrent
        // caller must not observe the empty slot as proof of a completed stop.
        let mut service_owner = self.services.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(services) = service_owner.take() {
            let _ = services.shutdown.send(true);
            self.runtime.block_on(async {
                for task in services.tasks {
                    let _ = task.await;
                }
            });
        }
        self.runtime.block_on(self.handle.shutdown_and_wait());
        self.runtime.block_on(self.bus.stop());
        drop(service_owner);
    }
}

impl Drop for MobileServing {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(test, feature = "full-tests"))]
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
