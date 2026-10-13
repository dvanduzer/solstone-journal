// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Callosum wire envelopes and their durable per-segment event log.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod local_inference;
mod model;
mod oneshot;
mod reader;
mod registry;
#[cfg(any(test, windows))]
pub mod windows;
#[cfg(feature = "wire")]
mod wire;
mod writer;

// iOS container paths can exceed sockaddr_un even when the socket's app-owned
// target is short. Following a final socket symlink preserves Unix path semantics.
#[cfg(unix)]
fn endpoint_path(path: &std::path::Path) -> std::io::Result<std::borrow::Cow<'_, std::path::Path>> {
    #[cfg(target_os = "ios")]
    if path.as_os_str().len() >= 104 {
        return short_socket_alias(path).map(std::borrow::Cow::Owned);
    }
    Ok(std::borrow::Cow::Borrowed(path))
}

#[cfg(any(target_os = "ios", all(unix, test, feature = "full-tests")))]
fn short_socket_alias(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let target = std::fs::read_link(path)?;
    if !target.is_absolute() || target.as_os_str().len() >= 104 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket alias requires a short absolute target",
        ));
    }
    Ok(target)
}

#[cfg(all(unix, test, feature = "full-tests"))]
#[test]
fn long_socket_alias_reaches_the_same_unix_endpoint() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let physical = root.path().join("s");
    let directory = root.path().join("long".repeat(32));
    std::fs::create_dir(&directory).unwrap();
    let logical = directory.join("callosum.sock");
    std::os::unix::fs::symlink(&physical, &logical).unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(short_socket_alias(&logical).unwrap()).unwrap();
    let _client =
        std::os::unix::net::UnixStream::connect(short_socket_alias(&logical).unwrap()).unwrap();
    let _peer = listener.accept().unwrap();
    std::fs::remove_file(&logical).unwrap();
    std::os::unix::fs::symlink("relative.sock", &logical).unwrap();
    assert!(short_socket_alias(&logical).is_err());
}

pub use local_inference::LOCAL_INFERENCE_TOKEN_MAX;
#[cfg(feature = "wire")]
pub use local_inference::{
    LocalInferenceReadError, LocalInferenceSnapshot, LocalInferenceSnapshotOffer,
};
pub use model::{CallosumEnvelope, DeviceIngestEvent, DurableEvent, FileDescriptor, ReportedZone};
pub use oneshot::{CallosumOneShotError, CallosumOneShotSender};
pub use reader::{
    CallosumReadError, DeviceIngestReport, DurableEventsReport, read_device_ingest_events,
    read_durable_events, read_reported_zone,
};
pub use registry::callosum_registry;
#[cfg(feature = "full-tests")]
#[doc(hidden)]
pub use wire::one_shot_fixture as test_fixture;
#[cfg(all(feature = "wire", any(test, feature = "test-hooks")))]
#[doc(hidden)]
pub use wire::test_support;
#[cfg(feature = "wire")]
pub use wire::{
    CallosumConnectionPhase, CallosumGapReason, CallosumReceiveEvent, CallosumRetrySource,
    CallosumSocketConnection, CallosumSocketServer, CallosumSocketServerError,
    CallosumStoppedReason, TokioRetrySource, request_local_inference_snapshot,
    request_local_inference_snapshot_sync,
};
pub use writer::{CallosumWriteError, append_durable_event};
