// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One Linux HTTP request owns its resolver child, socket and runtime until join.

use crate::hosted_runtime::{HttpError, HttpRequest, HttpResponse};
use solstone_core_backup::archive_admission::ArchiveCancellation;
use solstone_core_backup::owned_dns::{OwnedDnsFamily, resolve_owned_http_host};
use std::fmt;
use std::future::Future;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout,
    RustlsConnector, Transport, time::Duration as TransportDuration,
};

const CANCEL_POLL: Duration = Duration::from_millis(5);
const RESOLVER_ENV: [&str; 3] = ["LOCALDOMAIN", "RES_OPTIONS", "HOSTALIASES"];

struct Control {
    runtime: Runtime,
    deadline: Instant,
    cancellation: ArchiveCancellation,
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlledHttp")
    }
}

impl Control {
    fn check(&self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            // Rustls complete_io retries Interrupted; cancellation must be terminal.
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "HTTP operation cancelled",
            ))
        } else if Instant::now() >= self.deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP deadline reached",
            ))
        } else {
            Ok(())
        }
    }

    fn phase_end(&self, timeout: NextTimeout) -> Instant {
        match timeout.after {
            TransportDuration::Exact(d) => Instant::now()
                .checked_add(d)
                .unwrap_or(self.deadline)
                .min(self.deadline),
            TransportDuration::NotHappening => self.deadline,
        }
    }

    fn wait<T>(
        &self,
        future: impl Future<Output = io::Result<T>>,
        timeout: NextTimeout,
    ) -> io::Result<T> {
        self.check()?;
        let end = self.phase_end(timeout);
        if Instant::now() >= end {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP phase deadline reached",
            ));
        }
        // Poll the same future until completion or drop. Neither polling nor progress
        // renews this anchored phase end or the original request deadline.
        let mut future = Box::pin(future);
        self.runtime.block_on(async {
            loop {
                self.check()?;
                if Instant::now() >= end {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP phase deadline reached"));
                }
                tokio::select! { biased;
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(end)) => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP phase deadline reached"));
                    }
                    _ = tokio::time::sleep(CANCEL_POLL) => {},
                    result = future.as_mut() => {
                        self.check()?;
                        if Instant::now() >= end {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP phase deadline reached"));
                        }
                        return result;
                    }
                }
            }
        })
    }

    fn wait_transport<T>(
        &self,
        future: impl Future<Output = io::Result<T>>,
        timeout: NextTimeout,
    ) -> Result<T, ureq::Error> {
        self.wait(future, timeout).map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut {
                // ureq's Await100 state treats this typed phase timeout as the
                // signal to send the body after no 100-continue response.
                ureq::Error::Timeout(timeout.reason)
            } else {
                error.into()
            }
        })
    }
}

#[derive(Debug)]
struct OwnedResolver(Arc<Control>);
impl Resolver for OwnedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        self.0.check()?;
        let authority = uri
            .authority()
            .ok_or_else(|| io::Error::other("HTTP authority missing"))?;
        let port = authority
            .port_u16()
            .or_else(|| match uri.scheme_str() {
                Some("http") => Some(80),
                Some("https") => Some(443),
                _ => None,
            })
            .ok_or_else(|| io::Error::other("unsupported HTTP scheme"))?;
        let family = match config.ip_family() {
            ureq::config::IpFamily::Any => OwnedDnsFamily::Any,
            ureq::config::IpFamily::Ipv4Only => OwnedDnsFamily::Ipv4,
            ureq::config::IpFamily::Ipv6Only => OwnedDnsFamily::Ipv6,
        };
        let addresses = resolve_owned_http_host(
            authority.host(),
            port,
            family,
            self.0.phase_end(timeout),
            &self.0.cancellation,
        );
        // Child completion has already waited and joined its readers. Check the
        // terminal signal before propagating an internal child cancellation error.
        self.0.check()?;
        let mut result = self.empty();
        for address in addresses? {
            result.push(address);
        }
        Ok(result)
    }
}

#[derive(Debug)]
struct OwnedConnector(Arc<Control>);
impl<In: Transport> Connector<In> for OwnedConnector {
    type Out = Either<In, OwnedTcp>;
    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        self.0.check()?;
        if let Some(transport) = chained {
            return Ok(Some(Either::A(transport)));
        }
        if details.addrs.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        let end = self.0.phase_end(details.timeout);
        let connect_budget = end.saturating_duration_since(Instant::now());
        let total = 2.0 * (1.0 - 0.5_f64.powi(details.addrs.len() as i32));
        let mut weight = 1.0;
        for address in &details.addrs {
            self.0.check()?;
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ureq::Error::Timeout(details.timeout.reason));
            }
            let per_address =
                Duration::from_secs_f64(connect_budget.as_secs_f64() * weight / total)
                    .max(Duration::from_millis(10))
                    .min(remaining);
            let timeout = NextTimeout {
                after: TransportDuration::Exact(per_address),
                reason: details.timeout.reason,
            };
            match self.0.wait(TcpStream::connect(*address), timeout) {
                Ok(stream) => {
                    if details.config.no_delay() {
                        stream.set_nodelay(true)?;
                    }
                    return Ok(Some(Either::B(OwnedTcp {
                        stream,
                        buffers: LazyBuffers::new(
                            details.config.input_buffer_size(),
                            details.config.output_buffer_size(),
                        ),
                        control: self.0.clone(),
                    })));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
                    ) =>
                {
                    self.0.check()?;
                    weight *= 0.5;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(io::ErrorKind::ConnectionRefused, "HTTP addresses exhausted").into())
    }
}

#[derive(Debug)]
struct OwnedTcp {
    // Drop the socket before the last control/runtime owner.
    stream: TcpStream,
    buffers: LazyBuffers,
    control: Arc<Control>,
}
impl Transport for OwnedTcp {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.control.check()?;
        self.control.wait_transport(
            self.stream.write_all(&self.buffers.output()[..amount]),
            timeout,
        )?;
        Ok(())
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.control.check()?;
        let count = self
            .control
            .wait_transport(self.stream.read(self.buffers.input_append_buf()), timeout)?;
        self.buffers.input_appended(count);
        Ok(count > 0)
    }
    fn maybe_await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.control.check()?;
        if self.buffers.can_use_input() {
            return Ok(true);
        }
        self.await_input(timeout)
    }
    fn is_open(&mut self) -> bool {
        if self.control.check().is_err() {
            return false;
        }
        let mut bytes = [std::mem::MaybeUninit::uninit(); 1];
        match self.stream.try_io(tokio::io::Interest::READABLE, || {
            socket2::SockRef::from(&self.stream).peek(&mut bytes)
        }) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => true,
            // Pending bytes are unsolicited input; ureq's pinned TcpTransport
            // closes these connections rather than reusing them.
            Ok(_) => false,
            Err(_) => false,
        }
    }
}

fn supported(request: &HttpRequest, config: &ureq::config::Config) -> bool {
    matches!(request.method.as_str(), "GET" | "POST")
        && request
            .url
            .parse::<ureq::http::Uri>()
            .ok()
            .is_some_and(|uri| {
                matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some()
            })
        && config.tls_config().provider() == ureq::tls::TlsProvider::Rustls
        && config
            .proxy()
            .is_none_or(|proxy| proxy.protocol() == ureq::ProxyProtocol::Http)
}

pub(crate) fn execute(
    request: &HttpRequest,
    cancellation: &ArchiveCancellation,
) -> Result<HttpResponse, HttpError> {
    // Anchor before admission or thread launch. The owning archive/handoff keeps
    // its pre-existing operation deadline and rejects results against it too.
    let deadline = Instant::now()
        .checked_add(request.timeout)
        .ok_or(HttpError::Other)?;
    if cancellation.is_cancelled() {
        return Err(HttpError::Other);
    }
    if Instant::now() >= deadline {
        return Err(HttpError::Timeout);
    }
    let resolver_env_present = RESOLVER_ENV.map(|name| std::env::var_os(name).is_some());
    let config = ureq::config::Config::builder()
        .http_status_as_error(false)
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .timeout_global(Some(deadline.saturating_duration_since(Instant::now())))
        .build();
    if resolver_env_present.into_iter().any(|present| present) || !supported(request, &config) {
        return Err(HttpError::Other);
    }
    let cancellation = cancellation.clone();
    let joined = std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("controlled-http".into())
            .spawn_scoped(scope, || {
                run_request(request, &cancellation, deadline, config)
            });
        worker.map(|worker| worker.join())
    });
    // Both a worker panic and spawn failure are inspected only after the scope
    // has closed. Worker-owned sockets, child and runtime unwind before join.
    let result = match joined {
        Ok(Ok(result)) => result,
        Ok(Err(_)) | Err(_) => Err(HttpError::Other),
    };
    if cancellation.is_cancelled() {
        Err(HttpError::Other)
    } else if Instant::now() >= deadline {
        Err(HttpError::Timeout)
    } else {
        result
    }
}

fn run_request(
    request: &HttpRequest,
    cancellation: &ArchiveCancellation,
    deadline: Instant,
    config: ureq::config::Config,
) -> Result<HttpResponse, HttpError> {
    let control = Arc::new(Control {
        runtime: tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|_| HttpError::Other)?,
        deadline,
        cancellation: cancellation.clone(),
    });
    control.check().map_err(|_| HttpError::Other)?;
    let connector =
        ().chain(ConnectProxyConnector::default())
            .chain(OwnedConnector(control.clone()))
            .chain(RustlsConnector::default());
    let agent = ureq::Agent::with_parts(config, connector, OwnedResolver(control.clone()));
    let response = match request.method.as_str() {
        "POST" => {
            let mut builder = agent.post(&request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            builder.send(&request.body)
        }
        _ => {
            let mut builder = agent.get(&request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            builder.call()
        }
    };
    control.check().map_err(|_| HttpError::Other)?;
    let response = response.map_err(|error| match error {
        ureq::Error::Timeout(_) => HttpError::Timeout,
        ureq::Error::Io(error) if error.kind() == io::ErrorKind::TimedOut => HttpError::Timeout,
        _ => HttpError::Unreachable,
    })?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_owned()))
        })
        .collect();
    let mut reader = response.into_body().into_reader();
    let mut body = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        control.check().map_err(|_| HttpError::Other)?;
        let count = reader.read(&mut buffer).map_err(|_| HttpError::Other)?;
        control.check().map_err(|_| HttpError::Other)?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..count]);
    }
    drop(reader);
    drop(agent);
    control.check().map_err(|_| HttpError::Other)?;
    // All other control references die with the per-request Agent/transports.
    // Normal Runtime drop occurs on this owning thread before the caller joins.
    drop(control);
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(all(test, feature = "full-tests"))]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tcp_probe_accepts_idle_socket_and_rejects_pending_bytes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let control = Arc::new(Control {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap(),
            deadline: Instant::now() + Duration::from_secs(2),
            cancellation: ArchiveCancellation::new(),
        });
        let stream = control
            .runtime
            .block_on(TcpStream::connect(listener.local_addr().unwrap()))
            .unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let mut transport = OwnedTcp {
            stream,
            buffers: LazyBuffers::new(128, 128),
            control: control.clone(),
        };
        assert!(transport.is_open());
        peer.write_all(b"x").unwrap();
        control.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), transport.stream.readable())
                .await
                .unwrap()
                .unwrap();
        });
        assert!(!transport.is_open());
    }
}
