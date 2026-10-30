use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::dto::{lock, HeaderEntry, TransportHooks};
use crate::net::connectors::{connect_proxy_with_hooks, http_connect, socks_negotiate, tls_wrap};
use crate::net::policy::{
    admit_remote_target, admit_target, hooks_decoy, ResolveLimits, ResolvedTarget,
};
use crate::net::route::NativeProxyRoute;
use crate::observe::Metrics;

/// Source `requests` default upper bound for redirect chains.
const MAX_REDIRECTS: usize = 30;

pub struct OpenedResponse {
    pub body: Incoming,
    pub content_length: Option<u64>,
    pub status: u16,
    pub url: String,
    pub location: Option<String>,
    pub headers: Vec<(String, String)>,
    #[allow(dead_code)]
    pub peer: SocketAddr,
    pub connection: Connection,
}

pub struct Connection {
    driver: Option<JoinHandle<()>>,
    sender: Option<hyper::client::conn::http1::SendRequest<Empty<Bytes>>>,
    owner: Option<Arc<DriverOwner>>,
}

/// Immutable options for one open request path.
///
/// The direct constructor (`route: None`, `follow_redirects: false`) preserves
/// the Task3 transfer entry exactly: redirects are rejected and no proxy
/// endpoint is consulted.
#[derive(Clone, Copy)]
pub struct OpenOptions<'a> {
    pub route: Option<&'a NativeProxyRoute>,
    pub follow_redirects: bool,
    /// Return a 3xx response to the caller instead of following or rejecting
    /// it. The Task3 transfer entry leaves this false.
    pub allow_redirect_response: bool,
}

impl OpenOptions<'_> {
    pub fn direct() -> Self {
        Self {
            route: None,
            follow_redirects: false,
            allow_redirect_response: false,
        }
    }
}

/// Per-worker queue for drivers that could not be joined before a sync drop.
/// Abort is not an exit confirmation. The worker awaits this queue before reuse
/// and before it reports quiescence.
pub struct DriverOwner {
    retired: Mutex<Vec<JoinHandle<()>>>,
    cap: usize,
    active: AtomicUsize,
}

impl DriverOwner {
    pub fn new() -> Self {
        Self {
            retired: Mutex::new(Vec::new()),
            cap: 8,
            active: AtomicUsize::new(0),
        }
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub fn retired_len(&self) -> usize {
        lock(&self.retired).len()
    }

    fn note_spawn(&self) {
        self.active.fetch_add(1, Ordering::AcqRel);
    }

    fn note_joined(&self) {
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0, "driver active underflow");
    }

    fn retain(&self, handle: JoinHandle<()>) {
        let mut retired = lock(&self.retired);
        retired.retain(|item| !item.is_finished());
        assert!(
            retired.len() < self.cap,
            "driver retirement queue exceeded {}",
            self.cap
        );
        retired.push(handle);
    }

    pub async fn reap(&self, deadline: Instant) -> bool {
        let handles = std::mem::take(&mut *lock(&self.retired));
        let mut pending = Vec::new();
        for mut handle in handles {
            if handle.is_finished() {
                let _ = handle.await;
                self.note_joined();
                continue;
            }
            tokio::select! {
                biased;
                _ = &mut handle => self.note_joined(),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    pending.push(handle);
                }
            }
        }
        let done = pending.is_empty();
        lock(&self.retired).extend(pending);
        done && self.retired_len() == 0
    }
}

impl Default for DriverOwner {
    fn default() -> Self {
        Self::new()
    }
}

pub fn cleanup_deadline(overall: Instant) -> Instant {
    let window = Instant::now() + Duration::from_secs(2);
    if overall > Instant::now() {
        window.min(overall)
    } else {
        window
    }
}

impl Connection {
    pub fn abort(&mut self) {
        self.sender.take();
        if let Some(driver) = self.driver.as_ref() {
            driver.abort();
        }
    }

    /// Abort the owner driver and wait for that task to finish.
    /// `false` parks the handle on the worker owner. It is not dropped.
    pub async fn shutdown(&mut self, deadline: Instant) -> bool {
        self.abort();
        let Some(mut driver) = self.driver.take() else {
            return true;
        };
        tokio::select! {
            biased;
            _ = &mut driver => {
                if let Some(owner) = &self.owner {
                    owner.note_joined();
                }
                true
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                if let Some(owner) = &self.owner {
                    owner.retain(driver);
                } else {
                    self.driver = Some(driver);
                }
                false
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.abort();
        if let Some(driver) = self.driver.take() {
            if let Some(owner) = &self.owner {
                owner.retain(driver);
            }
        }
    }
}

/// Task3-compatible direct open. Redirects are rejected, and no proxy is used.
#[allow(clippy::too_many_arguments)]
pub async fn open_get(
    http: &vm::HttpConfig,
    url: &str,
    headers: &[HeaderEntry],
    cancel: &CancellationToken,
    metrics: &Metrics,
    hooks: &TransportHooks,
    owner: &Arc<DriverOwner>,
    started: Instant,
) -> Result<OpenedResponse, &'static str> {
    open_get_with_options(
        http,
        url,
        headers,
        cancel,
        metrics,
        hooks,
        owner,
        started,
        OpenOptions::direct(),
    )
    .await
}

/// Opens one request with an immutable route snapshot and redirect policy.
#[allow(clippy::too_many_arguments)]
pub async fn open_get_with_options(
    http: &vm::HttpConfig,
    url: &str,
    headers: &[HeaderEntry],
    cancel: &CancellationToken,
    metrics: &Metrics,
    hooks: &TransportHooks,
    owner: &Arc<DriverOwner>,
    started: Instant,
    options: OpenOptions<'_>,
) -> Result<OpenedResponse, &'static str> {
    let mut current = Url::parse(url).map_err(|_| "policy_denied")?;
    let overall = started + http.request_timeout.min(http.max_stream_duration);
    let mut current_headers = headers.to_vec();
    let mut redirects = 0usize;
    loop {
        let mut opened = open_once(
            http,
            &current,
            &current_headers,
            options.route,
            cancel,
            metrics,
            hooks,
            owner,
            started,
            overall,
        )
        .await?;
        if options.follow_redirects && (300..400).contains(&opened.status) {
            if let Some(location) = opened.location.clone() {
                if redirects >= MAX_REDIRECTS {
                    let _ = opened.connection.shutdown(cleanup_deadline(overall)).await;
                    return Err("redirect_limit");
                }
                let next = current.join(&location).map_err(|_| "policy_denied")?;
                let cross_origin = next.scheme() != current.scheme()
                    || next.host_str() != current.host_str()
                    || next.port_or_known_default() != current.port_or_known_default();
                if cross_origin {
                    strip_sensitive_headers(&mut current_headers);
                }
                let _ = opened.connection.shutdown(cleanup_deadline(overall)).await;
                current = next;
                redirects += 1;
                continue;
            }
        }
        if opened.status >= 300 && opened.status < 400 {
            if options.allow_redirect_response {
                return Ok(opened);
            }
            let _ = opened.connection.shutdown(cleanup_deadline(overall)).await;
            return Err("redirect_unsupported");
        }
        if !(200..300).contains(&opened.status) {
            let _ = opened.connection.shutdown(cleanup_deadline(overall)).await;
            return Err("http_status");
        }
        return Ok(opened);
    }
}

fn strip_sensitive_headers(headers: &mut Vec<HeaderEntry>) {
    headers.retain(|header| {
        let name = header.name.to_ascii_lowercase();
        name != b"authorization" && name != b"cookie" && name != b"proxy-authorization"
    });
}

#[allow(clippy::too_many_arguments)]
async fn open_once(
    http: &vm::HttpConfig,
    parsed: &Url,
    headers: &[HeaderEntry],
    route: Option<&NativeProxyRoute>,
    cancel: &CancellationToken,
    metrics: &Metrics,
    hooks: &TransportHooks,
    owner: &Arc<DriverOwner>,
    started: Instant,
    overall: Instant,
) -> Result<OpenedResponse, &'static str> {
    reject_headers(http, headers)?;
    let https = parsed.scheme().eq_ignore_ascii_case("https");
    let connect_deadline = (started + http.connect_timeout).min(overall);
    let Some(route) = route else {
        let request = build_request(parsed, headers, false)?;
        let target = admit_target(
            http,
            parsed,
            ResolveLimits {
                cancel,
                connect: connect_deadline,
                dns_hold: hooks.dns_hold.as_deref(),
                decoy: hooks_decoy(hooks),
            },
        )
        .await?;
        metrics.record_dns_candidates(&target.candidates);
        if https {
            let (peer, io) = connect_tls(&target, cancel, connect_deadline, hooks).await?;
            return finish_request(
                request,
                parsed.as_str().to_string(),
                cancel,
                metrics,
                peer,
                io,
                overall,
                connect_deadline,
                owner,
            )
            .await;
        }
        let (peer, io) = connect_plain(&target, cancel, connect_deadline).await?;
        return finish_request(
            request,
            parsed.as_str().to_string(),
            cancel,
            metrics,
            peer,
            io,
            overall,
            connect_deadline,
            owner,
        )
        .await;
    };
    if !route.kind.supported() {
        return Err("proxy_unsupported");
    }
    if route.kind.delegates_target_dns() && !route.trusted_remote_dns {
        return Err("policy_denied");
    }
    let (target_host, target_port, local_ip) = if route.kind.delegates_target_dns() {
        let (host, port) = admit_remote_target(http, parsed)?;
        (host, port, None)
    } else {
        let target = admit_target(
            http,
            parsed,
            ResolveLimits {
                cancel,
                connect: connect_deadline,
                dns_hold: hooks.dns_hold.as_deref(),
                decoy: hooks_decoy(hooks),
            },
        )
        .await?;
        metrics.record_dns_candidates(&target.candidates);
        let port = parsed.port_or_known_default().ok_or("policy_denied")?;
        (target.host.clone(), port, Some(target.address.ip()))
    };
    let (mut io, peer) = connect_proxy_with_hooks(route, hooks, cancel, connect_deadline).await?;
    if route.kind.is_http_proxy() {
        if https {
            http_connect(&mut io, &target_host, target_port, cancel, connect_deadline).await?;
            let io = tls_wrap(
                io,
                &target_host,
                hooks,
                cancel,
                connect_deadline,
                "target_tls_failed",
            )
            .await?;
            let request = build_request(parsed, headers, false)?;
            return finish_request(
                request,
                parsed.as_str().to_string(),
                cancel,
                metrics,
                peer,
                TokioIo::new(io),
                overall,
                connect_deadline,
                owner,
            )
            .await;
        }
        let request = build_request(parsed, headers, true)?;
        return finish_request(
            request,
            parsed.as_str().to_string(),
            cancel,
            metrics,
            peer,
            TokioIo::new(io),
            overall,
            connect_deadline,
            owner,
        )
        .await;
    }
    socks_negotiate(
        &mut io,
        route,
        &target_host,
        target_port,
        local_ip,
        cancel,
        connect_deadline,
    )
    .await?;
    if https {
        let io = tls_wrap(
            io,
            &target_host,
            hooks,
            cancel,
            connect_deadline,
            "target_tls_failed",
        )
        .await?;
        let request = build_request(parsed, headers, false)?;
        finish_request(
            request,
            parsed.as_str().to_string(),
            cancel,
            metrics,
            peer,
            TokioIo::new(io),
            overall,
            connect_deadline,
            owner,
        )
        .await
    } else {
        let request = build_request(parsed, headers, false)?;
        finish_request(
            request,
            parsed.as_str().to_string(),
            cancel,
            metrics,
            peer,
            TokioIo::new(io),
            overall,
            connect_deadline,
            owner,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_request<I>(
    request: http::Request<Empty<Bytes>>,
    url: String,
    cancel: &CancellationToken,
    metrics: &Metrics,
    peer: SocketAddr,
    io: I,
    overall: Instant,
    connect_deadline: Instant,
    owner: &Arc<DriverOwner>,
) -> Result<OpenedResponse, &'static str>
where
    I: hyper::rt::Read + hyper::rt::Write + Send + Unpin + 'static,
{
    metrics.record_connect(peer);
    let handshake = hyper::client::conn::http1::handshake(io);
    let (sender, connection) =
        crate::net::policy::select_deadline(cancel, connect_deadline, async {
            handshake.await.map_err(|_| "connect_failed")
        })
        .await?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    owner.note_spawn();
    let mut owned = Connection {
        driver: Some(driver),
        sender: Some(sender),
        owner: Some(Arc::clone(owner)),
    };
    let response = match crate::net::policy::select_deadline(cancel, overall, async {
        let sender = owned.sender.as_mut().ok_or("transport_failed")?;
        sender
            .send_request(request)
            .await
            .map_err(|_| "transport_failed")
    })
    .await
    {
        Ok(response) => response,
        Err(code) => {
            let _ = owned.shutdown(cleanup_deadline(overall)).await;
            return Err(code);
        }
    };
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get(http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let length = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    Ok(OpenedResponse {
        body: response.into_body(),
        content_length: length,
        status,
        url,
        location,
        headers,
        peer,
        connection: owned,
    })
}

async fn connect_plain(
    target: &ResolvedTarget,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(SocketAddr, TokioIo<TcpStream>), &'static str> {
    let tcp = crate::net::policy::select_deadline(cancel, deadline, async {
        TcpStream::connect(target.address)
            .await
            .map_err(|_| "connect_failed")
    })
    .await?;
    tcp.set_nodelay(true).map_err(|_| "connect_failed")?;
    let peer = tcp.peer_addr().map_err(|_| "connect_failed")?;
    Ok((peer, TokioIo::new(tcp)))
}

async fn connect_tls(
    target: &ResolvedTarget,
    cancel: &CancellationToken,
    deadline: Instant,
    hooks: &TransportHooks,
) -> Result<
    (
        SocketAddr,
        TokioIo<tokio_rustls::client::TlsStream<TcpStream>>,
    ),
    &'static str,
> {
    let tcp = crate::net::policy::select_deadline(cancel, deadline, async {
        TcpStream::connect(target.address)
            .await
            .map_err(|_| "connect_failed")
    })
    .await?;
    tcp.set_nodelay(true).map_err(|_| "connect_failed")?;
    let peer = tcp.peer_addr().map_err(|_| "connect_failed")?;
    let config = crate::net::connectors::rustls_client_config(hooks)?;
    let name = ServerName::try_from(target.host.clone()).map_err(|_| "policy_denied")?;
    let connector = TlsConnector::from(Arc::new(config));
    let tls = crate::net::policy::select_deadline(cancel, deadline, async {
        connector
            .connect(name, tcp)
            .await
            .map_err(|_| "connect_failed")
    })
    .await?;
    Ok((peer, TokioIo::new(tls)))
}

fn build_request(
    url: &Url,
    headers: &[HeaderEntry],
    absolute_form: bool,
) -> Result<http::Request<Empty<Bytes>>, &'static str> {
    let target = if absolute_form {
        url.as_str().to_string()
    } else {
        origin_form(url)
    };
    let mut request = http::Request::builder()
        .method(http::Method::GET)
        .uri(target)
        .header(http::header::HOST, host_header(url))
        .header(http::header::CONNECTION, "close")
        .header(http::header::USER_AGENT, "xeh-host");
    for header in headers {
        let name = http::HeaderName::from_bytes(&header.name).map_err(|_| "header_rejected")?;
        let value = http::HeaderValue::from_bytes(&header.value).map_err(|_| "header_rejected")?;
        request = request.header(name, value);
    }
    request
        .body(Empty::<Bytes>::new())
        .map_err(|_| "header_rejected")
}

fn origin_form(url: &Url) -> String {
    let mut path = url.path().to_string();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    path
}

fn host_header(url: &Url) -> String {
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    }
}

fn reject_headers(http: &vm::HttpConfig, headers: &[HeaderEntry]) -> Result<(), &'static str> {
    if headers.len() > http.max_request_header_count {
        return Err("header_rejected");
    }
    let mut total = 2usize;
    for header in headers {
        if header.name.is_empty()
            || header
                .name
                .iter()
                .any(|byte| *byte == b'\r' || *byte == b'\n')
        {
            return Err("header_rejected");
        }
        let name = std::str::from_utf8(&header.name).map_err(|_| "header_rejected")?;
        if is_hop_by_hop(name) {
            return Err("header_rejected");
        }
        if header
            .value
            .iter()
            .any(|byte| *byte == b'\r' || *byte == b'\n')
        {
            return Err("header_rejected");
        }
        total = total.saturating_add(
            header
                .name
                .len()
                .saturating_add(2)
                .saturating_add(header.value.len())
                .saturating_add(2),
        );
        if total > http.max_request_header_bytes {
            return Err("header_rejected");
        }
    }
    Ok(())
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
    )
}
