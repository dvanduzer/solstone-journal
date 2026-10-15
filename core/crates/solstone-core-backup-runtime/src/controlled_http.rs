// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! One HTTP request owns its request worker, socket and runtime until join. On Linux
//! and macOS the resolver is an installed child. Windows permits a hostname-only
//! resolver survivor; Android uses one permanent hostname-only worker.

use crate::hosted_runtime::{HttpError, HttpRequest, HttpResponse};
use solstone_core_backup::archive_admission::ArchiveCancellation;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use solstone_core_backup::owned_dns::{OwnedDnsFamily, resolve_owned_http_host};
use std::fmt;
use std::future::Future;
use std::io::{self, Read};
use std::net::SocketAddr;
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
pub(crate) const TEARDOWN_BODY_LIMIT: usize = 4 * 1024 * 1024;

#[cfg(any(target_os = "android", all(test, feature = "full-tests")))]
pub(crate) mod android_dns;

#[derive(Clone, Copy)]
enum RequestPolicy {
    #[cfg_attr(any(windows, target_os = "android"), allow(dead_code))]
    Archive,
    Teardown,
}

impl RequestPolicy {
    fn supports(self, method: &str) -> bool {
        matches!(method, "GET" | "POST") || matches!(self, Self::Teardown) && method == "DELETE"
    }

    fn body_limit(self) -> usize {
        match self {
            Self::Archive => usize::MAX,
            Self::Teardown => TEARDOWN_BODY_LIMIT,
        }
    }
}

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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve_host(
    host: &str,
    port: u16,
    family: ureq::config::IpFamily,
    end: Instant,
    cancellation: &ArchiveCancellation,
) -> io::Result<Vec<SocketAddr>> {
    let family = match family {
        ureq::config::IpFamily::Any => OwnedDnsFamily::Any,
        ureq::config::IpFamily::Ipv4Only => OwnedDnsFamily::Ipv4,
        ureq::config::IpFamily::Ipv6Only => OwnedDnsFamily::Ipv6,
    };
    resolve_owned_http_host(host, port, family, end, cancellation)
}

/// Windows has no owned resolver child yet. The system resolver runs on a
/// helper thread, waited on with this request's own end and cancellation; if
/// either comes first the thread is left to finish alone, holding nothing.
#[cfg(windows)]
fn resolve_host(
    host: &str,
    port: u16,
    family: ureq::config::IpFamily,
    end: Instant,
    cancellation: &ArchiveCancellation,
) -> io::Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    use std::sync::mpsc::{RecvTimeoutError, channel};
    const MAX_ADDRESSES: usize = 16;
    let (sender, receiver) = channel();
    // Keep ureq's authority.host():port form, including [IPv6] brackets.
    let target = format!("{host}:{port}");
    std::thread::Builder::new()
        .name("controlled-http-dns".into())
        .spawn(move || {
            let _ = sender.send(
                target
                    .to_socket_addrs()
                    .map(|addresses| addresses.collect::<Vec<_>>()),
            );
        })?;
    loop {
        if cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "HTTP operation cancelled",
            ));
        }
        let now = Instant::now();
        if now >= end {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP DNS deadline reached",
            ));
        }
        match receiver.recv_timeout(CANCEL_POLL.min(end - now)) {
            Ok(addresses) => {
                return Ok(addresses?
                    .into_iter()
                    .filter(|address| match family {
                        ureq::config::IpFamily::Any => true,
                        ureq::config::IpFamily::Ipv4Only => address.is_ipv4(),
                        ureq::config::IpFamily::Ipv6Only => address.is_ipv6(),
                    })
                    .take(MAX_ADDRESSES)
                    .collect());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("HTTP DNS resolver ended"));
            }
        }
    }
}

#[cfg(target_os = "android")]
fn resolve_host(
    host: &str,
    port: u16,
    family: ureq::config::IpFamily,
    end: Instant,
    cancellation: &ArchiveCancellation,
) -> io::Result<Vec<SocketAddr>> {
    android_dns::resolve(host, port, family, end, cancellation)
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
        let addresses = resolve_host(
            authority.host(),
            port,
            config.ip_family(),
            self.0.phase_end(timeout),
            &self.0.cancellation,
        );
        // Recheck the original signal before any address can authorize a socket.
        // A late Android/Windows hostname-only lookup cannot dispatch HTTP.
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

fn supported(request: &HttpRequest, config: &ureq::config::Config, policy: RequestPolicy) -> bool {
    policy.supports(&request.method)
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

#[cfg_attr(any(windows, target_os = "android"), allow(dead_code))]
pub(crate) fn execute(
    request: &HttpRequest,
    cancellation: &ArchiveCancellation,
) -> Result<HttpResponse, HttpError> {
    execute_with_policy(request, cancellation, RequestPolicy::Archive)
}

pub(crate) fn execute_teardown(
    request: &HttpRequest,
    cancellation: &ArchiveCancellation,
) -> Result<HttpResponse, HttpError> {
    execute_with_policy(request, cancellation, RequestPolicy::Teardown)
}

fn execute_with_policy(
    request: &HttpRequest,
    cancellation: &ArchiveCancellation,
    policy: RequestPolicy,
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
    let mut builder = ureq::config::Config::builder()
        .http_status_as_error(false)
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .timeout_global(Some(deadline.saturating_duration_since(Instant::now())));
    if matches!(policy, RequestPolicy::Teardown) {
        builder = builder.max_redirects(0);
    }
    let config = builder.build();
    if resolver_env_present.into_iter().any(|present| present)
        || !supported(request, &config, policy)
    {
        return Err(HttpError::Other);
    }
    let cancellation = cancellation.clone();
    let joined = std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("controlled-http".into())
            .spawn_scoped(scope, || {
                run_request(request, &cancellation, deadline, config, policy)
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
    policy: RequestPolicy,
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
        "DELETE" => {
            let mut builder = agent.delete(&request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            builder.call()
        }
        "GET" => {
            let mut builder = agent.get(&request.url);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            builder.call()
        }
        _ => return Err(HttpError::Other),
    };
    control.check().map_err(|_| HttpError::Other)?;
    let response = response.map_err(|error| match error {
        ureq::Error::Timeout(_) => HttpError::Timeout,
        ureq::Error::Io(error) if error.kind() == io::ErrorKind::TimedOut => HttpError::Timeout,
        _ => HttpError::Unreachable,
    })?;
    let status = response.status().as_u16();
    if matches!(policy, RequestPolicy::Teardown) && (300..400).contains(&status) {
        return Err(HttpError::Other);
    }
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
        if body
            .len()
            .checked_add(count)
            .is_none_or(|length| length > policy.body_limit())
        {
            return Err(HttpError::Other);
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

    fn local_request(
        method: &str,
        timeout: Duration,
        cancellation: &ArchiveCancellation,
        respond: impl FnOnce(&mut std::net::TcpStream, &str) + Send,
    ) -> Result<HttpResponse, HttpError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/exact?uploadId=a%26b",
            listener.local_addr().unwrap()
        );
        std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                let end = Instant::now() + Duration::from_secs(5);
                let (mut peer, _) = loop {
                    match listener.accept() {
                        Ok(peer) => break peer,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < end, "request never arrived");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    peer.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                    assert!(head.len() <= 65536);
                }
                let head = String::from_utf8(head).unwrap();
                assert!(head.starts_with(&format!("{method} /exact?uploadId=a%26b HTTP/1.1\r\n")));
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                peer.read_exact(&mut body).unwrap();
                assert_eq!(
                    body,
                    if method == "POST" {
                        b"abc".as_slice()
                    } else {
                        b"".as_slice()
                    }
                );
                respond(&mut peer, &url);
                let _ = peer.shutdown(std::net::Shutdown::Write);
                let mut extra = Vec::new();
                match peer.read_to_end(&mut extra) {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                        ) => {}
                    Err(error) => panic!("client did not close the owned socket: {error}"),
                }
                assert!(extra.is_empty());
            });
            let result = execute_teardown(
                &HttpRequest {
                    method: method.into(),
                    url: url.clone(),
                    headers: vec![],
                    body: if method == "POST" {
                        b"abc".to_vec()
                    } else {
                        vec![]
                    },
                    timeout,
                },
                cancellation,
            );
            eprintln!("teardown HTTP control {method}: {result:?}");
            server.join().unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
                "unexpected redirect/retry connection"
            );
            result
        })
    }

    #[test]
    #[ignore = "requires a copied installed package with the actual owned DNS member"]
    fn teardown_dispatches_actual_get_post_delete_and_caps_before_extension() {
        for method in ["GET", "POST", "DELETE"] {
            let response = local_request(
                method,
                Duration::from_secs(5),
                &ArchiveCancellation::new(),
                |peer, _| {
                    peer.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .unwrap();
                },
            )
            .unwrap();
            assert_eq!(response.body, b"ok");
        }
        let response = local_request("GET", Duration::from_secs(5), &ArchiveCancellation::new(), |peer, _| {
            write!(peer, "HTTP/1.1 200 OK\r\nContent-Length: {TEARDOWN_BODY_LIMIT}\r\nConnection: close\r\n\r\n").unwrap();
            peer.write_all(&vec![b'x'; TEARDOWN_BODY_LIMIT]).unwrap();
        }).unwrap();
        assert_eq!(response.body.len(), TEARDOWN_BODY_LIMIT);
        assert!(
            local_request(
                "GET",
                Duration::from_secs(5),
                &ArchiveCancellation::new(),
                |peer, _| {
                    write!(
                        peer,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        TEARDOWN_BODY_LIMIT + 1
                    )
                    .unwrap();
                    let _ = peer.write_all(&vec![b'x'; TEARDOWN_BODY_LIMIT + 1]);
                }
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires a copied installed package with the actual owned DNS member"]
    fn teardown_redirect_cancel_and_body_deadline_close_actual_socket_before_return() {
        assert!(local_request("DELETE", Duration::from_secs(5), &ArchiveCancellation::new(), |peer, url| {
            write!(peer, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {url}\r\nContent-Length: 0\r\n\r\n").unwrap();
        }).is_err());
        let cancellation = ArchiveCancellation::new();
        let server_cancel = cancellation.clone();
        assert!(
            local_request(
                "DELETE",
                Duration::from_secs(5),
                &cancellation,
                move |peer, _| {
                    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nx")
                        .unwrap();
                    server_cancel.cancel();
                }
            )
            .is_err()
        );
        assert!(
            local_request(
                "GET",
                Duration::from_secs(3),
                &ArchiveCancellation::new(),
                |peer, _| {
                    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nx")
                        .unwrap();
                    // Keep the write half open until the deadline closes the client.
                    let mut extra = [0];
                    assert_eq!(peer.read(&mut extra).unwrap(), 0);
                }
            )
            .is_err()
        );
    }

    #[test]
    fn teardown_unsupported_method_refuses_without_a_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let request = HttpRequest {
            method: "PUT".into(),
            url: format!("http://{}/", listener.local_addr().unwrap()),
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(1),
        };
        assert!(execute_teardown(&request, &ArchiveCancellation::new()).is_err());
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

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
