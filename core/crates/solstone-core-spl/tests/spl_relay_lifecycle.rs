// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Real-boundary SPL relay listen/enroll tests that bind loopback TCP.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use solstone_core_sol_link::service_identity::{
    load_or_create_service_identity, load_service_identity_ca,
};
use solstone_core_spl::ServiceTokenHealth;
use solstone_core_spl::service_token_renewal::{
    Outcome, RenewalClock, RenewalConfig, RenewalState, RenewalStep, Trigger, evaluate_once,
};
use solstone_core_spl::{
    CallosumEmit, EnrollError, EnrollMode, JournalAttemptTokenSource, LoopbackConnect,
    LoopbackDialer, PairWindowClientError, PairWindowSecret, PostureGate, PostureInput,
    RelayClient, RelayClientConfig, RelayDecision, RelayWebSocket, ServiceToken,
    StaticAttemptTokenSource, TokenInput, WsByteSink, WsByteSource, attach_pair_window_tunnel,
    enroll_home, register_pair_window, relay_tunnel_url, save_service_token, stop_relay_run,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener as TokioTcpListener, TcpStream},
    sync::{Notify, mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::{
    accept_async, accept_hdr_async, connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::server::Request as ServerRequest,
        http::{HeaderValue, StatusCode, header::AUTHORIZATION},
    },
};

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

fn token() -> Result<ServiceToken, String> {
    let mut gate = PostureGate::new();
    gate.update_posture(PostureInput::Value("spl".to_owned()));
    gate.update_token(TokenInput::Value("service-token".to_owned()));
    match gate.decision() {
        RelayDecision::Allowed(permit) => Ok(permit.token().clone()),
        RelayDecision::Blocked(_) => Err("test token was unexpectedly blocked".to_owned()),
    }
}

async fn relay_response(
    status: u16,
    body: &str,
) -> Result<(String, tokio::task::JoinHandle<Result<(), String>>), String> {
    let listener = TokioTcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "relay listener binds".to_owned())?;
    let address = listener
        .local_addr()
        .map_err(|_| "relay address reads".to_owned())?;
    let body = body.to_owned();
    let worker = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| "relay accepts request".to_owned())?;
        let mut request = [0_u8; 1024];
        let _ = stream
            .read(&mut request)
            .await
            .map_err(|_| "relay reads request".to_owned())?;
        let reason = if status == 200 {
            "OK"
        } else {
            "Service Unavailable"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
            .await
            .map_err(|_| "relay response writes".to_owned())
    });
    Ok((format!("http://{address}"), worker))
}

struct NullEmit;

impl CallosumEmit for NullEmit {
    fn emit(&self, _event: &'static str, _payload: serde_json::Value) {}
}

struct ClosedDialer;

impl LoopbackDialer for ClosedDialer {
    fn connect(&self) -> LoopbackConnect {
        Box::pin(async {
            Err(std::io::Error::other(
                "loopback dialer is unused by this test",
            ))
        })
    }
}

struct ConnectedEmit(Mutex<Option<oneshot::Sender<()>>>);

impl CallosumEmit for ConnectedEmit {
    fn emit(&self, event: &'static str, _payload: serde_json::Value) {
        if event != "connected" {
            return;
        }
        let sender = match self.0.lock() {
            Ok(mut sender) => sender.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn enroll_home_accepts_a_legacy_account_token() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let (relay, worker) = relay_response(200, r#"{"account_token":"legacy-token"}"#).await?;
        let result = tokio::task::spawn_blocking(move || {
            enroll_home(&relay, "instance", "ca", "home", EnrollMode::Establish)
        })
        .await
        .map_err(|_| "enrollment task failed".to_owned())?;
        worker
            .await
            .map_err(|_| "relay worker failed".to_owned())??;
        if result.map_err(|error| error.to_string())? != "legacy-token" {
            return Err("legacy token mapping differed".to_owned());
        }
        Ok(())
    })
    .await
    .map_err(|_| "legacy enrollment fixture timed out".to_owned())?
}

#[tokio::test]
async fn enroll_home_rejects_non_json_http_error_bodies() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let (relay, worker) = relay_response(503, "temporarily unavailable").await?;
        let result = tokio::task::spawn_blocking(move || {
            enroll_home(&relay, "instance", "ca", "home", EnrollMode::Establish)
        })
        .await
        .map_err(|_| "enrollment task failed".to_owned())?;
        worker
            .await
            .map_err(|_| "relay worker failed".to_owned())??;
        if !matches!(
            result,
            Err(EnrollError::Rejected {
                status: 503,
                reason: None
            })
        ) {
            return Err("non-JSON rejection mapping differed".to_owned());
        }
        Ok(())
    })
    .await
    .map_err(|_| "rejected enrollment fixture timed out".to_owned())?
}

#[tokio::test]
async fn split_adapter_preserves_binary_and_text_source_bytes() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "listener bind failed".to_owned())?;
        let address = listener
            .local_addr()
            .map_err(|_| "listener address failed".to_owned())?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|_| "listener accept failed".to_owned())?;
            let mut websocket = accept_async(stream)
                .await
                .map_err(|_| "server upgrade failed".to_owned())?;
            websocket
                .send(Message::Binary(Bytes::from_static(b"binary")))
                .await
                .map_err(|_| "server binary send failed".to_owned())?;
            websocket
                .send(Message::Text("text".into()))
                .await
                .map_err(|_| "server text send failed".to_owned())?;
            let response = websocket
                .next()
                .await
                .ok_or_else(|| "server response ended".to_owned())?
                .map_err(|_| "server response failed".to_owned())?;
            assert_eq!(response, Message::Binary(Bytes::from_static(b"reply")));
            websocket
                .close(None)
                .await
                .map_err(|_| "server close failed".to_owned())
        });

        let token = token()?;
        let endpoint = format!("ws://{address}");
        let url = relay_tunnel_url(&endpoint, "/session/listen", "home-a", token.as_str());
        let websocket = RelayWebSocket::connect(&url, &token)
            .await
            .map_err(|error| error.to_string())?;
        let (mut reader, mut writer) = websocket.split();

        assert_eq!(
            reader
                .next_message()
                .await
                .map_err(|_| "binary source read failed".to_owned())?,
            Some(Bytes::from_static(b"binary"))
        );
        assert_eq!(
            reader
                .next_message()
                .await
                .map_err(|_| "text source read failed".to_owned())?,
            Some(Bytes::from_static(b"text"))
        );
        writer
            .send(Bytes::from_static(b"reply"))
            .await
            .map_err(|_| "sink response failed".to_owned())?;
        server
            .await
            .map_err(|_| "server task failed".to_owned())??;
        Ok(())
    })
    .await
    .map_err(|_| "split-adapter fixture timed out".to_owned())?
}

#[tokio::test]
async fn listen_events_surface_pongs_and_flush_automatic_ping_replies() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "listener bind failed".to_owned())?;
        let address = listener
            .local_addr()
            .map_err(|_| "listener address failed".to_owned())?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|_| "listener accept failed".to_owned())?;
            let mut websocket = accept_async(stream)
                .await
                .map_err(|_| "server upgrade failed".to_owned())?;
            let initial_ping = websocket
                .next()
                .await
                .ok_or_else(|| "client closed before initial Ping".to_owned())?
                .map_err(|_| "initial Ping read failed".to_owned())?;
            let Message::Ping(nonce) = initial_ping else {
                return Err("first listener frame was not Ping".to_owned());
            };
            websocket
                .send(Message::Pong(nonce))
                .await
                .map_err(|_| "listener Pong send failed".to_owned())?;
            websocket
                .send(Message::Ping(Bytes::from_static(b"peer-ping")))
                .await
                .map_err(|_| "server Ping send failed".to_owned())?;
            let reply = websocket
                .next()
                .await
                .ok_or_else(|| "client closed before automatic Pong".to_owned())?
                .map_err(|_| "automatic Pong read failed".to_owned())?;
            if reply != Message::Pong(Bytes::from_static(b"peer-ping")) {
                return Err("client automatic Pong payload differed".to_owned());
            }
            Ok::<(), String>(())
        });

        let (connected_send, connected_receive) = oneshot::channel();
        let service_token = token()?;
        let mut client = RelayClient::new(
            RelayClientConfig {
                instance_id: "home-a".to_owned(),
                relay_endpoint: format!("http://{address}"),
                token_source: Arc::new(StaticAttemptTokenSource::new(service_token.clone())),
                service_token,
                dispatch_read_deadline: Duration::from_secs(1),
                ping_interval: Duration::from_secs(3600),
                ping_ack_timeout: Duration::from_secs(1),
                ack_stability_window: Duration::from_secs(3600),
                global_admission_ceiling: 1,
            },
            Arc::new(ConnectedEmit(Mutex::new(Some(connected_send)))),
            Arc::new(ClosedDialer),
        );
        let running_client = client.clone();
        let run_task = tokio::spawn(async move { running_client.run().await });
        connected_receive
            .await
            .map_err(|_| "listener Pong was not surfaced as connected".to_owned())?;
        server
            .await
            .map_err(|_| "server task failed".to_owned())??;
        stop_relay_run(&mut client, run_task)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    })
    .await
    .map_err(|_| "listen-event fixture timed out".to_owned())?
}

#[derive(Default)]
struct RecordingEmit {
    events: Mutex<Vec<(String, serde_json::Value)>>,
    changed: Notify,
}

impl CallosumEmit for RecordingEmit {
    fn emit(&self, event: &'static str, payload: serde_json::Value) {
        match self.events.lock() {
            Ok(mut events) => events.push((event.to_owned(), payload)),
            Err(poisoned) => poisoned.into_inner().push((event.to_owned(), payload)),
        }
        self.changed.notify_waiters();
    }
}

impl RecordingEmit {
    fn snapshot(&self) -> Vec<(String, serde_json::Value)> {
        match self.events.lock() {
            Ok(events) => events.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    async fn wait_for_count(&self, event: &str, count: usize) {
        loop {
            if self
                .snapshot()
                .iter()
                .filter(|(name, _)| name == event)
                .count()
                >= count
            {
                return;
            }
            self.changed.notified().await;
        }
    }
}

struct TcpDialer(std::net::SocketAddr);

impl LoopbackDialer for TcpDialer {
    fn connect(&self) -> LoopbackConnect {
        let address = self.0;
        Box::pin(async move {
            let stream = TcpStream::connect(address).await?;
            Ok(Box::new(stream) as Box<dyn solstone_core_spl::LoopbackStream>)
        })
    }
}

#[tokio::test]
async fn incoming_offer_dials_loopback_replays_split_tls_prefix_and_emits_exact_tail()
-> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "relay bind failed".to_owned())?;
        let relay_address = relay
            .local_addr()
            .map_err(|_| "relay address failed".to_owned())?;
        let loopback = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "loopback bind failed".to_owned())?;
        let loopback_address = loopback
            .local_addr()
            .map_err(|_| "loopback address failed".to_owned())?;
        let (replayed_send, replayed_receive) = oneshot::channel();
        let relay_task = tokio::spawn(async move {
            let (listen_stream, _) = relay
                .accept()
                .await
                .map_err(|_| "listen accept failed".to_owned())?;
            let mut listen = accept_async(listen_stream)
                .await
                .map_err(|_| "listen upgrade failed".to_owned())?;
            let ping = listen
                .next()
                .await
                .ok_or_else(|| "listen ended before ping".to_owned())?
                .map_err(|_| "ping read failed".to_owned())?;
            let Message::Ping(nonce) = ping else {
                return Err("first listen frame was not Ping".to_owned());
            };
            listen
                .send(Message::Pong(nonce))
                .await
                .map_err(|_| "pong send failed".to_owned())?;
            listen
                .send(Message::Text(
                    r#"{"type":"incoming","tunnel_id":"tls-7"}"#.into(),
                ))
                .await
                .map_err(|_| "offer send failed".to_owned())?;

            let (tunnel_stream, _) = relay
                .accept()
                .await
                .map_err(|_| "tunnel accept failed".to_owned())?;
            let mut tunnel = accept_async(tunnel_stream)
                .await
                .map_err(|_| "tunnel upgrade failed".to_owned())?;
            tunnel
                .send(Message::Binary(Bytes::from_static(&[0x16, 0x03])))
                .await
                .map_err(|_| "first prefix send failed".to_owned())?;
            tunnel
                .send(Message::Binary(Bytes::from_static(&[0x01, 0x00])))
                .await
                .map_err(|_| "second prefix send failed".to_owned())?;
            let (mut local, _) = loopback
                .accept()
                .await
                .map_err(|_| "loopback dial was not observed".to_owned())?;
            let mut replayed = [0_u8; 4];
            local
                .read_exact(&mut replayed)
                .await
                .map_err(|_| "prefix replay read failed".to_owned())?;

            listen
                .send(Message::Text(
                    r#"{"type":"incoming","tunnel_id":"tls-8"}"#.into(),
                ))
                .await
                .map_err(|_| "second offer send failed".to_owned())?;
            let (second_tunnel_stream, _) = relay
                .accept()
                .await
                .map_err(|_| "second tunnel accept failed".to_owned())?;
            let mut second_tunnel = accept_async(second_tunnel_stream)
                .await
                .map_err(|_| "second tunnel upgrade failed".to_owned())?;
            second_tunnel
                .send(Message::Binary(Bytes::from_static(&[0x16, 0x03])))
                .await
                .map_err(|_| "second first-prefix send failed".to_owned())?;
            second_tunnel
                .send(Message::Binary(Bytes::from_static(&[0x01, 0x00])))
                .await
                .map_err(|_| "second second-prefix send failed".to_owned())?;
            let (mut second_local, _) = loopback.accept().await.map_err(|_| {
                "released admission did not permit a second loopback dial".to_owned()
            })?;
            let mut second_replayed = [0_u8; 4];
            second_local
                .read_exact(&mut second_replayed)
                .await
                .map_err(|_| "second prefix replay read failed".to_owned())?;
            let _ = replayed_send.send([replayed, second_replayed]);
            drop(local);
            drop(second_local);
            tunnel
                .close(None)
                .await
                .map_err(|_| "tunnel close failed".to_owned())?;
            second_tunnel
                .close(None)
                .await
                .map_err(|_| "second tunnel close failed".to_owned())?;
            Ok::<(), String>(())
        });

        let emit = Arc::new(RecordingEmit::default());
        let service_token = ServiceToken::new("test-service-token".to_owned());
        let mut client = RelayClient::new(
            RelayClientConfig {
                instance_id: "persisted-instance".to_owned(),
                relay_endpoint: format!("http://{relay_address}"),
                token_source: Arc::new(StaticAttemptTokenSource::new(service_token.clone())),
                service_token,
                dispatch_read_deadline: Duration::from_secs(1),
                ping_interval: Duration::from_secs(3600),
                ping_ack_timeout: Duration::from_secs(1),
                ack_stability_window: Duration::from_secs(3600),
                global_admission_ceiling: 1,
            },
            emit.clone(),
            Arc::new(TcpDialer(loopback_address)),
        );
        let running_client = client.clone();
        let run_task = tokio::spawn(async move { running_client.run().await });
        if replayed_receive
            .await
            .map_err(|_| "prefix replay report dropped".to_owned())?
            != [[0x16, 0x03, 0x01, 0x00], [0x16, 0x03, 0x01, 0x00]]
        {
            return Err("split TLS prefixes were not replayed exactly once".to_owned());
        }
        emit.wait_for_count("tunnel_close", 2).await;
        stop_relay_run(&mut client, run_task)
            .await
            .map_err(|error| error.to_string())?;
        relay_task
            .await
            .map_err(|_| "relay task failed".to_owned())??;

        let events = emit.snapshot();
        let mut closed_ids = events
            .iter()
            .filter_map(|(event, payload)| {
                (event == "tunnel_close")
                    .then(|| payload.get("tunnel_id")?.as_str().map(str::to_owned))?
            })
            .collect::<Vec<_>>();
        closed_ids.sort();
        if closed_ids != ["tls-7", "tls-8"] {
            return Err(format!("tunnel-close identities differed: {closed_ids:?}"));
        }
        let tail = events
            .get(events.len().saturating_sub(4)..)
            .ok_or_else(|| "lifecycle tail was incomplete".to_owned())?;
        if tail
            .iter()
            .map(|(event, _)| event.as_str())
            .collect::<Vec<_>>()
            != ["tunnel_close", "health", "disconnect", "health"]
        {
            return Err(format!("lifecycle tail differed: {tail:?}"));
        }
        if tail[0]
            .1
            .get("tunnel_id")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || tail[1]
                .1
                .get("relay_admission_saturated_count")
                .and_then(serde_json::Value::as_u64)
                != Some(0)
            || tail[3].1.get("state").and_then(serde_json::Value::as_str) != Some("reconnecting")
        {
            return Err("lifecycle tail payload differed or admission was not released".to_owned());
        }
        Ok(())
    })
    .await
    .map_err(|_| "incoming-offer fixture timed out".to_owned())?
}

#[tokio::test]
async fn concrete_service_shutdown_stops_then_cancels_the_live_listen_socket() -> Result<(), String>
{
    timeout(TEST_TIMEOUT, async {
        let relay = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "could not bind fake relay".to_owned())?;
        let address = relay
            .local_addr()
            .map_err(|_| "could not read fake relay address".to_owned())?;
        let (opened_send, opened_receive) = oneshot::channel();
        let (closed_send, closed_receive) = oneshot::channel();
        let relay_task = tokio::spawn(async move {
            let (stream, _) = relay
                .accept()
                .await
                .map_err(|_| "fake relay did not accept listen socket".to_owned())?;
            let mut socket = accept_async(stream)
                .await
                .map_err(|_| "fake relay WebSocket upgrade failed".to_owned())?;
            let _ = opened_send.send(());
            let closed = timeout(std::time::Duration::from_secs(2), socket.next())
                .await
                .map_err(|_| "listen socket stayed open after service cancellation".to_owned())?;
            let observed_close = matches!(closed, None | Some(Err(_)));
            let _ = closed_send.send(observed_close);
            Ok::<(), String>(())
        });

        let service_token = ServiceToken::new("test-service-token".to_owned());
        let mut client = RelayClient::new(
            RelayClientConfig {
                instance_id: "persisted-instance".to_owned(),
                relay_endpoint: format!("http://{address}"),
                token_source: Arc::new(StaticAttemptTokenSource::new(service_token.clone())),
                service_token,
                dispatch_read_deadline: std::time::Duration::from_secs(10),
                ping_interval: std::time::Duration::from_secs(3600),
                ping_ack_timeout: std::time::Duration::from_secs(2),
                ack_stability_window: std::time::Duration::from_secs(3600),
                global_admission_ceiling: 1,
            },
            Arc::new(NullEmit),
            Arc::new(ClosedDialer),
        );
        let running_client = client.clone();
        let run_task = tokio::spawn(async move { running_client.run().await });
        timeout(std::time::Duration::from_secs(2), opened_receive)
            .await
            .map_err(|_| "relay listen socket was never opened".to_owned())?
            .map_err(|_| "fake relay did not report opened socket".to_owned())?;

        stop_relay_run(&mut client, run_task)
            .await
            .map_err(|error| error.to_string())?;
        let closed = timeout(std::time::Duration::from_secs(2), closed_receive)
            .await
            .map_err(|_| "fake relay did not observe socket closure".to_owned())?
            .map_err(|_| "fake relay closure report dropped".to_owned())?;
        if !closed {
            return Err("relay listen socket ended without closing".to_owned());
        }
        relay_task
            .await
            .map_err(|_| "fake relay task failed".to_owned())??;
        Ok(())
    })
    .await
    .map_err(|_| "concrete service-shutdown fixture timed out".to_owned())?
}

struct FakePairRelay {
    endpoint: String,
    state: Arc<Mutex<FakePairRelayState>>,
    changed: Arc<Notify>,
    listener_task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct FakePairRelayState {
    windows: HashMap<String, FakePairRelayWindow>,
    tunnels: HashMap<String, FakePairRelayTunnel>,
    rejected_registrations: usize,
    rejected_tunnel_attaches: usize,
    rejected_pair_dials: usize,
    refuse_next_registration: bool,
    accepted_pair_dials: usize,
}

struct FakePairRelayWindow {
    token: String,
    offers: mpsc::UnboundedSender<String>,
}

struct FakePairRelayTunnel {
    token: String,
    relay_key: String,
    attached: bool,
    bytes_from_home: usize,
}

enum AcceptedPairRelayConnection {
    Registration { relay_key: String, token: String },
    Tunnel { tunnel_id: String },
    PairDial,
}

enum PairRelayEndpoint {
    Registration,
    TunnelAttach,
    PairDial,
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl FakePairRelay {
    async fn bind() -> Result<Self, String> {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "pair relay listener bind failed".to_owned())?;
        let endpoint = format!(
            "http://{}",
            listener
                .local_addr()
                .map_err(|_| "pair relay listener address failed".to_owned())?
        );
        let state = Arc::new(Mutex::new(FakePairRelayState::default()));
        let changed = Arc::new(Notify::new());
        let listener_state = Arc::clone(&state);
        let listener_changed = Arc::clone(&changed);
        let listener_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&listener_state);
                let changed = Arc::clone(&listener_changed);
                std::mem::drop(tokio::spawn(async move {
                    serve_fake_pair_relay_connection(stream, state, changed).await;
                }));
            }
        });
        Ok(Self {
            endpoint,
            state,
            changed,
            listener_task,
        })
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn reject_next_registration(&self) {
        lock(&self.state).refuse_next_registration = true;
    }

    fn open_window_count(&self) -> usize {
        lock(&self.state).windows.len()
    }

    fn rejected_registration_count(&self) -> usize {
        lock(&self.state).rejected_registrations
    }

    fn rejected_tunnel_attach_count(&self) -> usize {
        lock(&self.state).rejected_tunnel_attaches
    }

    fn rejected_pair_dial_count(&self) -> usize {
        lock(&self.state).rejected_pair_dials
    }

    fn accepted_pair_dial_count(&self) -> usize {
        lock(&self.state).accepted_pair_dials
    }

    fn bytes_from_home(&self, tunnel_id: &str) -> Option<usize> {
        lock(&self.state)
            .tunnels
            .get(tunnel_id)
            .map(|tunnel| tunnel.bytes_from_home)
    }

    async fn send_offer(&self, token: &str, tunnel_id: &str) -> Result<(), String> {
        let (relay_key, offers) = loop {
            let notified = self.changed.notified();
            if let Some((relay_key, offers)) = lock(&self.state)
                .windows
                .iter()
                .find(|(_, window)| window.token == token)
                .map(|(relay_key, window)| (relay_key.clone(), window.offers.clone()))
            {
                break (relay_key, offers);
            }
            notified.await;
        };
        {
            let mut state = lock(&self.state);
            if state.tunnels.contains_key(tunnel_id) {
                return Err("pair relay tunnel identifier was reused".to_owned());
            }
            state.tunnels.insert(
                tunnel_id.to_owned(),
                FakePairRelayTunnel {
                    token: token.to_owned(),
                    relay_key,
                    attached: false,
                    bytes_from_home: 0,
                },
            );
        }
        offers
            .send(tunnel_id.to_owned())
            .map_err(|_| "pair relay registration closed before offer".to_owned())
    }
}

impl Drop for FakePairRelay {
    fn drop(&mut self) {
        self.listener_task.abort();
    }
}

#[allow(clippy::result_large_err)]
async fn serve_fake_pair_relay_connection(
    stream: TcpStream,
    state: Arc<Mutex<FakePairRelayState>>,
    changed: Arc<Notify>,
) {
    let accepted = Arc::new(Mutex::new(None));
    let accepted_for_callback = Arc::clone(&accepted);
    let state_for_callback = Arc::clone(&state);
    let websocket = accept_hdr_async(stream, move |request: &ServerRequest, response| {
        match inspect_pair_relay_request(request, &state_for_callback) {
            Ok(connection) => {
                *lock(&accepted_for_callback) = Some(connection);
                Ok(response)
            }
            Err(status) => Err(rejection_response(status)),
        }
    })
    .await;
    let Ok(websocket) = websocket else {
        return;
    };
    let Some(accepted) = lock(&accepted).take() else {
        return;
    };

    match accepted {
        AcceptedPairRelayConnection::Registration { relay_key, token } => {
            let (offers, offer_receiver) = mpsc::unbounded_channel();
            lock(&state)
                .windows
                .insert(relay_key.clone(), FakePairRelayWindow { token, offers });
            changed.notify_waiters();
            serve_fake_pair_relay_registration(websocket, offer_receiver).await;
            lock(&state).windows.remove(&relay_key);
            changed.notify_waiters();
        }
        AcceptedPairRelayConnection::Tunnel { tunnel_id } => {
            serve_fake_pair_relay_tunnel(websocket, state, tunnel_id).await;
        }
        AcceptedPairRelayConnection::PairDial => serve_fake_pair_dial(websocket).await,
    }
}

fn inspect_pair_relay_request(
    request: &ServerRequest,
    state: &Arc<Mutex<FakePairRelayState>>,
) -> Result<AcceptedPairRelayConnection, StatusCode> {
    match request.uri().path() {
        "/session/pair-window" => inspect_pair_window_registration(request, state),
        "/session/pair-dial" => inspect_pair_dial(request, state),
        path if path.starts_with("/tunnel/") => inspect_pair_tunnel_attach(request, state),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

fn inspect_pair_window_registration(
    request: &ServerRequest,
    state: &Arc<Mutex<FakePairRelayState>>,
) -> Result<AcceptedPairRelayConnection, StatusCode> {
    let Some(token) = bearer_token(request) else {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::Registration,
            StatusCode::UNAUTHORIZED,
        );
    };
    let Some(relay_key) = nonempty_header(request, "sec-pair-key") else {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::Registration,
            StatusCode::BAD_REQUEST,
        );
    };
    if request.uri().query().is_some() {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::Registration,
            StatusCode::BAD_REQUEST,
        );
    }
    if std::mem::take(&mut lock(state).refuse_next_registration) {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::Registration,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
    Ok(AcceptedPairRelayConnection::Registration { relay_key, token })
}

fn inspect_pair_tunnel_attach(
    request: &ServerRequest,
    state: &Arc<Mutex<FakePairRelayState>>,
) -> Result<AcceptedPairRelayConnection, StatusCode> {
    let Some(token) = bearer_token(request) else {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::UNAUTHORIZED,
        );
    };
    let Some(tunnel_id) = request.uri().path().strip_prefix("/tunnel/") else {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::NOT_FOUND,
        );
    };
    let Some(relay_key) = nonempty_header(request, "sec-pair-key") else {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::BAD_REQUEST,
        );
    };
    if request.uri().query().is_some() || tunnel_id.is_empty() {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::BAD_REQUEST,
        );
    }

    let mut relay_state = lock(state);
    let Some(tunnel) = relay_state.tunnels.get_mut(tunnel_id) else {
        drop(relay_state);
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::NOT_FOUND,
        );
    };
    if tunnel.token != token {
        drop(relay_state);
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::FORBIDDEN,
        );
    }
    if tunnel.relay_key != relay_key {
        drop(relay_state);
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::FORBIDDEN,
        );
    }
    if tunnel.attached {
        drop(relay_state);
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::TunnelAttach,
            StatusCode::UNAUTHORIZED,
        );
    }
    tunnel.attached = true;
    Ok(AcceptedPairRelayConnection::Tunnel {
        tunnel_id: tunnel_id.to_owned(),
    })
}

fn inspect_pair_dial(
    request: &ServerRequest,
    state: &Arc<Mutex<FakePairRelayState>>,
) -> Result<AcceptedPairRelayConnection, StatusCode> {
    let Some(relay_key) = nonempty_header(request, "sec-pair-key") else {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::PairDial,
            StatusCode::BAD_REQUEST,
        );
    };
    if request.uri().query().is_some() || request.headers().contains_key(AUTHORIZATION) {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::PairDial,
            StatusCode::BAD_REQUEST,
        );
    }
    if !lock(state).windows.contains_key(&relay_key) {
        return reject_pair_relay_request(
            state,
            PairRelayEndpoint::PairDial,
            StatusCode::UNAUTHORIZED,
        );
    }
    lock(state).accepted_pair_dials += 1;
    Ok(AcceptedPairRelayConnection::PairDial)
}

fn reject_pair_relay_request(
    state: &Arc<Mutex<FakePairRelayState>>,
    endpoint: PairRelayEndpoint,
    status: StatusCode,
) -> Result<AcceptedPairRelayConnection, StatusCode> {
    let mut state = lock(state);
    match endpoint {
        PairRelayEndpoint::Registration => state.rejected_registrations += 1,
        PairRelayEndpoint::TunnelAttach => state.rejected_tunnel_attaches += 1,
        PairRelayEndpoint::PairDial => state.rejected_pair_dials += 1,
    }
    Err(status)
}

fn bearer_token(request: &ServerRequest) -> Option<String> {
    let token = request
        .headers()
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    (!token.is_empty()).then(|| token.to_owned())
}

fn nonempty_header(request: &ServerRequest, name: &str) -> Option<String> {
    let value = request.headers().get(name)?.to_str().ok()?;
    (!value.is_empty()).then(|| value.to_owned())
}

fn rejection_response(
    status: StatusCode,
) -> tokio_tungstenite::tungstenite::handshake::server::ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(status)
        .body(Some("pair relay rejected".to_owned()))
        .expect("valid fake relay rejection response")
}

async fn serve_fake_pair_relay_registration(
    mut websocket: tokio_tungstenite::WebSocketStream<TcpStream>,
    mut offers: mpsc::UnboundedReceiver<String>,
) {
    loop {
        tokio::select! {
            Some(tunnel_id) = offers.recv() => {
                if websocket.send(Message::Text(format!(r#"{{"type":"incoming","tunnel_id":"{tunnel_id}"}}"#).into())).await.is_err() {
                    return;
                }
            }
            message = websocket.next() => match message {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(Message::Ping(payload))) => {
                    if websocket.send(Message::Pong(payload)).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Binary(_) | Message::Text(_) | Message::Pong(_) | Message::Frame(_))) => {}
            },
        }
    }
}

async fn serve_fake_pair_relay_tunnel(
    mut websocket: tokio_tungstenite::WebSocketStream<TcpStream>,
    state: Arc<Mutex<FakePairRelayState>>,
    tunnel_id: String,
) {
    while let Some(message) = websocket.next().await {
        match message {
            Ok(Message::Binary(bytes)) => {
                if let Some(tunnel) = lock(&state).tunnels.get_mut(&tunnel_id) {
                    tunnel.bytes_from_home += bytes.len();
                }
                if websocket.send(Message::Binary(bytes)).await.is_err() {
                    return;
                }
            }
            Ok(Message::Ping(payload)) => {
                if websocket.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(Message::Text(_) | Message::Pong(_) | Message::Frame(_)) => {}
        }
    }
}

async fn serve_fake_pair_dial(mut websocket: tokio_tungstenite::WebSocketStream<TcpStream>) {
    while let Some(message) = websocket.next().await {
        match message {
            Ok(Message::Ping(payload)) => {
                if websocket.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(Message::Binary(_) | Message::Text(_) | Message::Pong(_) | Message::Frame(_)) => {}
        }
    }
}

async fn raw_pair_window_registration(
    endpoint: &str,
    relay_key: Option<&str>,
    query: bool,
) -> Result<(), String> {
    let suffix = if query { "?unexpected=value" } else { "" };
    let mut request = format!(
        "{}/session/pair-window{suffix}",
        websocket_endpoint(endpoint)?
    )
    .into_client_request()
    .map_err(|_| "raw registration request construction failed".to_owned())?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer fake-registration-token"),
    );
    if let Some(relay_key) = relay_key {
        let relay_key = HeaderValue::from_str(relay_key)
            .map_err(|_| "raw registration relay key header was invalid".to_owned())?;
        request.headers_mut().insert("sec-pair-key", relay_key);
    }
    let (mut socket, _) = connect_async(request)
        .await
        .map_err(|_| "raw registration was refused".to_owned())?;
    socket
        .close(None)
        .await
        .map_err(|_| "raw registration close failed".to_owned())
}

async fn fake_mobile_pair_dial(endpoint: &str, relay_key: &str) -> Result<(), String> {
    let mut request = format!("{}/session/pair-dial", websocket_endpoint(endpoint)?)
        .into_client_request()
        .map_err(|_| "fake mobile request construction failed".to_owned())?;
    let relay_key = HeaderValue::from_str(relay_key)
        .map_err(|_| "fake mobile relay key header was invalid".to_owned())?;
    request.headers_mut().insert("sec-pair-key", relay_key);
    let (mut socket, _) = connect_async(request)
        .await
        .map_err(|error| format!("fake mobile pair dial was refused: {error}"))?;
    socket
        .close(None)
        .await
        .map_err(|_| "fake mobile pair dial close failed".to_owned())
}

fn websocket_endpoint(endpoint: &str) -> Result<String, String> {
    endpoint
        .strip_prefix("http://")
        .map(|rest| format!("ws://{rest}"))
        .ok_or_else(|| "fake relay endpoint was not HTTP".to_owned())
}

fn pair_window_secret(bytes: [u8; 8]) -> PairWindowSecret {
    PairWindowSecret::from(bytes)
}

fn service_token(value: &str) -> ServiceToken {
    ServiceToken::new(value.to_owned())
}

fn assert_rejected(
    result: Result<solstone_core_spl::PairWindowTunnel, PairWindowClientError>,
    status: u16,
) -> Result<(), String> {
    match result {
        Err(PairWindowClientError::Rejected(actual)) if actual == status => Ok(()),
        Err(error) => Err(format!("pair relay rejection status differed: {error:?}")),
        Ok(_) => Err("pair relay unexpectedly accepted tunnel attach".to_owned()),
    }
}

#[tokio::test]
async fn pair_window_registration_is_header_only_and_refusal_tested() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = FakePairRelay::bind().await?;

        if raw_pair_window_registration(relay.endpoint(), None, false)
            .await
            .is_ok()
        {
            return Err("registration without Sec-Pair-Key was accepted".to_owned());
        }
        if raw_pair_window_registration(relay.endpoint(), Some("relay-key"), true)
            .await
            .is_ok()
        {
            return Err("registration query string was accepted".to_owned());
        }
        if relay.rejected_registration_count() != 2 {
            return Err("fake relay did not record both registration refusals".to_owned());
        }
        let secret = pair_window_secret([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let relay_key = secret.relay_key();
        let registration = register_pair_window(
            relay.endpoint(),
            &service_token("header-only-token"),
            &relay_key,
        )
        .await
        .map_err(|error| format!("valid registration failed: {error}"))?;
        registration
            .close()
            .await
            .map_err(|error| format!("valid registration close failed: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "pair-window registration fixture timed out".to_owned())?
}

#[tokio::test]
async fn pair_window_registration_receives_fake_relay_offer() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = FakePairRelay::bind().await?;
        let secret = pair_window_secret([0x10, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let relay_key = secret.relay_key();
        let token = service_token("offer-token");
        let mut registration = register_pair_window(relay.endpoint(), &token, &relay_key)
            .await
            .map_err(|error| format!("registration failed: {error}"))?;

        relay.send_offer("offer-token", "offer-7").await?;
        let offer = registration
            .next_offer()
            .await
            .map_err(|error| format!("offer read failed: {error}"))?;
        if offer.tunnel_id != "offer-7" {
            return Err("fake relay offer tunnel identifier differed".to_owned());
        }
        registration
            .close()
            .await
            .map_err(|error| format!("registration close failed: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "pair-window offer fixture timed out".to_owned())?
}

#[tokio::test]
async fn pair_window_tunnel_attach_refuses_mismatched_window_credentials_before_bytes()
-> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = FakePairRelay::bind().await?;
        let secret = pair_window_secret([0x20, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let relay_key = secret.relay_key();
        let wrong_relay_key =
            pair_window_secret([0x21, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]).relay_key();
        let token_a = service_token("home-a-token");
        let token_b = service_token("home-b-token");
        let mut registration = register_pair_window(relay.endpoint(), &token_a, &relay_key)
            .await
            .map_err(|error| format!("registration failed: {error}"))?;

        relay.send_offer("home-a-token", "a-only-tunnel").await?;
        let offer = registration
            .next_offer()
            .await
            .map_err(|error| format!("offer read failed: {error}"))?;
        assert_rejected(
            attach_pair_window_tunnel(relay.endpoint(), &offer.tunnel_id, &token_b, &relay_key)
                .await,
            403,
        )?;
        assert_rejected(
            attach_pair_window_tunnel(
                relay.endpoint(),
                &offer.tunnel_id,
                &token_a,
                &wrong_relay_key,
            )
            .await,
            403,
        )?;
        if relay.bytes_from_home(&offer.tunnel_id) != Some(0) {
            return Err("mismatched bearer reached pairing tunnel bytes".to_owned());
        }
        if relay.rejected_tunnel_attach_count() != 2 {
            return Err("fake relay did not record mismatched credential refusals".to_owned());
        }
        registration
            .close()
            .await
            .map_err(|error| format!("registration close failed: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "mismatched tunnel attach fixture timed out".to_owned())?
}

#[tokio::test]
async fn pair_window_tunnel_attach_accepts_matching_window_bearer_and_exchanges_bytes()
-> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = FakePairRelay::bind().await?;
        let secret = pair_window_secret([0x30, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let relay_key = secret.relay_key();
        let token = service_token("matching-home-token");
        let mut registration = register_pair_window(relay.endpoint(), &token, &relay_key)
            .await
            .map_err(|error| format!("registration failed: {error}"))?;

        relay
            .send_offer("matching-home-token", "matching-tunnel")
            .await?;
        let offer = registration
            .next_offer()
            .await
            .map_err(|error| format!("offer read failed: {error}"))?;
        let mut tunnel =
            attach_pair_window_tunnel(relay.endpoint(), &offer.tunnel_id, &token, &relay_key)
                .await
                .map_err(|error| format!("matching tunnel attach failed: {error}"))?;
        tunnel
            .write_all(b"pair-window bytes")
            .await
            .map_err(|_| "pair tunnel write failed".to_owned())?;
        tunnel
            .flush()
            .await
            .map_err(|_| "pair tunnel flush failed".to_owned())?;
        let mut echoed = [0_u8; 17];
        tunnel
            .read_exact(&mut echoed)
            .await
            .map_err(|_| "pair tunnel echo read failed".to_owned())?;
        if echoed != *b"pair-window bytes" {
            return Err("pair tunnel echo bytes differed".to_owned());
        }
        tunnel
            .shutdown()
            .await
            .map_err(|_| "pair tunnel shutdown failed".to_owned())?;
        if relay.bytes_from_home(&offer.tunnel_id) != Some(echoed.len()) {
            return Err("fake relay byte count differed".to_owned());
        }
        registration
            .close()
            .await
            .map_err(|error| format!("registration close failed: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "matching tunnel attach fixture timed out".to_owned())?
}

#[tokio::test]
async fn pair_windows_isolate_secrets_bearers_and_used_tunnels() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = FakePairRelay::bind().await?;
        let secret_a = pair_window_secret([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let secret_b = pair_window_secret([0xf1, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let relay_key_a = secret_a.relay_key();
        let relay_key_b = secret_b.relay_key();
        let token_a = service_token("window-a-token");
        let token_b = service_token("window-b-token");
        let mut registration_a = register_pair_window(relay.endpoint(), &token_a, &relay_key_a)
            .await
            .map_err(|error| format!("window A registration failed: {error}"))?;
        let mut registration_b = register_pair_window(relay.endpoint(), &token_b, &relay_key_b)
            .await
            .map_err(|error| format!("window B registration failed: {error}"))?;

        relay
            .send_offer("window-a-token", "window-a-tunnel")
            .await?;
        relay
            .send_offer("window-b-token", "window-b-tunnel")
            .await?;
        let offer_a = registration_a
            .next_offer()
            .await
            .map_err(|error| format!("window A offer read failed: {error}"))?;
        let offer_b = registration_b
            .next_offer()
            .await
            .map_err(|error| format!("window B offer read failed: {error}"))?;
        if relay.open_window_count() != 2 {
            return Err("fake relay did not retain two independent windows".to_owned());
        }

        assert_rejected(
            attach_pair_window_tunnel(relay.endpoint(), &offer_a.tunnel_id, &token_b, &relay_key_a)
                .await,
            403,
        )?;
        assert_rejected(
            attach_pair_window_tunnel(relay.endpoint(), &offer_b.tunnel_id, &token_a, &relay_key_b)
                .await,
            403,
        )?;

        let mut tunnel_a =
            attach_pair_window_tunnel(relay.endpoint(), &offer_a.tunnel_id, &token_a, &relay_key_a)
                .await
                .map_err(|error| format!("window A matching attach failed: {error}"))?;
        tunnel_a
            .write_all(b"a")
            .await
            .map_err(|_| "window A tunnel write failed".to_owned())?;
        tunnel_a
            .flush()
            .await
            .map_err(|_| "window A tunnel flush failed".to_owned())?;
        let mut echoed = [0_u8; 1];
        tunnel_a
            .read_exact(&mut echoed)
            .await
            .map_err(|_| "window A tunnel echo read failed".to_owned())?;
        tunnel_a
            .shutdown()
            .await
            .map_err(|_| "window A tunnel shutdown failed".to_owned())?;
        assert_rejected(
            attach_pair_window_tunnel(relay.endpoint(), &offer_a.tunnel_id, &token_a, &relay_key_a)
                .await,
            401,
        )?;
        if relay.bytes_from_home(&offer_a.tunnel_id) != Some(1) {
            return Err("window A byte exchange was not isolated".to_owned());
        }
        if relay.bytes_from_home(&offer_b.tunnel_id) != Some(0) {
            return Err("window B received bytes from window A".to_owned());
        }
        if relay.rejected_tunnel_attach_count() != 3 {
            return Err(
                "fake relay did not enforce window isolation and one-use attachment".to_owned(),
            );
        }
        registration_a
            .close()
            .await
            .map_err(|error| format!("window A registration close failed: {error}"))?;
        registration_b
            .close()
            .await
            .map_err(|error| format!("window B registration close failed: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "pair-window isolation fixture timed out".to_owned())?
}

#[tokio::test]
async fn fake_mobile_pair_dial_uses_header_only_pair_key() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = FakePairRelay::bind().await?;
        let secret = pair_window_secret([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let relay_key = secret.relay_key();
        let token = service_token("mobile-pair-token");
        let registration = register_pair_window(relay.endpoint(), &token, &relay_key)
            .await
            .map_err(|error| format!("registration failed: {error}"))?;

        relay
            .send_offer("mobile-pair-token", "mobile-offer")
            .await?;
        fake_mobile_pair_dial(relay.endpoint(), "e34481a4cde647ba9c9fb29a59e18271").await?;
        if relay.accepted_pair_dial_count() != 1 || relay.rejected_pair_dial_count() != 0 {
            return Err(
                "fake mobile pair dial did not use the required header-only shape".to_owned(),
            );
        }
        registration
            .close()
            .await
            .map_err(|error| format!("registration close failed: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|_| "fake mobile pair dial fixture timed out".to_owned())?
}

#[tokio::test]
async fn rejected_pair_window_registration_leaves_no_open_window() -> Result<(), String> {
    timeout(TEST_TIMEOUT, async {
        let relay = FakePairRelay::bind().await?;
        relay.reject_next_registration();
        let secret = pair_window_secret([0x40, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        let relay_key = secret.relay_key();
        let result = register_pair_window(
            relay.endpoint(),
            &service_token("refused-registration-token"),
            &relay_key,
        )
        .await;
        if !matches!(result, Err(PairWindowClientError::Rejected(503))) {
            return Err("refused registration did not return a coarse rejection".to_owned());
        }
        if relay.open_window_count() != 0 || relay.rejected_registration_count() != 1 {
            return Err("refused registration left an offer-listening window live".to_owned());
        }
        Ok(())
    })
    .await
    .map_err(|_| "refused pair-window registration fixture timed out".to_owned())?
}

#[tokio::test]
#[allow(clippy::result_large_err)]
async fn rotated_service_token_is_presented_on_the_next_listen() -> Result<(), String> {
    timeout(Duration::from_secs(8), async {
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let journal_root = std::env::temp_dir().join(format!(
            "solstone-spl-lifecycle-rotate-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&journal_root);
        std::fs::create_dir_all(&journal_root).map_err(|e| e.to_string())?;
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(journal_root.clone());
        save_service_token(&journal_root, "token-1").map_err(|e| e.to_string())?;

        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "listener bind failed".to_owned())?;
        let address = listener
            .local_addr()
            .map_err(|_| "local addr failed".to_owned())?;

        let server_journal = journal_root.clone();
        let (first_seen_send, first_seen_recv) = oneshot::channel();
        let (second_seen_send, second_seen_recv) = oneshot::channel();
        let mut first_send_slot = Some(first_seen_send);
        let mut second_send_slot = Some(second_seen_send);

        let server_task = tokio::spawn(async move {
            let mut attempt = 0;
            while let Ok((stream, _)) = listener.accept().await {
                attempt += 1;
                if attempt == 1 {
                    if let Some(first_seen_send) = first_send_slot.take() {
                        let server_journal = server_journal.clone();
                        let _ = accept_hdr_async(stream, move |req: &ServerRequest, _res| {
                            let auth = req
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|h| h.to_str().ok())
                                .map(str::to_owned);
                            let query = req.uri().query().map(str::to_owned);
                            let _ = first_seen_send.send((auth, query));
                            let _ = save_service_token(&server_journal, "test-service-token");
                            Err(rejection_response(StatusCode::UNAUTHORIZED))
                        })
                        .await;
                    }
                } else if attempt == 2 {
                    if let Some(second_seen_send) = second_send_slot.take() {
                        let ws = accept_hdr_async(stream, move |req: &ServerRequest, res| {
                            let auth = req
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|h| h.to_str().ok())
                                .map(str::to_owned);
                            let query = req.uri().query().map(str::to_owned);
                            let _ = second_seen_send.send((auth, query));
                            Ok(res)
                        })
                        .await;
                        if let Ok(mut socket) = ws {
                            // Keep open until cancelled
                            let _ = socket.next().await;
                        }
                    }
                    break;
                }
            }
            Ok::<(), String>(())
        });

        let mut client = RelayClient::new(
            RelayClientConfig {
                instance_id: "home-a".to_owned(),
                relay_endpoint: format!("http://{address}"),
                token_source: Arc::new(JournalAttemptTokenSource::new(journal_root)),
                service_token: ServiceToken::new(String::new()),
                dispatch_read_deadline: Duration::from_secs(1),
                ping_interval: Duration::from_secs(3600),
                ping_ack_timeout: Duration::from_secs(1),
                ack_stability_window: Duration::from_secs(3600),
                global_admission_ceiling: 1,
            },
            Arc::new(NullEmit),
            Arc::new(ClosedDialer),
        );
        let running_client = client.clone();
        let run_task = tokio::spawn(async move { running_client.run().await });

        let (first_auth, first_query) = first_seen_recv
            .await
            .map_err(|_| "first request not received".to_owned())?;
        assert_eq!(first_auth.as_deref(), Some("Bearer token-1"));
        assert!(
            first_query
                .as_deref()
                .unwrap_or("")
                .contains("token=token-1")
        );

        let (second_auth, second_query) = second_seen_recv
            .await
            .map_err(|_| "second request not received".to_owned())?;
        assert_eq!(second_auth.as_deref(), Some("Bearer test-service-token"));
        assert!(
            second_query
                .as_deref()
                .unwrap_or("")
                .contains("token=test-service-token")
        );

        stop_relay_run(&mut client, run_task)
            .await
            .map_err(|e| e.to_string())?;
        server_task.abort();
        Ok(())
    })
    .await
    .map_err(|_| "rotated token lifecycle test timed out".to_owned())?
}

struct TempJournalCleanup(std::path::PathBuf);

impl Drop for TempJournalCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

static NEXT_HARNESS_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_harness_journal(prefix: &str) -> (std::path::PathBuf, TempJournalCleanup) {
    let id = NEXT_HARNESS_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::path::PathBuf::from("/var/tmp").join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        id
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    let cleanup = TempJournalCleanup(path.clone());
    (path, cleanup)
}

fn write_test_journal_config(
    journal_path: &std::path::Path,
    posture: &str,
    relay_url: Option<&str>,
) {
    let config_dir = journal_path.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let mut link = serde_json::Map::new();
    link.insert(
        "posture".to_owned(),
        serde_json::Value::String(posture.to_owned()),
    );
    if let Some(url) = relay_url {
        link.insert(
            "relay_url".to_owned(),
            serde_json::Value::String(url.to_owned()),
        );
    }
    let mut root = serde_json::Map::new();
    root.insert("link".to_owned(), serde_json::Value::Object(link));
    std::fs::write(
        config_dir.join("journal.json"),
        serde_json::to_string(&root).unwrap(),
    )
    .unwrap();
}

fn base64_url_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut b = (chunk[0] as u32) << 16;
        if chunk.len() > 1 {
            b |= (chunk[1] as u32) << 8;
        }
        if chunk.len() > 2 {
            b |= chunk[2] as u32;
        }
        out.push(CHARS[((b >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((b >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((b >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(CHARS[(b & 0x3F) as usize] as char);
        }
    }
    out
}

fn make_unsigned_jwt(
    kid: Option<&str>,
    instance_id: &str,
    iat: i64,
    exp: i64,
    scope: &str,
) -> String {
    let header = match kid {
        Some(k) => {
            let h = serde_json::json!({"alg": "none", "typ": "JWT", "kid": k});
            base64_url_encode(serde_json::to_string(&h).unwrap().as_bytes())
        }
        None => "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0".to_owned(),
    };
    let payload = serde_json::json!({
        "iss": "sol-relay",
        "sub": format!("home:{instance_id}"),
        "aud": "relay",
        "scope": scope,
        "instance_id": instance_id,
        "iat": iat,
        "exp": exp,
        "jti": "jwt-test",
    });
    let payload_b64 = base64_url_encode(serde_json::to_string(&payload).unwrap().as_bytes());
    format!("{header}.{payload_b64}.sig")
}

async fn read_http_request(stream: &mut TcpStream) -> Result<(String, String), std::io::Error> {
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    let mut header_end = None;
    let mut content_length = 0usize;
    while header_end.is_none() {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = Some(pos + 4);
            let headers_str = String::from_utf8_lossy(&data[..pos]);
            for line in headers_str.lines() {
                let lower = line.to_ascii_lowercase();
                if let Some(val) = lower.strip_prefix("content-length:") {
                    content_length = val.trim().parse::<usize>().unwrap_or(0);
                }
            }
        }
    }
    let h_end = header_end.unwrap_or(data.len());
    let mut body = data[h_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let req_headers = String::from_utf8_lossy(&data[..h_end]).to_string();
    let req_body = String::from_utf8_lossy(&body[..content_length.min(body.len())]).to_string();
    Ok((req_headers, req_body))
}

async fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    body: &str,
) -> Result<(), std::io::Error> {
    let resp = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

struct HarnessEmitter(Arc<Mutex<Option<ServiceTokenHealth>>>);

impl CallosumEmit for HarnessEmitter {
    fn emit(&self, _event: &'static str, payload: serde_json::Value) {
        if let Some(obj) = payload.as_object() {
            let reason = obj
                .get("last_relay_tunnel_error")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let error_at = obj
                .get("last_relay_tunnel_error_at")
                .and_then(|v| v.as_i64());
            let success_at = obj
                .get("last_successful_relay_tunnel_at")
                .and_then(|v| v.as_i64());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            *self.0.lock().unwrap() = Some(ServiceTokenHealth {
                event_at_ms: Some(now),
                reason,
                tunnel_error_at_ms: error_at,
                tunnel_success_at_ms: success_at,
                now_ms: now,
            });
        }
    }
}

#[tokio::test]
async fn service_token_renewal_replaces_token_and_tunnel_connects() -> Result<(), String> {
    timeout(Duration::from_secs(10), async {
        let (journal_root, _cleanup) = temp_harness_journal("solstone-spl-lifecycle-14");
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;

        write_test_journal_config(&journal_root, "spl", Some(&format!("http://{address}")));
        let identity = load_or_create_service_identity(&journal_root).map_err(|e| e.to_string())?;
        let ca = load_service_identity_ca(&journal_root).map_err(|e| e.to_string())?;
        let expected_pem = ca.public_key_spki_pem();
        let inst_id = identity.instance_id.clone();

        let now_unix_val = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
        let now_mono_val = Arc::new(std::sync::atomic::AtomicU64::new(1000));
        let clock = RenewalClock {
            now_unix: {
                let u = now_unix_val.clone();
                Arc::new(move || u.load(std::sync::atomic::Ordering::SeqCst))
            },
            now_mono: {
                let m = now_mono_val.clone();
                Arc::new(move || Duration::from_secs(m.load(std::sync::atomic::Ordering::SeqCst)))
            },
        };

        // Old token: signed with kid-test-2
        let old_token = make_unsigned_jwt(
            Some("kid-test-2"),
            &inst_id,
            now_unix_val.load(std::sync::atomic::Ordering::SeqCst) - 100,
            now_unix_val.load(std::sync::atomic::Ordering::SeqCst) + 365 * 86_400,
            "session.listen",
        );
        save_service_token(&journal_root, &old_token).map_err(|e| e.to_string())?;

        let enrolled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let enroll_bodies = Arc::new(Mutex::new(Vec::new()));
        let issued_token = Arc::new(Mutex::new(None));
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let accepted_tx = Arc::new(Mutex::new(Some(accepted_tx)));

        let stand_in_task = tokio::spawn({
            let enrolled = enrolled.clone();
            let enroll_bodies = enroll_bodies.clone();
            let issued_token = issued_token.clone();
            let accepted_tx = accepted_tx.clone();
            let now_unix_val = now_unix_val.clone();
            let inst_id = inst_id.clone();
            async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut peek_buf = [0u8; 128];
                    let n = stream.peek(&mut peek_buf).await.unwrap_or(0);
                    let peek_str = String::from_utf8_lossy(&peek_buf[..n]);
                    if peek_str.starts_with("GET /.well-known/jwks.json") {
                        let _ = read_http_request(&mut stream).await;
                        // Serves without old kid before enroll, and with kid-test-1 after
                        let body = r#"{"keys":[{"kid":"kid-test-1"}]}"#;
                        let _ = write_http_response(&mut stream, 200, "OK", body).await;
                    } else if peek_str.starts_with("POST /enroll/home") {
                        let (_, body) = read_http_request(&mut stream).await.unwrap_or_default();
                        enroll_bodies.lock().unwrap().push(body.clone());
                        let parsed: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        let id = parsed
                            .get("instance_id")
                            .or_else(|| parsed.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(&inst_id);
                        let now = now_unix_val.load(std::sync::atomic::Ordering::SeqCst);
                        let tok = make_unsigned_jwt(
                            Some("kid-test-1"),
                            id,
                            now - 100,
                            now + 365 * 86_400,
                            "session.listen",
                        );
                        *issued_token.lock().unwrap() = Some(tok.clone());
                        enrolled.store(true, std::sync::atomic::Ordering::SeqCst);
                        let resp_body = serde_json::json!({"service_token": tok}).to_string();
                        let _ = write_http_response(&mut stream, 200, "OK", &resp_body).await;
                    } else {
                        // Tunnel WebSocket
                        let issued_token = issued_token.clone();
                        let accepted_tx = accepted_tx.clone();
                        let ws = accept_hdr_async(stream, move |req: &ServerRequest, res| {
                            let auth = req
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|h| h.to_str().ok())
                                .map(str::to_owned);
                            let expected = issued_token.lock().unwrap().clone();
                            if let Some(exp) = expected
                                && auth.as_deref() == Some(&format!("Bearer {exp}"))
                            {
                                if let Some(tx) = accepted_tx.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                                Ok(res)
                            } else {
                                Err(rejection_response(StatusCode::UNAUTHORIZED))
                            }
                        })
                        .await;
                        if let Ok(mut socket) = ws {
                            let _ = socket.next().await;
                        }
                    }
                }
            }
        });

        // Set up emitter to record client health
        let client_health = Arc::new(Mutex::new(None));

        let mut client = RelayClient::new(
            RelayClientConfig {
                instance_id: inst_id.clone(),
                relay_endpoint: format!("http://{address}"),
                token_source: Arc::new(JournalAttemptTokenSource::new(journal_root.clone())),
                service_token: ServiceToken::new(String::new()),
                dispatch_read_deadline: Duration::from_secs(1),
                ping_interval: Duration::from_secs(3600),
                ping_ack_timeout: Duration::from_secs(1),
                ack_stability_window: Duration::from_secs(3600),
                global_admission_ceiling: 1,
            },
            Arc::new(HarnessEmitter(client_health.clone())),
            Arc::new(ClosedDialer),
        );
        let running_client = client.clone();
        let run_task = tokio::spawn(async move { running_client.run().await });

        // Wait until client attempts tunnel and records health after 401 refusal
        for _ in 0..100 {
            if client_health
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|h| h.reason.as_deref())
                == Some("service_token_rejected")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            client_health
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|h| h.reason.as_deref()),
            Some("service_token_rejected")
        );

        // Build component with health closure filled from relay client's own health
        let renewal_cfg = RenewalConfig {
            clock,
            ..RenewalConfig::production(journal_root.clone(), {
                let h = client_health.clone();
                Arc::new(move || h.lock().unwrap().clone().unwrap_or_default())
            })
        };

        let mut renewal_state = RenewalState::default();
        let step = evaluate_once(&renewal_cfg, &mut renewal_state);
        assert_eq!(
            step,
            RenewalStep::Attempted {
                trigger: Trigger::UnknownKey,
                outcome: Outcome::Renewed,
            }
        );

        // Exactly one enroll
        assert_eq!(enroll_bodies.lock().unwrap().len(), 1);
        let recorded_body = &enroll_bodies.lock().unwrap()[0];
        assert!(recorded_body.contains(&inst_id));
        assert!(recorded_body.contains(&expected_pem));

        // Wait for next tunnel attempt presenting the new token to be accepted
        accepted_rx.await.map_err(|e| e.to_string())?;

        stop_relay_run(&mut client, run_task)
            .await
            .map_err(|e| e.to_string())?;
        stand_in_task.abort();
        Ok(())
    })
    .await
    .map_err(|_| "service_token_renewal_replaces_token_and_tunnel_connects timed out".to_owned())?
}

#[tokio::test]
async fn service_token_renewal_not_enrolled_rejection_backs_off_production() -> Result<(), String> {
    timeout(Duration::from_secs(10), async {
        let (journal_root, _cleanup) = temp_harness_journal("solstone-spl-lifecycle-14a");
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;

        write_test_journal_config(&journal_root, "spl", Some(&format!("http://{address}")));
        let identity = load_or_create_service_identity(&journal_root).map_err(|e| e.to_string())?;
        let ca = load_service_identity_ca(&journal_root).map_err(|e| e.to_string())?;
        let expected_pem = ca.public_key_spki_pem();
        let inst_id = identity.instance_id.clone();

        let expired_token =
            make_unsigned_jwt(Some("kid-test-1"), &inst_id, 100, 200, "session.listen");
        save_service_token(&journal_root, &expired_token).map_err(|e| e.to_string())?;
        let token_bytes_before =
            std::fs::read(journal_root.join("link/tokens/account.json")).unwrap();

        let recorded_enrolls = Arc::new(Mutex::new(Vec::new()));
        let stand_in_task = tokio::spawn({
            let recorded_enrolls = recorded_enrolls.clone();
            async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut peek_buf = [0u8; 128];
                    let n = stream.peek(&mut peek_buf).await.unwrap_or(0);
                    let peek_str = String::from_utf8_lossy(&peek_buf[..n]);
                    if peek_str.starts_with("POST /enroll/home") {
                        let (_, body) = read_http_request(&mut stream).await.unwrap_or_default();
                        recorded_enrolls.lock().unwrap().push(body.clone());
                        let parsed: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        if parsed.get("mode").and_then(|v| v.as_str()) == Some("renew") {
                            let _ = write_http_response(
                                &mut stream,
                                404,
                                "Not Found",
                                r#"{"error":"not_enrolled"}"#,
                            )
                            .await;
                        } else {
                            let _ = write_http_response(
                                &mut stream,
                                400,
                                "Bad Request",
                                r#"{"error":"bad_mode"}"#,
                            )
                            .await;
                        }
                    } else if peek_str.starts_with("GET /.well-known/jwks.json") {
                        let _ = read_http_request(&mut stream).await;
                        let _ = write_http_response(
                            &mut stream,
                            200,
                            "OK",
                            r#"{"keys":[{"kid":"kid-test-1"}]}"#,
                        )
                        .await;
                    }
                }
            }
        });

        let mono_sec = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let clock = RenewalClock {
            now_unix: Arc::new(|| 1_000_000), // expired (exp was 200)
            now_mono: {
                let m = mono_sec.clone();
                Arc::new(move || Duration::from_secs(m.load(std::sync::atomic::Ordering::SeqCst)))
            },
        };

        let mut renewal_cfg =
            RenewalConfig::production(journal_root.clone(), Arc::new(ServiceTokenHealth::default));
        renewal_cfg.clock = clock;

        let mut renewal_state = RenewalState::default();
        let step1 = evaluate_once(&renewal_cfg, &mut renewal_state);
        assert_eq!(
            step1,
            RenewalStep::Attempted {
                trigger: Trigger::Expired,
                outcome: Outcome::NotEnrolled,
            }
        );
        assert_eq!(
            std::fs::read(journal_root.join("link/tokens/account.json")).unwrap(),
            token_bytes_before
        );
        assert_eq!(recorded_enrolls.lock().unwrap().len(), 1);

        // Advance injected monotonic clock by less than 6 hours
        mono_sec.store(5 * 3600, std::sync::atomic::Ordering::SeqCst);
        let step2 = evaluate_once(&renewal_cfg, &mut renewal_state);
        assert_eq!(step2, RenewalStep::Blocked);
        assert_eq!(recorded_enrolls.lock().unwrap().len(), 1);

        for body in recorded_enrolls.lock().unwrap().iter() {
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed.get("mode").and_then(|v| v.as_str()), Some("renew"));
            assert_eq!(
                parsed
                    .get("instance_id")
                    .or_else(|| parsed.get("id"))
                    .and_then(|v| v.as_str()),
                Some(inst_id.as_str())
            );
            assert!(body.contains(&expected_pem));
        }

        stand_in_task.abort();
        Ok(())
    })
    .await
    .map_err(|_| {
        "service_token_renewal_not_enrolled_rejection_backs_off_production timed out".to_owned()
    })?
}

#[tokio::test]
async fn service_token_renewal_unhealthy_jwks_skips_enrollment() -> Result<(), String> {
    timeout(Duration::from_secs(10), async {
        let (journal_root, _cleanup) = temp_harness_journal("solstone-spl-lifecycle-15");
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;

        write_test_journal_config(&journal_root, "spl", Some(&format!("http://{address}")));
        let identity = load_or_create_service_identity(&journal_root).map_err(|e| e.to_string())?;
        let inst_id = identity.instance_id.clone();

        let jwks_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let enroll_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let stand_in_task = tokio::spawn({
            let jwks_requests = jwks_requests.clone();
            let enroll_requests = enroll_requests.clone();
            async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut peek_buf = [0u8; 128];
                    let n = stream.peek(&mut peek_buf).await.unwrap_or(0);
                    let peek_str = String::from_utf8_lossy(&peek_buf[..n]);
                    if peek_str.starts_with("GET /.well-known/jwks.json") {
                        let _ = read_http_request(&mut stream).await;
                        jwks_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = write_http_response(
                            &mut stream,
                            503,
                            "Service Unavailable",
                            r#"{"keys":[]}"#,
                        )
                        .await;
                    } else if peek_str.starts_with("POST /enroll/home") {
                        let _ = read_http_request(&mut stream).await;
                        enroll_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = write_http_response(
                            &mut stream,
                            200,
                            "OK",
                            r#"{"service_token":"dummy"}"#,
                        )
                        .await;
                    }
                }
            }
        });

        let now_unix = 1_000_000i64;
        let young_token = make_unsigned_jwt(
            Some("kid-test-1"),
            &inst_id,
            now_unix - 100,
            now_unix + 365 * 86_400,
            "session.listen",
        );
        save_service_token(&journal_root, &young_token).map_err(|e| e.to_string())?;

        let now_ms = now_unix * 1000;
        let health = ServiceTokenHealth {
            event_at_ms: Some(now_ms),
            now_ms,
            reason: Some("service_token_rejected".into()),
            tunnel_error_at_ms: Some(100),
            tunnel_success_at_ms: Some(50),
        };

        let mut renewal_cfg =
            RenewalConfig::production(journal_root.clone(), Arc::new(move || health.clone()));
        renewal_cfg.clock = RenewalClock {
            now_unix: Arc::new(move || now_unix),
            now_mono: Arc::new(|| Duration::from_secs(1000)),
        };

        let mut renewal_state = RenewalState::default();
        for _ in 0..5 {
            let step = evaluate_once(&renewal_cfg, &mut renewal_state);
            assert_eq!(step, RenewalStep::Current);
        }

        assert!(jwks_requests.load(std::sync::atomic::Ordering::SeqCst) >= 5);
        assert_eq!(enroll_requests.load(std::sync::atomic::Ordering::SeqCst), 0);

        stand_in_task.abort();
        Ok(())
    })
    .await
    .map_err(|_| "service_token_renewal_unhealthy_jwks_skips_enrollment timed out".to_owned())?
}

#[path = "spl_relay_lifecycle/access_contract.rs"]
mod access_contract;
