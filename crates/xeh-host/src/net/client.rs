use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

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

use crate::dto::{HeaderEntry, TransportHooks};
use crate::net::policy::{admit_target, hooks_decoy, ResolveLimits, ResolvedTarget};
use crate::observe::Metrics;

pub struct OpenedResponse {
    pub body: Incoming,
    pub content_length: Option<u64>,
    #[allow(dead_code)]
    pub peer: SocketAddr,
    pub connection: Connection,
}

pub struct Connection {
    driver: Option<JoinHandle<()>>,
    sender: Option<hyper::client::conn::http1::SendRequest<Empty<Bytes>>>,
}

impl Connection {
    pub fn abort(&mut self) {
        self.sender.take();
        if let Some(driver) = self.driver.as_ref() {
            driver.abort();
        }
    }

    /// Abort the owner driver and wait for that task to finish.
    /// A `false` result means the driver was still running at `deadline`.
    pub async fn shutdown(&mut self, deadline: Instant) -> bool {
        self.abort();
        let Some(mut driver) = self.driver.take() else {
            return true;
        };
        tokio::select! {
            biased;
            _ = &mut driver => true,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                self.driver = Some(driver);
                false
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.abort();
    }
}

pub async fn open_get(
    http: &vm::HttpConfig,
    url: &str,
    headers: &[HeaderEntry],
    cancel: &CancellationToken,
    metrics: &Metrics,
    hooks: &TransportHooks,
    started: Instant,
) -> Result<OpenedResponse, &'static str> {
    let parsed = Url::parse(url).map_err(|_| "policy_denied")?;
    reject_headers(http, headers)?;
    let overall = started + http.request_timeout.min(http.max_stream_duration);
    let connect_deadline = started + http.connect_timeout;
    let connect_deadline = connect_deadline.min(overall);
    let target = admit_target(
        http,
        &parsed,
        ResolveLimits {
            cancel,
            connect: connect_deadline,
            dns_hold: hooks.dns_hold.as_deref(),
            decoy: hooks_decoy(hooks),
        },
    )
    .await?;
    metrics.record_dns_candidates(&target.candidates);
    let https = parsed.scheme().eq_ignore_ascii_case("https");
    if https {
        let (peer, io) = connect_tls(&target, cancel, connect_deadline, hooks).await?;
        finish_request(
            parsed,
            headers,
            cancel,
            metrics,
            peer,
            io,
            overall,
            connect_deadline,
        )
        .await
    } else {
        let (peer, io) = connect_plain(&target, cancel, connect_deadline).await?;
        finish_request(
            parsed,
            headers,
            cancel,
            metrics,
            peer,
            io,
            overall,
            connect_deadline,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_request<I>(
    parsed: Url,
    headers: &[HeaderEntry],
    cancel: &CancellationToken,
    metrics: &Metrics,
    peer: SocketAddr,
    io: I,
    overall: Instant,
    connect_deadline: Instant,
) -> Result<OpenedResponse, &'static str>
where
    I: hyper::rt::Read + hyper::rt::Write + Send + Unpin + 'static,
{
    metrics.record_connect(peer);
    let handshake = hyper::client::conn::http1::handshake(io);
    let (mut sender, connection) =
        crate::net::policy::select_deadline(cancel, connect_deadline, async {
            handshake.await.map_err(|_| "connect_failed")
        })
        .await?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = http::Request::builder()
        .method(http::Method::GET)
        .uri(origin_form(&parsed))
        .header(http::header::HOST, host_header(&parsed))
        .header(http::header::CONNECTION, "close")
        .header(http::header::USER_AGENT, "xeh-host");
    for header in headers {
        let name = http::HeaderName::from_bytes(&header.name).map_err(|_| "header_rejected")?;
        let value = http::HeaderValue::from_bytes(&header.value).map_err(|_| "header_rejected")?;
        request = request.header(name, value);
    }
    let request = request
        .body(Empty::<Bytes>::new())
        .map_err(|_| "transport_failed")?;
    let response = crate::net::policy::select_deadline(cancel, overall, async {
        sender
            .send_request(request)
            .await
            .map_err(|_| "transport_failed")
    })
    .await?;
    let status = response.status();
    if status.is_redirection() {
        let mut connection = Connection {
            driver: Some(driver),
            sender: Some(sender),
        };
        let _ = connection.shutdown(overall).await;
        return Err("redirect_unsupported");
    }
    if !status.is_success() {
        let mut connection = Connection {
            driver: Some(driver),
            sender: Some(sender),
        };
        let _ = connection.shutdown(overall).await;
        return Err("http_status");
    }
    let length = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    Ok(OpenedResponse {
        body: response.into_body(),
        content_length: length,
        peer,
        connection: Connection {
            driver: Some(driver),
            sender: Some(sender),
        },
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
    let config = rustls_config(hooks)?;
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

fn rustls_config(hooks: &TransportHooks) -> Result<rustls::ClientConfig, &'static str> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for der in &hooks.extra_trust_der {
        let cert = rustls::pki_types::CertificateDer::from(der.clone());
        roots.add(cert).map_err(|_| "transport_failed")?;
    }
    Ok(
        rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .map_err(|_| "transport_failed")?
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
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
