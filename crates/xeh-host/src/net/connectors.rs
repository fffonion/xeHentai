//! Native proxy connectors for the protocols the source pool can configure.
//!
//! Only the generic protocol mechanics live here: TCP/TLS to the proxy
//! endpoint, HTTP CONNECT tunnelling, SOCKS4/4a/5/5h negotiation, and the
//! bounded reads those handshakes need. Route choice, scoring, URL policy and
//! Glype rewriting stay in the RSS application.
//!
//! Every phase runs under the caller's cancellation token and connection
//! deadline. A connector error is returned as a named code; no branch falls
//! back to a direct socket.

use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;

use crate::dto::TransportHooks;
use crate::net::policy::select_deadline;
use crate::net::route::{NativeProxyRoute, ProxyKind};

/// Largest HTTP CONNECT response header block accepted from a proxy.
const MAX_CONNECT_HEADER_BYTES: usize = 16 * 1024;
/// Largest SOCKS reply accepted (domain reply includes a one-byte length).
const MAX_SOCKS_REPLY_BYTES: usize = 300;

pub trait IoStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> IoStream for T {}
pub type BoxedIo = Box<dyn IoStream>;

pub fn boxed<T>(stream: T) -> BoxedIo
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    Box::new(stream)
}

pub fn rustls_client_config(hooks: &TransportHooks) -> Result<rustls::ClientConfig, &'static str> {
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

fn server_name(host: &str) -> Result<rustls::pki_types::ServerName<'static>, &'static str> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(rustls::pki_types::ServerName::IpAddress(ip.into()));
    }
    rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| "policy_denied")
}

/// TLS-wraps `stream`, validating `host`, under the caller's deadline.
pub async fn tls_wrap(
    stream: BoxedIo,
    host: &str,
    hooks: &TransportHooks,
    cancel: &CancellationToken,
    deadline: Instant,
    failure: &'static str,
) -> Result<BoxedIo, &'static str> {
    let config = rustls_client_config(hooks)?;
    let name = server_name(host)?;
    let connector = TlsConnector::from(std::sync::Arc::new(config));
    let tls = select_deadline(cancel, deadline, async {
        connector.connect(name, stream).await.map_err(|_| failure)
    })
    .await?;
    Ok(boxed(tls))
}

/// Connects to the configured proxy and validates its TLS layer with the
/// embedding's trust anchors (the fixture/production hook set).
#[allow(clippy::too_many_arguments)]
pub async fn connect_proxy_with_hooks(
    route: &NativeProxyRoute,
    hooks: &TransportHooks,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(BoxedIo, SocketAddr), &'static str> {
    if !route.kind.supported() {
        return Err("proxy_unsupported");
    }
    let addresses = select_deadline(cancel, deadline, async {
        if let Ok(ip) = route.host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(ip, route.port)]);
        }
        let looked = tokio::net::lookup_host((route.host.as_str(), route.port))
            .await
            .map_err(|_| "proxy_resolution_failed")?;
        let addresses: Vec<SocketAddr> = looked.collect();
        if addresses.is_empty() {
            return Err("proxy_resolution_failed");
        }
        Ok(addresses)
    })
    .await?;
    let mut last = "proxy_connect_failed";
    for address in addresses {
        let attempt = select_deadline(cancel, deadline, async {
            TcpStream::connect(address)
                .await
                .map_err(|_| "proxy_connect_failed")
        })
        .await;
        match attempt {
            Ok(tcp) => {
                tcp.set_nodelay(true).map_err(|_| "proxy_connect_failed")?;
                let peer = tcp.peer_addr().map_err(|_| "proxy_connect_failed")?;
                let mut stream: BoxedIo = boxed(tcp);
                if route.kind.outer_tls() {
                    stream = tls_wrap(
                        stream,
                        &route.host,
                        hooks,
                        cancel,
                        deadline,
                        "proxy_tls_failed",
                    )
                    .await?;
                }
                return Ok((stream, peer));
            }
            Err(code) => last = code,
        }
    }
    Err(last)
}

/// Sends one HTTP/1.1 CONNECT request and requires a 200 response.
///
/// The authority carries the target name and port; the placeholder Host is
/// the same authority. Response bytes are bounded and the whole exchange is
/// covered by the caller's deadline.
pub async fn http_connect(
    stream: &mut BoxedIo,
    host: &str,
    port: u16,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), &'static str> {
    let authority = host_header(host, port);
    let request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\nUser-Agent: xeh-host\r\n\r\n"
    );
    let read = select_deadline(cancel, deadline, async {
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|_| "proxy_handshake_failed")?;
        stream.flush().await.map_err(|_| "proxy_handshake_failed")?;
        read_bounded_headers(stream, MAX_CONNECT_HEADER_BYTES).await
    })
    .await?;
    let status = parse_connect_status(&read)?;
    if status != 200 {
        return Err("proxy_status");
    }
    Ok(())
}

fn host_header(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn parse_connect_status(bytes: &[u8]) -> Result<u16, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "proxy_handshake_failed")?;
    let line = text.split("\r\n").next().ok_or("proxy_handshake_failed")?;
    let mut parts = line.split_whitespace();
    let version = parts.next().ok_or("proxy_handshake_failed")?;
    if !version.starts_with("HTTP/1.") {
        return Err("proxy_handshake_failed");
    }
    parts
        .next()
        .ok_or("proxy_handshake_failed")?
        .parse::<u16>()
        .map_err(|_| "proxy_handshake_failed")
}

async fn read_bounded_headers(stream: &mut BoxedIo, limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut output = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let read = stream
            .read(&mut byte)
            .await
            .map_err(|_| "proxy_handshake_failed")?;
        if read == 0 {
            return Err("proxy_handshake_failed");
        }
        output.push(byte[0]);
        if output.len() > limit {
            return Err("proxy_handshake_failed");
        }
        if output.ends_with(b"\r\n\r\n") {
            return Ok(output);
        }
    }
}

async fn write_all(
    stream: &mut BoxedIo,
    bytes: &[u8],
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), &'static str> {
    select_deadline(cancel, deadline, async {
        stream
            .write_all(bytes)
            .await
            .map_err(|_| "proxy_handshake_failed")?;
        stream.flush().await.map_err(|_| "proxy_handshake_failed")
    })
    .await
}

async fn read_exact(
    stream: &mut BoxedIo,
    buffer: &mut [u8],
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), &'static str> {
    select_deadline(cancel, deadline, async {
        stream
            .read_exact(buffer)
            .await
            .map(|_| ())
            .map_err(|_| "proxy_handshake_failed")
    })
    .await
}

/// Negotiates SOCKS4/4a or SOCKS5/5h to `target`.
///
/// `local_ip` is the policy-admitted/pinned destination address for local
/// modes and `None` for delegated modes. Delegated modes carry the name to
/// the proxy; they never call the local resolver.
pub async fn socks_negotiate(
    stream: &mut BoxedIo,
    route: &NativeProxyRoute,
    target_host: &str,
    target_port: u16,
    local_ip: Option<IpAddr>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), &'static str> {
    match route.kind {
        ProxyKind::Socks4 | ProxyKind::Socks4a => {
            socks4_negotiate(
                stream,
                route,
                target_host,
                target_port,
                local_ip,
                cancel,
                deadline,
            )
            .await
        }
        ProxyKind::Socks5 | ProxyKind::Socks5h => {
            socks5_negotiate(
                stream,
                route,
                target_host,
                target_port,
                local_ip,
                cancel,
                deadline,
            )
            .await
        }
        _ => Err("proxy_unsupported"),
    }
}

async fn socks4_negotiate(
    stream: &mut BoxedIo,
    route: &NativeProxyRoute,
    target_host: &str,
    target_port: u16,
    local_ip: Option<IpAddr>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), &'static str> {
    let mut request = Vec::with_capacity(300);
    request.push(0x04u8);
    request.push(0x01);
    request.extend_from_slice(&target_port.to_be_bytes());
    let remote = matches!(route.kind, ProxyKind::Socks4a);
    let mut name: Option<String> = None;
    if remote {
        request.extend_from_slice(&[0, 0, 0, 1]);
        name = Some(target_host.to_string());
    } else {
        let address = match local_ip {
            Some(IpAddr::V4(ip)) => ip,
            _ => return Err("proxy_unsupported_target"),
        };
        request.extend_from_slice(&address.octets());
    }
    request.push(0);
    if let Some(name) = name {
        if name.is_empty() || name.len() > 255 {
            return Err("policy_denied");
        }
        request.extend_from_slice(name.as_bytes());
        request.push(0);
    }
    write_all(stream, &request, cancel, deadline).await?;
    let mut reply = [0u8; 8];
    read_exact(stream, &mut reply, cancel, deadline).await?;
    if reply[0] != 0x00 || reply[1] != 0x5a {
        return Err("proxy_status");
    }
    Ok(())
}

async fn socks5_negotiate(
    stream: &mut BoxedIo,
    route: &NativeProxyRoute,
    target_host: &str,
    target_port: u16,
    local_ip: Option<IpAddr>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(), &'static str> {
    write_all(stream, &[0x05, 0x01, 0x00], cancel, deadline).await?;
    let mut greeting = [0u8; 2];
    read_exact(stream, &mut greeting, cancel, deadline).await?;
    if greeting[0] != 0x05 {
        return Err("proxy_handshake_failed");
    }
    if greeting[1] != 0x00 {
        return Err("proxy_auth_required");
    }
    let mut request = Vec::with_capacity(300);
    request.extend_from_slice(&[0x05, 0x01, 0x00]);
    let remote = matches!(route.kind, ProxyKind::Socks5h);
    if remote {
        if target_host.is_empty() || target_host.len() > 255 {
            return Err("policy_denied");
        }
        request.push(0x03);
        request.push(target_host.len() as u8);
        request.extend_from_slice(target_host.as_bytes());
    } else {
        match local_ip {
            Some(IpAddr::V4(ip)) => {
                request.push(0x01);
                request.extend_from_slice(&ip.octets());
            }
            Some(IpAddr::V6(ip)) => {
                request.push(0x04);
                request.extend_from_slice(&ip.octets());
            }
            None => return Err("proxy_unsupported_target"),
        }
    }
    request.extend_from_slice(&target_port.to_be_bytes());
    write_all(stream, &request, cancel, deadline).await?;
    let mut head = [0u8; 4];
    read_exact(stream, &mut head, cancel, deadline).await?;
    if head[0] != 0x05 {
        return Err("proxy_handshake_failed");
    }
    if head[1] != 0x00 {
        return Err("proxy_status");
    }
    let remaining = match head[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => {
            let mut length = [0u8; 1];
            read_exact(stream, &mut length, cancel, deadline).await?;
            usize::from(length[0]) + 2
        }
        _ => return Err("proxy_handshake_failed"),
    };
    if remaining + head.len() > MAX_SOCKS_REPLY_BYTES {
        return Err("proxy_handshake_failed");
    }
    let mut tail = vec![0u8; remaining];
    read_exact(stream, &mut tail, cancel, deadline).await?;
    Ok(())
}
