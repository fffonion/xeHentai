//! Native proxy transport fixtures (included by `tests/transport.rs`).
//!
//! Every fixture is a local server speaking the real protocol on a loopback
//! socket. Proxy servers record the received request target, the negotiated
//! destination address, and the bytes they forward; target servers record the
//! requests they receive. Tests assert both sides and the staged file bytes.

#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use url::Url;
use vm::HttpConfig;

use xeh_host::{
    open_get_with_options, DriverOwner, Metrics, NativeProxyRoute, OpenOptions, ProxyKind,
    TransportHooks, WorkerPool,
};

use super::{pool_config, request as base_request};
use crate::support::TestRoot;

#[derive(Clone)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn ok(body: &[u8]) -> Self {
        Self {
            status: 200,
            headers: vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            )],
            body: body.to_vec(),
        }
    }

    fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".to_string(), location.to_string())],
            body: Vec::new(),
        }
    }
}

struct Count {
    count: Mutex<usize>,
    cv: Condvar,
}

impl Count {
    fn new() -> Self {
        Self {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn signal(&self) {
        *self.count.lock().expect("count") += 1;
        self.cv.notify_all();
    }

    fn wait(&self, expected: usize, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut count = self.count.lock().expect("count");
        while *count < expected {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("count waited for {expected}, saw {}", *count));
            }
            let (next, wait) = self.cv.wait_timeout(count, remaining).expect("wait");
            count = next;
            if wait.timed_out() && *count < expected {
                return Err(format!("count waited for {expected}, saw {}", *count));
            }
        }
        Ok(())
    }

    fn value(&self) -> usize {
        *self.count.lock().expect("count")
    }
}

fn read_header_block<R: Read>(reader: &mut R) -> Vec<u8> {
    let mut output = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) | Err(_) => return output,
            Ok(_) => output.push(byte[0]),
        }
        if output.ends_with(b"\r\n\r\n") || output.len() > 16 * 1024 {
            return output;
        }
    }
}

fn first_line(headers: &[u8]) -> String {
    String::from_utf8_lossy(headers)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn header_value(headers: &[u8], name: &str) -> Option<String> {
    for line in String::from_utf8_lossy(headers).lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case(name) {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn split_authority(authority: &str) -> Option<(String, u16)> {
    let (host, port) = authority.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some((host.to_string(), port.parse().ok()?))
}

fn connect_target(
    mapping: &HashMap<String, SocketAddr>,
    host: &str,
    port: u16,
) -> Option<TcpStream> {
    if let Some(address) = mapping.get(host) {
        return TcpStream::connect(address).ok();
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return TcpStream::connect(SocketAddr::new(ip, port)).ok();
    }
    mapping
        .get(&format!("{host}:{port}"))
        .and_then(|address| TcpStream::connect(address).ok())
}

fn duplex(mut left: TcpStream, mut right: TcpStream) {
    let _ = left.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = right.set_read_timeout(Some(Duration::from_secs(10)));
    let (mut left_read, mut right_read) = (
        left.try_clone().expect("left clone"),
        right.try_clone().expect("right clone"),
    );
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _ = std::io::copy(&mut left_read, &mut right);
            let _ = right.shutdown(std::net::Shutdown::Write);
        });
        let _ = std::io::copy(&mut right_read, &mut left);
        let _ = left.shutdown(std::net::Shutdown::Write);
    });
}

fn rewrite_to_origin(headers: &[u8], origin: &str) -> Vec<u8> {
    let mut output = format!("GET {origin} HTTP/1.1\r\n").into_bytes();
    let text = String::from_utf8_lossy(headers);
    for line in text.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        if line.to_ascii_lowercase().starts_with("proxy-connection:") {
            continue;
        }
        output.extend_from_slice(line.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    output.extend_from_slice(b"\r\n");
    output
}

struct ProxyFixture {
    address: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
    accepts: Arc<Count>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProxyFixture {
    fn log(&self) -> Vec<String> {
        self.log.lock().expect("proxy log").clone()
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum FixtureKind {
    Http,
    Https,
    Socks4,
    Socks4a,
    Socks5,
    Socks5h,
}

fn tls_pair(san: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["fixture-ca".to_string()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf_params = rcgen::CertificateParams::new(vec![san.to_string()]).expect("leaf params");
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("leaf");
    (
        ca_cert.der().to_vec(),
        leaf.der().to_vec(),
        leaf_key.serialize_der(),
    )
}

fn server_config(leaf: Vec<u8>, key: Vec<u8>) -> Arc<rustls::ServerConfig> {
    let config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .expect("versions")
    .with_no_client_auth()
    .with_single_cert(
        vec![rustls::pki_types::CertificateDer::from(leaf)],
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(key)),
    )
    .expect("server cert");
    Arc::new(config)
}

fn spawn_proxy(
    kind: FixtureKind,
    mapping: Arc<HashMap<String, SocketAddr>>,
    tls: Option<(Vec<u8>, Vec<u8>)>,
) -> ProxyFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let log = Arc::new(Mutex::new(Vec::new()));
    let accepts = Arc::new(Count::new());
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let log = Arc::clone(&log);
        let accepts = Arc::clone(&accepts);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let config = tls.map(|(leaf, key)| server_config(leaf, key));
            for stream in listener.incoming() {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let Ok(mut stream) = stream else {
                    return;
                };
                accepts.signal();
                let log = Arc::clone(&log);
                let mapping = Arc::clone(&mapping);
                let config = config.clone();
                std::thread::spawn(move || match kind {
                    FixtureKind::Http => handle_http_proxy(stream, &mapping, &log),
                    FixtureKind::Https => {
                        let Some(config) = config else { return };
                        handle_https_proxy(&mut stream, config, &mapping, &log);
                    }
                    _ => {
                        let _ = handle_socks(&mut stream, kind, &mapping, &log);
                    }
                });
            }
        })
    };
    ProxyFixture {
        address,
        log,
        accepts,
        stop,
        handle: Some(handle),
    }
}

fn handle_http_proxy(
    mut client: TcpStream,
    mapping: &HashMap<String, SocketAddr>,
    log: &Mutex<Vec<String>>,
) {
    let _ = client.set_read_timeout(Some(Duration::from_secs(10)));
    let headers = read_header_block(&mut client);
    if headers.is_empty() {
        return;
    }
    let line = first_line(&headers);
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    log.lock()
        .expect("log")
        .push(format!("request {method} {target}"));
    log.lock().expect("log").push(format!(
        "headers {}",
        String::from_utf8_lossy(&headers).replace("\r\n", " | ")
    ));
    if method == "CONNECT" {
        let Some((host, port)) = split_authority(&target) else {
            return;
        };
        log.lock()
            .expect("log")
            .push(format!("connect {host}:{port}"));
        let Some(target_stream) = connect_target(mapping, &host, port) else {
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
            return;
        };
        let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
        duplex(client, target_stream);
        return;
    }
    let Ok(url) = Url::parse(&target) else {
        return;
    };
    let host = url.host_str().unwrap_or("").to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    log.lock()
        .expect("log")
        .push(format!("connect {host}:{port}"));
    let Some(mut target_stream) = connect_target(mapping, &host, port) else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    let mut origin = url.path().to_string();
    if let Some(query) = url.query() {
        origin.push('?');
        origin.push_str(query);
    }
    let request = rewrite_to_origin(&headers, &origin);
    let _ = target_stream.write_all(&request);
    let _ = target_stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = std::io::copy(&mut target_stream, &mut client);
}

fn handle_https_proxy(
    stream: &mut TcpStream,
    config: Arc<rustls::ServerConfig>,
    mapping: &HashMap<String, SocketAddr>,
    log: &Mutex<Vec<String>>,
) {
    let socket = stream.try_clone().ok();
    let mut conn = rustls::ServerConnection::new(config).expect("server connection");
    let mut tls = rustls::Stream::new(&mut conn, stream);
    handle_http_proxy_tls(&mut tls, mapping, log, socket.as_ref());
}

fn handle_http_proxy_tls(
    tls: &mut rustls::Stream<'_, rustls::ServerConnection, TcpStream>,
    mapping: &HashMap<String, SocketAddr>,
    log: &Mutex<Vec<String>>,
    socket: Option<&TcpStream>,
) {
    let headers = read_header_block(tls);
    if headers.is_empty() {
        return;
    }
    let line = first_line(&headers);
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    log.lock()
        .expect("log")
        .push(format!("request {method} {target}"));
    log.lock().expect("log").push(format!(
        "tls-headers {}",
        String::from_utf8_lossy(&headers).replace("\r\n", " | ")
    ));
    if method == "CONNECT" {
        let Some((host, port)) = split_authority(&target) else {
            return;
        };
        log.lock()
            .expect("log")
            .push(format!("connect {host}:{port}"));
        let Some(mut target_stream) = connect_target(mapping, &host, port) else {
            return;
        };
        let _ = tls.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
        let _ = tls.flush();
        relay_tls_tcp(tls, &mut target_stream, socket);
        return;
    }
    let Ok(url) = Url::parse(&target) else {
        return;
    };
    let host = url.host_str().unwrap_or("").to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    log.lock()
        .expect("log")
        .push(format!("connect {host}:{port}"));
    let Some(mut target_stream) = connect_target(mapping, &host, port) else {
        return;
    };
    let mut origin = url.path().to_string();
    if let Some(query) = url.query() {
        origin.push('?');
        origin.push_str(query);
    }
    let request = rewrite_to_origin(&headers, &origin);
    let _ = target_stream.write_all(&request);
    let _ = std::io::copy(&mut target_stream, tls);
}

/// Relays decrypted tunnel bytes between an outer-TLS stream and a TCP target.
///
/// The tunnel carries a second TLS session, so the fixture never parses it.
/// Short read timeouts let the single-threaded loop alternate directions; a
/// quiet period does not fabricate progress and both ends are closed.
fn relay_tls_tcp(
    tls: &mut rustls::Stream<'_, rustls::ServerConnection, TcpStream>,
    target: &mut TcpStream,
    socket: Option<&TcpStream>,
) {
    let _ = target.set_read_timeout(Some(Duration::from_millis(200)));
    if let Some(socket) = socket {
        let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
    }
    let mut buffer = [0u8; 8192];
    let mut idle = 0usize;
    loop {
        let read = tls.read(&mut buffer);
        match read {
            Ok(0) => return,
            Ok(n) => {
                idle = 0;
                if target.write_all(&buffer[..n]).is_err() {
                    return;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
        match target.read(&mut buffer) {
            Ok(0) => {
                let _ = tls.flush();
                return;
            }
            Ok(n) => {
                idle = 0;
                if tls.write_all(&buffer[..n]).is_err() {
                    return;
                }
                let _ = tls.flush();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
        idle += 1;
        if idle > 200 {
            return;
        }
    }
}

fn read_exact_n(stream: &mut TcpStream, count: usize) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; count];
    stream.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

fn read_until_nul(stream: &mut TcpStream, limit: usize) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).ok()?;
        if byte[0] == 0 {
            return Some(output);
        }
        output.push(byte[0]);
        if output.len() > limit {
            return None;
        }
    }
}

fn handle_socks(
    client: &mut TcpStream,
    kind: FixtureKind,
    mapping: &HashMap<String, SocketAddr>,
    log: &Mutex<Vec<String>>,
) -> Option<()> {
    let _ = client.set_read_timeout(Some(Duration::from_secs(10)));
    match kind {
        FixtureKind::Socks4 | FixtureKind::Socks4a => {
            let head = read_exact_n(client, 8)?;
            if head[0] != 4 || head[1] != 1 {
                return None;
            }
            let port = u16::from_be_bytes([head[2], head[3]]);
            let ip = std::net::Ipv4Addr::new(head[4], head[5], head[6], head[7]);
            let _user = read_until_nul(client, 255)?;
            let host = if kind == FixtureKind::Socks4a && ip.octets() == [0, 0, 0, 1] {
                let name = read_until_nul(client, 255)?;
                String::from_utf8_lossy(&name).to_string()
            } else {
                ip.to_string()
            };
            log.lock()
                .expect("log")
                .push(format!("socks4 {host}:{port}"));
            let Some(target) = connect_target(mapping, &host, port) else {
                let _ = client.write_all(&[0, 0x5b, 0, 0, 0, 0, 0, 0]);
                return None;
            };
            let _ = client.write_all(&[0, 0x5a, 0, 0, 0, 0, 0, 0]);
            duplex(client.try_clone().expect("client clone"), target);
            Some(())
        }
        FixtureKind::Socks5 | FixtureKind::Socks5h => {
            let greeting = read_exact_n(client, 2)?;
            if greeting[0] != 5 {
                return None;
            }
            let methods = read_exact_n(client, greeting[1] as usize)?;
            if !methods.contains(&0) {
                let _ = client.write_all(&[5, 0xff]);
                return None;
            }
            let _ = client.write_all(&[5, 0]);
            let head = read_exact_n(client, 4)?;
            if head[0] != 5 || head[1] != 1 {
                return None;
            }
            let (host, port) = match head[3] {
                1 => {
                    let address = read_exact_n(client, 4)?;
                    let port = read_exact_n(client, 2)?;
                    let host =
                        std::net::Ipv4Addr::new(address[0], address[1], address[2], address[3])
                            .to_string();
                    let port = u16::from_be_bytes([port[0], port[1]]);
                    log.lock()
                        .expect("log")
                        .push(format!("socks5 atyp1 {host}:{port}"));
                    (host, port)
                }
                3 => {
                    let length = read_exact_n(client, 1)?;
                    let name = read_exact_n(client, length[0] as usize)?;
                    let port = read_exact_n(client, 2)?;
                    let host = String::from_utf8_lossy(&name).to_string();
                    let port = u16::from_be_bytes([port[0], port[1]]);
                    log.lock()
                        .expect("log")
                        .push(format!("socks5 atyp3 {host}:{port}"));
                    (host, port)
                }
                4 => {
                    let address = read_exact_n(client, 16)?;
                    let mut octets = [0u8; 16];
                    octets.copy_from_slice(&address);
                    let port = read_exact_n(client, 2)?;
                    let host = std::net::Ipv6Addr::from(octets).to_string();
                    let port = u16::from_be_bytes([port[0], port[1]]);
                    log.lock()
                        .expect("log")
                        .push(format!("socks5 atyp4 {host}:{port}"));
                    (host, port)
                }
                _ => return None,
            };
            let Some(target) = connect_target(mapping, &host, port) else {
                let _ = client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]);
                return None;
            };
            let _ = client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
            duplex(client.try_clone().expect("clone"), target);
            Some(())
        }
        _ => None,
    }
}

type TargetFixture = (u16, Arc<Count>, Arc<Mutex<Vec<String>>>, Arc<AtomicBool>);

fn spawn_target(script: Arc<Mutex<VecDeque<Resp>>>) -> TargetFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("target listener");
    let port = listener.local_addr().expect("target address").port();
    let count = Arc::new(Count::new());
    let log = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let count = Arc::clone(&count);
        let log = Arc::clone(&log);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let Ok(mut stream) = stream else {
                    return;
                };
                count.signal();
                let response = script
                    .lock()
                    .expect("script")
                    .pop_front()
                    .unwrap_or_else(|| Resp::ok(b"ok"));
                let headers = read_header_block(&mut stream);
                log.lock().expect("target log").push(format!(
                    "request {} host={}",
                    first_line(&headers),
                    header_value(&headers, "host").unwrap_or_default()
                ));
                let mut head = format!("HTTP/1.1 {} X\r\n", response.status);
                for (name, value) in &response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    response.body.len()
                ));
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&response.body);
            }
        });
    }
    (port, count, log, stop)
}

fn spawn_tls_target(
    script: Arc<Mutex<VecDeque<Resp>>>,
    leaf: Vec<u8>,
    key: Vec<u8>,
) -> TargetFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("tls target listener");
    let port = listener.local_addr().expect("tls target address").port();
    let count = Arc::new(Count::new());
    let log = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let count = Arc::clone(&count);
        let log = Arc::clone(&log);
        let stop = Arc::clone(&stop);
        let config = server_config(leaf, key);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let Ok(mut stream) = stream else {
                    return;
                };
                count.signal();
                let mut first = [0u8; 5];
                let peeked = stream.peek(&mut first).unwrap_or(0);
                log.lock().expect("tls log").push(format!(
                    "first-bytes {}",
                    first[..peeked]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                ));
                let response = script
                    .lock()
                    .expect("script")
                    .pop_front()
                    .unwrap_or_else(|| Resp::ok(b"ok"));
                let mut conn =
                    rustls::ServerConnection::new(Arc::clone(&config)).expect("server conn");
                let mut tls = rustls::Stream::new(&mut conn, &mut stream);
                let headers = read_header_block(&mut tls);
                if headers.is_empty() {
                    continue;
                }
                log.lock().expect("tls log").push(format!(
                    "request {} host={}",
                    first_line(&headers),
                    header_value(&headers, "host").unwrap_or_default()
                ));
                let mut head = format!("HTTP/1.1 {} X\r\n", response.status);
                for (name, value) in &response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    response.body.len()
                ));
                let _ = tls.write_all(head.as_bytes());
                let _ = tls.write_all(&response.body);
                let _ = tls.flush();
            }
        });
    }
    (port, count, log, stop)
}

fn allow_http(hosts: Vec<&str>, ports: Vec<u16>) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string(), "https".to_string()],
        allowed_hosts: hosts.into_iter().map(str::to_string).collect(),
        allowed_ports: ports,
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(15),
        stream_idle_timeout: Duration::from_secs(5),
        max_stream_duration: Duration::from_secs(15),
        ..HttpConfig::default()
    }
}

fn run_open(
    http: &HttpConfig,
    url: &str,
    route: Option<NativeProxyRoute>,
    follow_redirects: bool,
    hooks: &TransportHooks,
) -> Result<(u16, Vec<u8>), &'static str> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let metrics = Metrics::default();
        let owner = Arc::new(DriverOwner::new());
        let cancel = CancellationToken::new();
        let mut opened = open_get_with_options(
            http,
            url,
            &[],
            &cancel,
            &metrics,
            hooks,
            &owner,
            Instant::now(),
            OpenOptions::get(route.as_ref(), follow_redirects, false),
        )
        .await?;
        let mut body = Vec::new();
        while let Some(frame) = opened.body.frame().await {
            let frame = frame.map_err(|_| "transport_failed")?;
            if let Ok(data) = frame.into_data() {
                body.extend_from_slice(&data);
                if body.len() > 1_000_000 {
                    return Err("body_limit");
                }
            }
        }
        let _ = opened
            .connection
            .shutdown(Instant::now() + Duration::from_secs(2))
            .await;
        assert_eq!(owner.active(), 0, "driver left active");
        Ok((opened.status, body))
    })
}

fn route(kind: ProxyKind, fixture: &ProxyFixture, trusted: bool) -> NativeProxyRoute {
    NativeProxyRoute::new(
        kind,
        fixture.address.ip().to_string(),
        fixture.address.port(),
        trusted,
    )
    .expect("route")
}

fn assert_no_accepts(count: &Count, label: &str) {
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(count.value(), 0, "{label} observed a connection");
}

#[test]
fn http_proxy_sends_absolute_form_and_streams_target_body() {
    let root = TestRoot::create();
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"proxied-body")])));
    let (target_port, _target_count, target_log, target_stop) = spawn_target(script);
    let mut mapping = HashMap::new();
    mapping.insert(
        "target.invalid".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    let proxy = spawn_proxy(FixtureKind::Http, Arc::new(mapping), None);
    let http = allow_http(vec!["target.invalid"], vec![target_port]);
    let url = format!("http://target.invalid:{target_port}/page.bin");
    let (status, body) = run_open(
        &http,
        &url,
        Some(route(ProxyKind::Http, &proxy, true)),
        false,
        &TransportHooks::default(),
    )
    .expect("open");
    assert_eq!(status, 200);
    assert_eq!(body, b"proxied-body");
    let log = proxy.log();
    assert!(log[0].contains("GET http://target.invalid:"), "log {log:?}");
    assert!(target_log
        .lock()
        .expect("target")
        .iter()
        .any(|line| line.contains("GET /page.bin")));
    target_stop.store(true, Ordering::Release);
    proxy.stop();
    drop(root);
}

#[test]
fn http_connect_tunnels_https_target_tls() {
    let root = TestRoot::create();
    let (ca, leaf, key) = tls_pair("127.0.0.1");
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"tls-proxied")])));
    let (target_port, _count, target_log, _stop) = spawn_tls_target(script, leaf, key);
    let mut mapping = HashMap::new();
    mapping.insert(
        "127.0.0.1".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    let proxy = spawn_proxy(FixtureKind::Http, Arc::new(mapping), None);
    let http = allow_http(vec!["127.0.0.1"], vec![target_port]);
    let hooks = TransportHooks {
        extra_trust_der: vec![ca],
        ..TransportHooks::default()
    };
    let url = format!("https://127.0.0.1:{target_port}/secure.bin");
    let (status, body) = run_open(
        &http,
        &url,
        Some(route(ProxyKind::Http, &proxy, true)),
        false,
        &hooks,
    )
    .expect("open");
    assert_eq!(status, 200);
    assert_eq!(body, b"tls-proxied");
    let log = proxy.log();
    assert!(
        log.iter()
            .any(|line| line.starts_with("connect 127.0.0.1:")),
        "log {log:?}"
    );
    assert!(target_log
        .lock()
        .expect("tls target")
        .iter()
        .any(|line| line.contains("GET /secure.bin")));
    proxy.stop();
    drop(root);
}

#[test]
fn https_proxy_outer_tls_plain_http_uses_absolute_form() {
    let root = TestRoot::create();
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"outer-tls")])));
    let (target_port, _count, target_log, _stop) = spawn_target(script);
    let mut mapping = HashMap::new();
    mapping.insert(
        "origin.invalid".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    let (ca, leaf, key) = tls_pair("127.0.0.1");
    let proxy = spawn_proxy(FixtureKind::Https, Arc::new(mapping), Some((leaf, key)));
    let hooks = TransportHooks {
        extra_trust_der: vec![ca],
        ..TransportHooks::default()
    };
    let http = allow_http(vec!["origin.invalid"], vec![target_port]);
    let url = format!("http://origin.invalid:{target_port}/plain.bin");
    let (status, body) = run_open(
        &http,
        &url,
        Some(route(ProxyKind::Https, &proxy, true)),
        false,
        &hooks,
    )
    .expect("open");
    assert_eq!(status, 200);
    assert_eq!(body, b"outer-tls");
    let log = proxy.log();
    assert!(
        log.iter()
            .any(|line| line.contains("GET http://origin.invalid:")),
        "log {log:?}"
    );
    assert!(target_log
        .lock()
        .expect("target")
        .iter()
        .any(|line| line.contains("GET /plain.bin")));
    proxy.stop();
    drop(root);
}

#[test]
fn https_proxy_outer_tls_connect_inner_tls_completes() {
    let root = TestRoot::create();
    let (outer_ca, outer_leaf, outer_key) = tls_pair("127.0.0.1");
    let (inner_ca, inner_leaf, inner_key) = tls_pair("127.0.0.1");
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"double-tls")])));
    let (target_port, _count, target_log, _stop) = spawn_tls_target(script, inner_leaf, inner_key);
    let mut mapping = HashMap::new();
    mapping.insert(
        "127.0.0.1".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    let proxy = spawn_proxy(
        FixtureKind::Https,
        Arc::new(mapping),
        Some((outer_leaf, outer_key)),
    );
    let hooks = TransportHooks {
        extra_trust_der: vec![outer_ca, inner_ca],
        ..TransportHooks::default()
    };
    let http = allow_http(vec!["127.0.0.1"], vec![target_port]);
    let url = format!("https://127.0.0.1:{target_port}/double.bin");
    let (status, body) = run_open(
        &http,
        &url,
        Some(route(ProxyKind::Https, &proxy, true)),
        false,
        &hooks,
    )
    .expect("open");
    assert_eq!(status, 200);
    assert_eq!(body, b"double-tls");
    let log = proxy.log();
    assert!(
        log.iter()
            .any(|line| line.starts_with("connect 127.0.0.1:")),
        "log {log:?}"
    );
    assert!(target_log
        .lock()
        .expect("tls target")
        .iter()
        .any(|line| line.contains("GET /double.bin")));
    proxy.stop();
    drop(root);
}

fn socks_case(kind: FixtureKind, proxy_kind: ProxyKind, host: &str) {
    let root = TestRoot::create();
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"socks-body")])));
    let (target_port, _count, target_log, _stop) = spawn_target(script);
    let mut mapping = HashMap::new();
    mapping.insert(
        host.to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    mapping.insert(
        "127.0.0.1".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    let proxy = spawn_proxy(kind, Arc::new(mapping), None);
    let http = allow_http(vec![host], vec![target_port]);
    let url = format!("http://{host}:{target_port}/socks.bin");
    let (status, body) = run_open(
        &http,
        &url,
        Some(route(proxy_kind, &proxy, true)),
        false,
        &TransportHooks::default(),
    )
    .expect("open");
    assert_eq!(status, 200);
    assert_eq!(body, b"socks-body");
    let log = proxy.log();
    assert!(!log.is_empty(), "{kind:?} proxy saw no handshake");
    assert!(target_log
        .lock()
        .expect("target")
        .iter()
        .any(|line| line.contains("GET /socks.bin")));
    match kind {
        FixtureKind::Socks4 => assert!(
            log.iter().any(|line| line.contains("socks4 127.0.0.1:")),
            "{log:?}"
        ),
        FixtureKind::Socks4a => assert!(
            log.iter()
                .any(|line| line.contains("socks4 mapped.invalid:")),
            "{log:?}"
        ),
        FixtureKind::Socks5 => assert!(
            log.iter()
                .any(|line| line.contains("socks5 atyp1 127.0.0.1:")),
            "{log:?}"
        ),
        FixtureKind::Socks5h => assert!(
            log.iter()
                .any(|line| line.contains("socks5 atyp3 mapped.invalid:")),
            "{log:?}"
        ),
        _ => unreachable!(),
    }
    proxy.stop();
    drop(root);
}

#[test]
fn socks4_uses_local_resolution() {
    socks_case(FixtureKind::Socks4, ProxyKind::Socks4, "127.0.0.1");
}

#[test]
fn socks4a_sends_remote_domain() {
    socks_case(FixtureKind::Socks4a, ProxyKind::Socks4a, "mapped.invalid");
}

#[test]
fn socks5_uses_local_resolution() {
    socks_case(FixtureKind::Socks5, ProxyKind::Socks5, "127.0.0.1");
}

#[test]
fn socks5h_sends_remote_domain_without_local_lookup() {
    socks_case(FixtureKind::Socks5h, ProxyKind::Socks5h, "mapped.invalid");
}

#[test]
fn remote_dns_requires_explicit_embedding_grant() {
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"never")])));
    let (target_port, target_count, _log, _stop) = spawn_target(script);
    let proxy = spawn_proxy(FixtureKind::Socks5h, Arc::new(HashMap::new()), None);
    let http = allow_http(vec!["mapped.invalid"], vec![target_port]);
    let url = format!("http://mapped.invalid:{target_port}/denied");
    let error = run_open(
        &http,
        &url,
        Some(route(ProxyKind::Socks5h, &proxy, false)),
        false,
        &TransportHooks::default(),
    )
    .expect_err("remote DNS without a grant must fail closed");
    assert_eq!(error, "policy_denied");
    assert_no_accepts(&proxy.accepts, "proxy");
    assert_no_accepts(&target_count, "target");
    proxy.stop();
}

#[test]
fn route_required_failure_never_falls_back_direct() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("trap");
    let trap_port = listener.local_addr().expect("trap addr").port();
    let trap_count = Arc::new(AtomicUsize::new(0));
    {
        let trap_count = Arc::clone(&trap_count);
        std::thread::spawn(move || {
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                trap_count.fetch_add(1, Ordering::AcqRel);
                let _ = read_header_block(&mut stream);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntrap",
                );
            }
        });
    }
    // A route to a closed proxy port on the same loopback range.
    let closed = TcpListener::bind("127.0.0.1:0").expect("closed");
    let closed_port = closed.local_addr().expect("closed addr").port();
    drop(closed);
    let failed =
        NativeProxyRoute::new(ProxyKind::Http, "127.0.0.1", closed_port, true).expect("route");
    let http = allow_http(vec!["127.0.0.1"], vec![trap_port]);
    let url = format!("http://127.0.0.1:{trap_port}/must-not-direct");
    let error = run_open(&http, &url, Some(failed), false, &TransportHooks::default())
        .expect_err("proxy failure must not fall back");
    assert!(
        error == "proxy_connect_failed" || error == "deadline",
        "unexpected {error}"
    );
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        trap_count.load(Ordering::Acquire),
        0,
        "direct trap saw a connection"
    );
    drop(root);
}

#[test]
fn environment_proxy_variables_are_ignored() {
    let root = TestRoot::create();
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"direct-ok")])));
    let (port, _count, _log, _stop) = spawn_target(script);
    let listener = TcpListener::bind("127.0.0.1:0").expect("env trap");
    let trap_port = listener.local_addr().expect("env trap addr").port();
    let trap_count = Arc::new(AtomicUsize::new(0));
    {
        let trap_count = Arc::clone(&trap_count);
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                trap_count.fetch_add(1, Ordering::AcqRel);
                let _ = read_header_block(&mut stream);
            }
        });
    }
    std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{trap_port}"));
    std::env::set_var("HTTPS_PROXY", format!("http://127.0.0.1:{trap_port}"));
    std::env::set_var("ALL_PROXY", format!("socks5h://127.0.0.1:{trap_port}"));
    std::env::set_var("NO_PROXY", "");
    let http = allow_http(vec!["127.0.0.1"], vec![port]);
    let url = format!("http://127.0.0.1:{port}/env.bin");
    let (status, body) =
        run_open(&http, &url, None, false, &TransportHooks::default()).expect("direct");
    assert_eq!(status, 200);
    assert_eq!(body, b"direct-ok");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        trap_count.load(Ordering::Acquire),
        0,
        "environment proxy was consulted"
    );
    std::env::remove_var("HTTP_PROXY");
    std::env::remove_var("HTTPS_PROXY");
    std::env::remove_var("ALL_PROXY");
    std::env::remove_var("NO_PROXY");
    drop(root);
}

#[test]
fn redirect_follow_reauthorizes_and_strips_cross_origin_headers() {
    let script = Arc::new(Mutex::new(VecDeque::new()));
    let (port, _count, target_log, _stop) = spawn_target(Arc::clone(&script));
    let mut mapping = HashMap::new();
    for host in ["one.invalid", "other.invalid"] {
        mapping.insert(
            host.to_string(),
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port),
        );
    }
    script
        .lock()
        .expect("script")
        .push_back(Resp::redirect(&format!(
            "http://other.invalid:{port}/final"
        )));
    script
        .lock()
        .expect("script")
        .push_back(Resp::ok(b"final-body"));
    let proxy = spawn_proxy(FixtureKind::Http, Arc::new(mapping), None);
    let http = allow_http(vec!["one.invalid", "other.invalid"], vec![port]);
    let url = format!("http://one.invalid:{port}/start");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (status, body) = runtime
        .block_on(async {
            let metrics = Metrics::default();
            let owner = Arc::new(DriverOwner::new());
            let cancel = CancellationToken::new();
            let headers = vec![xeh_host::HeaderEntry {
                name: b"authorization".to_vec(),
                value: b"Bearer secret".to_vec(),
            }];
            let mut opened = open_get_with_options(
                &http,
                &url,
                &headers,
                &cancel,
                &metrics,
                &TransportHooks::default(),
                &owner,
                Instant::now(),
                OpenOptions::get(Some(&route(ProxyKind::Http, &proxy, true)), true, false),
            )
            .await
            .expect("open");
            let mut body = Vec::new();
            while let Some(frame) = opened.body.frame().await {
                let frame = frame.map_err(|_| "transport_failed").expect("frame");
                if let Ok(data) = frame.into_data() {
                    body.extend_from_slice(&data);
                }
            }
            let _ = opened
                .connection
                .shutdown(Instant::now() + Duration::from_secs(2))
                .await;
            Ok::<(u16, Vec<u8>), &'static str>((opened.status, body))
        })
        .expect("follow");
    assert_eq!(status, 200);
    assert_eq!(body, b"final-body");
    let log = proxy.log();
    let requests: Vec<&String> = log
        .iter()
        .filter(|line| line.starts_with("request GET"))
        .collect();
    assert_eq!(requests.len(), 2, "proxy did not retain the route: {log:?}");
    assert!(requests[0].contains("http://one.invalid:"), "{log:?}");
    assert!(requests[1].contains("http://other.invalid:"), "{log:?}");
    let header_lines: Vec<&String> = log
        .iter()
        .filter(|line| line.starts_with("headers "))
        .collect();
    assert_eq!(header_lines.len(), 2, "{log:?}");
    assert!(header_lines[0]
        .to_ascii_lowercase()
        .contains("authorization"));
    assert!(!header_lines[1]
        .to_ascii_lowercase()
        .contains("authorization"));
    let targets = target_log.lock().expect("target");
    assert!(
        targets.iter().any(|line| line.contains("GET /final")),
        "{targets:?}"
    );
    drop(targets);
    proxy.stop();
}

#[test]
fn default_entry_still_rejects_redirects() {
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::redirect("/final")])));
    let (port, _count, _log, _stop) = spawn_target(script);
    let http = allow_http(vec!["127.0.0.1"], vec![port]);
    let url = format!("http://127.0.0.1:{port}/start");
    let error =
        run_open(&http, &url, None, false, &TransportHooks::default()).expect_err("redirect");
    assert_eq!(error, "redirect_unsupported");
}

#[test]
fn outer_tls_failure_is_distinct_and_never_speaks_http_to_the_target() {
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"never")])));
    let (target_port, target_count, _log, _stop) = spawn_target(script);
    let mut mapping = HashMap::new();
    mapping.insert(
        "origin.invalid".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    // The proxy's certificate is signed by a CA the client does not trust.
    let (_untrusted_ca, leaf, key) = tls_pair("127.0.0.1");
    let proxy = spawn_proxy(FixtureKind::Https, Arc::new(mapping), Some((leaf, key)));
    let http = allow_http(vec!["origin.invalid"], vec![target_port]);
    let url = format!("http://origin.invalid:{target_port}/never");
    let error = run_open(
        &http,
        &url,
        Some(route(ProxyKind::Https, &proxy, true)),
        false,
        &TransportHooks::default(),
    )
    .expect_err("outer TLS must fail");
    assert_eq!(error, "proxy_tls_failed");
    assert_no_accepts(&target_count, "target");
    proxy.stop();
}

#[test]
fn inner_tls_failure_keeps_the_tunnel_ciphertext_only() {
    let (trusted_ca, proxy_leaf, proxy_key) = tls_pair("127.0.0.1");
    let (_untrusted_ca, target_leaf, target_key) = tls_pair("127.0.0.1");
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(b"never")])));
    let (target_port, _count, target_log, _stop) =
        spawn_tls_target(script, target_leaf, target_key);
    let mut mapping = HashMap::new();
    mapping.insert(
        "127.0.0.1".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    let proxy = spawn_proxy(
        FixtureKind::Https,
        Arc::new(mapping),
        Some((proxy_leaf, proxy_key)),
    );
    let hooks = TransportHooks {
        extra_trust_der: vec![trusted_ca],
        ..TransportHooks::default()
    };
    let http = allow_http(vec!["127.0.0.1"], vec![target_port]);
    let url = format!("https://127.0.0.1:{target_port}/never");
    let error = run_open(
        &http,
        &url,
        Some(route(ProxyKind::Https, &proxy, true)),
        false,
        &hooks,
    )
    .expect_err("inner TLS must fail");
    assert_eq!(error, "target_tls_failed");
    let log = target_log.lock().expect("tls log");
    let first = log
        .iter()
        .find(|line| line.starts_with("first-bytes "))
        .expect("first bytes");
    assert!(
        !first.contains("474554"),
        "plaintext HTTP reached the target: {first}"
    );
    assert!(!log.iter().any(|line| line.starts_with("request ")));
    drop(log);
    proxy.stop();
}

fn spawn_stall_accept() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("stall listener");
    let port = listener.local_addr().expect("stall address").port();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                std::thread::sleep(Duration::from_secs(1));
            });
        }
    });
    port
}

#[test]
fn outer_tls_handshake_stall_hits_the_connect_deadline() {
    let stall_port = spawn_stall_accept();
    let mut config = allow_http(vec!["127.0.0.1"], vec![stall_port]);
    config.connect_timeout = Duration::from_millis(200);
    let route =
        NativeProxyRoute::new(ProxyKind::Https, "127.0.0.1", stall_port, true).expect("route");
    let url = format!("http://127.0.0.1:{stall_port}/outer-stall");
    let error = run_open(
        &config,
        &url,
        Some(route),
        false,
        &TransportHooks::default(),
    )
    .expect_err("outer TLS stall must hit the deadline");
    assert_eq!(error, "deadline");
}

#[test]
fn inner_tls_handshake_stall_hits_the_connect_deadline() {
    let stall_port = spawn_stall_accept();
    let mut mapping = HashMap::new();
    mapping.insert(
        "127.0.0.1".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), stall_port),
    );
    let proxy = spawn_proxy(FixtureKind::Http, Arc::new(mapping), None);
    let mut config = allow_http(vec!["127.0.0.1"], vec![stall_port]);
    config.connect_timeout = Duration::from_millis(200);
    let url = format!("https://127.0.0.1:{stall_port}/inner-stall");
    let error = run_open(
        &config,
        &url,
        Some(route(ProxyKind::Http, &proxy, true)),
        false,
        &TransportHooks::default(),
    )
    .expect_err("inner TLS stall must hit the deadline");
    assert_eq!(error, "deadline");
    proxy.stop();
}

#[test]
fn worker_streams_through_selected_proxy_and_publishes_bytes() {
    let root = TestRoot::create();
    let script = Arc::new(Mutex::new(VecDeque::from([Resp::ok(
        b"worker-proxy-bytes",
    )])));
    let (target_port, _count, target_log, _stop) = spawn_target(script);
    let mut mapping = HashMap::new();
    mapping.insert(
        "worker.invalid".to_string(),
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), target_port),
    );
    let proxy = spawn_proxy(FixtureKind::Http, Arc::new(mapping), None);
    let mut config = pool_config(&root, target_port, 1, 1, 4, 64);
    config.http = allow_http(vec!["worker.invalid"], vec![target_port]);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    let lease = pool.register(90, 1).expect("register");
    let mut job = base_request(90, 1, 1, target_port, "proxy.bin");
    job.url = format!("http://worker.invalid:{target_port}/proxy.bin");
    pool.submit(lease.submission_with_route(job, Some(route(ProxyKind::Http, &proxy, true))))
        .expect("submit");
    let events = pool
        .wait_terminals(1, Duration::from_secs(10))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("completed"));
    assert_eq!(
        std::fs::read(root.path().join("proxy.bin")).expect("file"),
        b"worker-proxy-bytes"
    );
    assert!(proxy
        .log()
        .iter()
        .any(|line| line.contains("GET http://worker.invalid:")));
    assert!(target_log
        .lock()
        .expect("target")
        .iter()
        .any(|line| line.contains("GET /proxy.bin")));
    let report = pool.shutdown().expect("shutdown");
    assert!(report.joined);
    proxy.stop();
}

#[test]
fn cancel_during_connect_handshake_releases_every_driver() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("stall proxy");
    let port = listener.local_addr().expect("stall addr").port();
    let accepts = Arc::new(Count::new());
    let server_accepts = Arc::clone(&accepts);
    let hold = Arc::new(Count::new());
    let server_hold = Arc::clone(&hold);
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        server_accepts.signal();
        let _ = read_header_block(&mut stream);
        server_hold.signal();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut buffer = [0u8; 64];
        while let Ok(n) = stream.read(&mut buffer) {
            if n == 0 {
                break;
            }
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let error = runtime.block_on(async {
        let metrics = Metrics::default();
        let owner = Arc::new(DriverOwner::new());
        let cancel = CancellationToken::new();
        let cancel_thread = cancel.clone();
        let accepts = Arc::clone(&accepts);
        let hold = Arc::clone(&hold);
        std::thread::spawn(move || {
            let _ = accepts.wait(1, Duration::from_secs(5));
            let _ = hold.wait(1, Duration::from_secs(5));
            cancel_thread.cancel();
        });
        let http = allow_http(vec!["target.invalid"], vec![9]);
        let route = NativeProxyRoute::new(ProxyKind::Http, "127.0.0.1", port, true).expect("route");
        let url = "http://target.invalid:9/cancel.bin";
        let result = open_get_with_options(
            &http,
            url,
            &[],
            &cancel,
            &metrics,
            &TransportHooks::default(),
            &owner,
            Instant::now(),
            OpenOptions::get(Some(&route), false, false),
        )
        .await;
        let code = match result {
            Ok(mut opened) => {
                let _ = opened
                    .connection
                    .shutdown(Instant::now() + Duration::from_secs(2))
                    .await;
                "opened"
            }
            Err(code) => code,
        };
        assert_eq!(owner.active(), 0, "cancelled connector left a driver");
        code
    });
    assert_eq!(error, "cancelled");
}
