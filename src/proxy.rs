//! Proxy pool, protocol connectors and Glype handling from `xeHentai/proxy.py`.
//!
//! The pool keeps the reference insertion order, first-available selection,
//! success/fail counters, `MAX_FAIL` policy and ban expiry. Every proxy kind
//! has a real local protocol path: HTTP(S) proxy (origin/absolute-form or
//! CONNECT), SOCKS4/4a/5/5h handshakes and Glype URL rewriting. TLS
//! verification is never disabled; tests inject a test CA and DNS mapping.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use regex::Regex;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub const MAX_FAIL: i64 = 5;

/// One parsed proxy address.
#[derive(Clone, Debug)]
pub struct ProxySpec {
    pub addr: String,
    pub kind: ProxyKind,
    /// Glype handler state (`g_session`) is shared by every request through
    /// this pooled proxy, like the reference closure.
    pub glype: Option<Arc<Mutex<GlypeState>>>,
}

#[derive(Clone, Debug)]
pub enum ProxyKind {
    Http { tls: bool },
    Socks { version: u8, remote_dns: bool },
    Glype(GlypeSpec),
}

#[derive(Clone)]
pub struct GlypeState {
    pub s: Option<String>,
}

impl fmt::Debug for GlypeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlypeState").field("s", &self.s).finish()
    }
}

#[derive(Clone, Debug)]
pub struct GlypeSpec {
    pub server: String,
    pub inst_loc: String,
    pub script: String,
    pub argname: String,
    pub bval: String,
    pub url_re: Regex,
    pub error_re: Regex,
}

#[derive(Debug)]
pub enum ProxyError {
    Address(String),
    NoProxy,
    Dns(String),
    Connect(String),
    Handshake(String),
    Tls(String),
    Http(String),
    Glype(String),
    GlypeMissingContentType,
    GlypeDecode(String),
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address(value) => write!(f, "{value} is not an acceptable proxy address"),
            Self::NoProxy => write!(f, "try to use proxy but no proxies avaliable"),
            Self::Dns(message) => write!(f, "dns error: {message}"),
            Self::Connect(message) => write!(f, "connection error: {message}"),
            Self::Handshake(message) => write!(f, "proxy handshake error: {message}"),
            Self::Tls(message) => write!(f, "tls error: {message}"),
            Self::Http(message) => write!(f, "proxy http error: {message}"),
            Self::Glype(message) => write!(f, "{message}"),
            Self::GlypeMissingContentType => {
                write!(f, "'NoneType' object has no attribute 'startswith'")
            }
            Self::GlypeDecode(message) => write!(f, "glype body decode error: {message}"),
        }
    }
}

impl std::error::Error for ProxyError {}

impl ProxyError {
    pub fn is_bad_status_line(&self) -> bool {
        matches!(self, Self::Http(message) if message.contains("BadStatusLine"))
    }
}

#[derive(Clone, Debug)]
pub struct ProxyEntry {
    pub spec: ProxySpec,
    pub success: i64,
    pub fail: i64,
}

pub struct Pool {
    pub proxies: IndexMap<String, ProxyEntry>,
    pub disabled: IndexMap<String, f64>,
    disable_policy: Box<dyn Fn(i64, i64) -> bool + Send + Sync>,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("proxies", &self.proxies)
            .field("disabled", &self.disabled)
            .finish()
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

fn wall_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

impl Pool {
    pub fn new() -> Self {
        Self::with_policy_and_clock(
            Box::new(|_success, fails| fails >= MAX_FAIL),
            Box::new(wall_time),
        )
    }

    pub fn with_policy_and_clock(
        disable_policy: Box<dyn Fn(i64, i64) -> bool + Send + Sync>,
        now: Box<dyn Fn() -> f64 + Send + Sync>,
    ) -> Self {
        Self {
            proxies: IndexMap::new(),
            disabled: IndexMap::new(),
            disable_policy,
            now,
        }
    }

    /// Resolve expired bans, then return the first available proxy in
    /// insertion order. The reference never rotates or health-checks.
    pub fn select(&mut self) -> Result<ProxySpec, ProxyError> {
        let now = (self.now)();
        let expired: Vec<String> = self
            .disabled
            .iter()
            .filter(|(_, expire)| **expire > 0.0 && **expire < now)
            .map(|(addr, _)| addr.clone())
            .collect();
        for addr in expired {
            self.disabled.shift_remove(&addr);
        }
        let selected = self
            .proxies
            .keys()
            .find(|addr| !self.disabled.contains_key(*addr))
            .cloned();
        match selected {
            Some(addr) => Ok(self.proxies[&addr].spec.clone()),
            None => Err(ProxyError::NoProxy),
        }
    }

    /// Reference `has_available_proxies` is inverted: `true` means the pool
    /// has no usable proxy.
    pub fn has_available_proxies(&self) -> bool {
        !self
            .proxies
            .keys()
            .any(|addr| !self.disabled.contains_key(addr))
    }

    pub fn add_proxy(&mut self, addr: &str) -> Result<(), ProxyError> {
        let spec = parse_proxy(addr)?;
        let entry = ProxyEntry {
            spec,
            success: 0,
            fail: 0,
        };
        self.proxies.insert(addr.to_string(), entry);
        Ok(())
    }

    pub fn counts(&self, addr: &str) -> Option<(i64, i64)> {
        self.proxies
            .get(addr)
            .map(|entry| (entry.success, entry.fail))
    }

    pub fn is_disabled(&self, addr: &str) -> Option<f64> {
        self.disabled.get(addr).copied()
    }

    /// `not_good(weight=MAX_FAIL, expire=0)` from the reference.
    pub fn not_good(&mut self, addr: &str, weight: i64, expire_seconds: f64) {
        let Some(entry) = self.proxies.get_mut(addr) else {
            return;
        };
        entry.fail += weight;
        let success = entry.success;
        let fails = entry.fail;
        if (self.disable_policy)(success, fails) {
            // `not_good` always stores an absolute expiry, even for 0.
            let expire = expire_seconds + (self.now)();
            self.disabled.insert(addr.to_string(), expire);
        }
    }

    /// `trace_proxy` bookkeeping for one request attempt.
    ///
    /// `checked_ok` is `None` when no check function exists. A transport
    /// exception adds `weight` failures; a successful response adds `weight`
    /// successes unless a check rejected it. BadStatusLine is ignored, as in
    /// the reference.
    pub fn trace(&mut self, addr: &str, excepted: Option<bool>, checked_ok: Option<bool>) {
        let Some(entry) = self.proxies.get_mut(addr) else {
            return;
        };
        if let Some(failed) = excepted {
            if failed {
                entry.fail += 1;
            } else {
                match checked_ok {
                    Some(false) => entry.fail += 1,
                    _ => entry.success += 1,
                }
            }
        }
        let success = entry.success;
        let fails = entry.fail;
        if (self.disable_policy)(success, fails) {
            self.disabled.insert(addr.to_string(), 0.0);
        }
    }
}

/// The reference regex accepts any `[ah]*` suffix; requests itself only
/// understands 4a/5h for remote DNS. We keep the accepted spellings and treat
/// a trailing `a` (v4) / `h` (v5) as the remote-DNS variant.
pub fn parse_proxy(addr: &str) -> Result<ProxySpec, ProxyError> {
    static SOCKS: OnceLock<Regex> = OnceLock::new();
    static HTTP: OnceLock<Regex> = OnceLock::new();
    static GLYPE: OnceLock<Regex> = OnceLock::new();
    let socks = SOCKS.get_or_init(|| {
        Regex::new(r"^socks[45][ah]*://([^:^/]+)(:\d{1,5})*/*$").expect("socks regex")
    });
    let http =
        HTTP.get_or_init(|| Regex::new(r"^https*://([^:^/]+)(:\d{1,5})*/*$").expect("http regex"));
    let glype = GLYPE.get_or_init(|| {
        Regex::new(r"^https*://([^:^/]+)(:\d{1,5})*/(.*?)([^/]+\.php)\?.*b=.+")
            .expect("glype regex")
    });

    if let Some(captures) = socks.captures(addr) {
        let scheme = addr
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .unwrap_or("");
        let version = if scheme.starts_with("socks4") { 4 } else { 5 };
        let suffix = scheme
            .trim_start_matches("socks")
            .trim_start_matches(['4', '5']);
        let remote_dns = if version == 4 {
            suffix.contains('a')
        } else {
            suffix.contains('h')
        };
        let _ = captures;
        return Ok(ProxySpec {
            addr: addr.to_string(),
            kind: ProxyKind::Socks {
                version,
                remote_dns,
            },
            glype: None,
        });
    }
    if glype.is_match(addr) {
        let spec = parse_glype(addr)?;
        return Ok(ProxySpec {
            addr: addr.to_string(),
            kind: ProxyKind::Glype(spec),
            glype: Some(Arc::new(Mutex::new(GlypeState { s: None }))),
        });
    }
    if http.is_match(addr) {
        return Ok(ProxySpec {
            addr: addr.to_string(),
            kind: ProxyKind::Http {
                tls: addr.starts_with("https://"),
            },
            glype: None,
        });
    }
    Err(ProxyError::Address(addr.to_string()))
}

fn parse_glype(addr: &str) -> Result<GlypeSpec, ProxyError> {
    let parts = Regex::new(r"(https*://[^/]+)/(.*?)([^/]+\.php)")
        .expect("glype parts")
        .captures(addr)
        .ok_or_else(|| ProxyError::Address(addr.to_string()))?;
    let server = parts[1].to_string();
    let inst_loc = parts[2].to_string();
    let script = parts[3].to_string();
    let argname = Regex::new(r"[&?]([a-zA-Z._]+)=[^\d]*")
        .expect("glype arg")
        .captures(addr)
        .map(|captures| captures[1].to_string())
        .ok_or_else(|| ProxyError::Address(addr.to_string()))?;
    let bval = Regex::new(r"[&?]b=(\d*)")
        .expect("glype b")
        .captures(addr)
        .map(|captures| captures[1].to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "4".to_string());
    let url_re = Regex::new(&format!("/{}{}\\?u=([^&\"']+)&[^\"']+", inst_loc, script))
        .map_err(|error| ProxyError::Glype(error.to_string()))?;
    let error_re = Regex::new(r#"<div id="error">(.*?)</div>"#).expect("glype error regex");
    Ok(GlypeSpec {
        server,
        inst_loc,
        script,
        argname,
        bval,
        url_re,
        error_re,
    })
}

impl GlypeSpec {
    pub fn mkurl(&self, url: &str) -> String {
        let encoded: String = url::form_urlencoded::byte_serialize(url.as_bytes()).collect();
        format!(
            "{}/{}{}?{}={}&b={}&f=norefer",
            self.server, self.inst_loc, self.script, self.argname, encoded, self.bval
        )
    }

    pub fn script_url(&self) -> String {
        format!("{}/{}{}", self.server, self.inst_loc, self.script)
    }

    pub fn process_php_url(&self) -> String {
        format!(
            "{}/{}includes/process.php?action=sslagree",
            self.server, self.inst_loc
        )
    }

    /// Rewrite session headers the way `glype_proxy.handle` does: every header
    /// is copied, Referer points at the Glype script, and an explicit Cookie
    /// header is re-keyed as `c[site][/][name]` (prefixed by the shared `s`).
    pub fn rewrite_headers(
        &self,
        session_headers: &[(String, String)],
        target_url: &str,
        state: &mut GlypeState,
    ) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = session_headers.to_vec();
        upsert_header(&mut headers, "Referer", self.script_url());
        let Some(cookie_index) = headers
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case("Cookie"))
        else {
            return headers;
        };
        let site = Regex::new(r"https*://([^/]+)/*")
            .expect("site regex")
            .captures(target_url)
            .map(|captures| captures[1].to_string())
            .unwrap_or_default();
        let old = crate::util::parse_cookie(&headers[cookie_index].1);
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Some(session) = &state.s {
            pairs.push(("s".to_string(), session.clone()));
        }
        for (key, value) in old {
            let mapped = format!("c[{}][/][{}]", site, key);
            if let Some(existing) = pairs.iter_mut().find(|(name, _)| *name == mapped) {
                existing.1 = value;
            } else {
                pairs.push((mapped, value));
            }
        }
        headers[cookie_index].1 = crate::util::make_cookie(&pairs);
        headers
    }

    /// Decode `text/*` Glype bodies, reject the error div with the full
    /// `match[0]`, and undo the proxy's URL transformation.
    pub fn process_text(&self, body: &[u8]) -> Result<Vec<u8>, ProxyError> {
        let text = std::str::from_utf8(body)
            .map_err(|error| ProxyError::GlypeDecode(error.to_string()))?;
        if let Some(captures) = self.error_re.captures(text) {
            return Err(ProxyError::Glype(format!(
                "glype returns: {}",
                &captures[0]
            )));
        }
        let rewritten = self.url_re.replace_all(text, |captures: &regex::Captures| {
            percent_decode(&captures[1])
        });
        Ok(rewritten.into_owned().into_bytes())
    }
}

fn percent_decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .into_owned()
}

/// Apply the reference's reverse Set-Cookie mapping: `c[site][/][name]` pairs
/// become `name` and the header is rebuilt with `make_cookie`.
pub fn apply_glype_response_cookies(headers: &mut Vec<(String, String)>, state: &mut GlypeState) {
    let joined = match joined_header(headers, "set-cookie") {
        Some(value) => value,
        None => return,
    };
    let mut pairs = crate::util::parse_cookie(&joined.replace(',', ";"));
    let key_re = Regex::new(r"c\[[^]]+\]\[[^]]+\]\[([^]]+)\]").expect("cookie map regex");
    let original = pairs.clone();
    for (key, value) in original {
        if let Some(captures) = key_re.captures(&key) {
            let short = captures[1].to_string();
            if short == "s" {
                state.s = Some(value.clone());
            }
            if let Some(existing) = pairs.iter_mut().find(|(name, _)| *name == short) {
                existing.1 = value;
            } else {
                pairs.push((short, value));
            }
        }
    }
    let rebuilt = crate::util::make_cookie(&pairs);
    set_single_header(headers, "set-cookie", rebuilt);
}

fn joined_header(headers: &[(String, String)], name: &str) -> Option<String> {
    let mut out: Option<String> = None;
    for (header_name, value) in headers {
        if header_name.eq_ignore_ascii_case(name) {
            match &mut out {
                Some(current) => {
                    current.push_str(", ");
                    current.push_str(value);
                }
                None => out = Some(value.clone()),
            }
        }
    }
    out
}

fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some(entry) = headers
        .iter_mut()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
    {
        entry.1 = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

fn set_single_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    let mut first = true;
    headers.retain_mut(|(header_name, header_value)| {
        if !header_name.eq_ignore_ascii_case(name) {
            return true;
        }
        if first {
            *header_value = value.clone();
            first = false;
            true
        } else {
            false
        }
    });
    if first {
        headers.push((name.to_string(), value));
    }
}

/// `verify_peer` for HTTPS proxy CONNECT: a healthy proxy may answer 403 to
/// the tunnel request; that is a proxy error in requests, not a filter
/// response. Only the CONNECT handshake status is interpreted here.
pub fn ssl_agree_marker() -> &'static str {
    "<input type=\"hidden\" name=\"action\" value=\"sslagree\">"
}

// --- DNS and TLS -----------------------------------------------------------

pub type LookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SocketAddr>, String>> + Send + 'a>>;

pub trait Resolver: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;

    /// Asynchronous variant used by the real system resolver. `None` means
    /// this resolver is an in-memory map whose synchronous `resolve` does no
    /// blocking I/O and is used directly.
    fn lookup<'a>(&'a self, _host: &'a str, _port: u16) -> Option<LookupFuture<'a>> {
        None
    }
}

pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        use std::net::ToSocketAddrs;
        (host, port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect())
            .map_err(|error| error.to_string())
    }

    /// Standard Tokio resolution. The lookup runs on Tokio's blocking pool;
    /// cancelling the future drops the result but cannot interrupt the
    /// operating-system call already in flight. It can never start a late
    /// HTTP exchange because its owner returns the cancellation first.
    fn lookup<'a>(&'a self, host: &'a str, port: u16) -> Option<LookupFuture<'a>> {
        Some(Box::pin(async move {
            tokio::net::lookup_host((host, port))
                .await
                .map(|addresses| addresses.collect())
                .map_err(|error| error.to_string())
        }))
    }
}

pub fn ensure_crypto_provider() {
    static INSTALL: OnceLock<()> = OnceLock::new();
    INSTALL.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Clone)]
pub struct TlsClient {
    config: Arc<rustls::ClientConfig>,
}

impl TlsClient {
    /// Production client: normal public roots, TLS verification on.
    pub fn production() -> Self {
        ensure_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::from_roots(roots)
    }

    pub fn from_roots(roots: rustls::RootCertStore) -> Self {
        ensure_crypto_provider();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Self {
            config: Arc::new(config),
        }
    }

    /// Test-only constructor: trusts exactly the supplied CA bundle.
    pub fn with_test_ca(ca: CertificateDer<'static>) -> Self {
        ensure_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(ca)
            .unwrap_or_else(|error| panic!("invalid test CA: {error}"));
        Self::from_roots(roots)
    }

    pub async fn connect<S>(
        &self,
        host: &str,
        stream: S,
    ) -> Result<tokio_rustls::client::TlsStream<S>, ProxyError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|error| ProxyError::Tls(error.to_string()))?;
        TlsConnector::from(Arc::clone(&self.config))
            .connect(server_name, stream)
            .await
            .map_err(|error| ProxyError::Tls(error.to_string()))
    }
}

// --- connectors ------------------------------------------------------------

pub trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}
pub type BoxStream = Box<dyn AsyncReadWrite + Unpin + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestForm {
    Origin,
    Absolute,
}

fn default_port(scheme: &str) -> u16 {
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

async fn tcp_connect(
    addr: SocketAddr,
    deadline: tokio::time::Instant,
) -> Result<TcpStream, ProxyError> {
    match tokio::time::timeout_at(deadline, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(ProxyError::Connect(error.to_string())),
        Err(_) => Err(ProxyError::Connect("route establishment timed out".into())),
    }
}

async fn resolve_host(
    host: &str,
    port: u16,
    resolver: &dyn Resolver,
) -> Result<Vec<SocketAddr>, ProxyError> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    match resolver.lookup(host, port) {
        Some(future) => future.await.map_err(ProxyError::Dns),
        None => resolver.resolve(host, port).map_err(ProxyError::Dns),
    }
}

fn first_address(addresses: Vec<SocketAddr>) -> Result<SocketAddr, ProxyError> {
    addresses
        .into_iter()
        .next()
        .ok_or_else(|| ProxyError::Dns("no addresses".into()))
}

async fn connect_target(
    host: &str,
    port: u16,
    resolver: &dyn Resolver,
    deadline: tokio::time::Instant,
    tls: &TlsClient,
    target_tls: bool,
) -> Result<BoxStream, ProxyError> {
    let address = first_address(resolve_host(host, port, resolver).await?)?;
    let stream = tcp_connect(address, deadline).await?;
    if target_tls {
        let tls_stream = tls.connect(host, stream).await?;
        Ok(Box::new(tls_stream))
    } else {
        Ok(Box::new(stream))
    }
}

/// Open the socket path for one hop and tell the caller how to write the
/// request line. `absolute_url` is only used for an HTTP proxy and a plain
/// HTTP target, where the reference requests library sends absolute-form.
///
/// Every step of one route attempt (asynchronous DNS, then TCP, SOCKS4/4a/5/5h,
/// CONNECT and TLS) shares a single absolute deadline derived from the
/// configured request timeout, so a peer that drips handshake bytes cannot
/// extend the budget one byte at a time. DNS is awaited through Tokio's
/// standard resolver, so a pending lookup cannot freeze an unrelated worker on
/// the same `LocalSet`; dropping the future at the deadline cannot start a
/// late HTTP exchange. The deadline does not become an `HttpError::Pool`; it
/// is the same retryable network error as any other route failure.
#[allow(clippy::too_many_arguments)]
pub async fn open_route(
    spec: Option<&ProxySpec>,
    absolute_url: &str,
    host: &str,
    port: u16,
    target_tls: bool,
    resolver: &dyn Resolver,
    tls: &TlsClient,
    timeout: Duration,
) -> Result<(BoxStream, RequestForm), ProxyError> {
    let deadline = tokio::time::Instant::now() + timeout;
    match tokio::time::timeout_at(
        deadline,
        open_route_inner(
            spec,
            absolute_url,
            host,
            port,
            target_tls,
            resolver,
            tls,
            deadline,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ProxyError::Connect("route establishment timed out".into())),
    }
}

#[allow(clippy::too_many_arguments)]
async fn open_route_inner(
    spec: Option<&ProxySpec>,
    absolute_url: &str,
    host: &str,
    port: u16,
    target_tls: bool,
    resolver: &dyn Resolver,
    tls: &TlsClient,
    deadline: tokio::time::Instant,
) -> Result<(BoxStream, RequestForm), ProxyError> {
    let Some(spec) = spec else {
        let stream = connect_target(host, port, resolver, deadline, tls, target_tls).await?;
        return Ok((stream, RequestForm::Origin));
    };
    match &spec.kind {
        ProxyKind::Http { tls: proxy_tls } => {
            let proxy_url = url::Url::parse(&spec.addr)
                .map_err(|error| ProxyError::Address(error.to_string()))?;
            let proxy_host = proxy_url
                .host_str()
                .ok_or_else(|| ProxyError::Address(spec.addr.clone()))?;
            let proxy_port = proxy_url.port_or_known_default().unwrap_or(80);
            let address = first_address(resolve_host(proxy_host, proxy_port, resolver).await?)?;
            let stream = tcp_connect(address, deadline).await?;
            let mut stream: BoxStream = if *proxy_tls {
                Box::new(tls.connect(proxy_host, stream).await?)
            } else {
                Box::new(stream)
            };
            if target_tls {
                let authority = format!("{host}:{port}");
                let request = format!(
                    "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nConnection: keep-alive\r\n\r\n"
                );
                stream
                    .write_all(request.as_bytes())
                    .await
                    .map_err(|error| ProxyError::Connect(error.to_string()))?;
                stream
                    .flush()
                    .await
                    .map_err(|error| ProxyError::Connect(error.to_string()))?;
                read_connect_response(&mut stream).await?;
                let tls_stream = tls.connect(host, stream).await?;
                Ok((Box::new(tls_stream) as BoxStream, RequestForm::Origin))
            } else {
                let _ = absolute_url;
                Ok((stream, RequestForm::Absolute))
            }
        }
        ProxyKind::Socks {
            version,
            remote_dns,
        } => {
            let proxy_url = url::Url::parse(&spec.addr)
                .map_err(|error| ProxyError::Address(error.to_string()))?;
            let proxy_host = proxy_url
                .host_str()
                .ok_or_else(|| ProxyError::Address(spec.addr.clone()))?;
            let proxy_port = proxy_url.port_or_known_default().unwrap_or(1080);
            let address = first_address(resolve_host(proxy_host, proxy_port, resolver).await?)?;
            let mut stream = tcp_connect(address, deadline).await?;
            if *version == 4 {
                socks4_handshake(&mut stream, host, port, *remote_dns, resolver).await?;
            } else {
                socks5_handshake(&mut stream, host, port, *remote_dns, resolver).await?;
            }
            if target_tls {
                let tls_stream = tls.connect(host, stream).await?;
                Ok((Box::new(tls_stream), RequestForm::Origin))
            } else {
                Ok((Box::new(stream), RequestForm::Origin))
            }
        }
        ProxyKind::Glype(_) => Err(ProxyError::Glype(
            "glype uses the rewritten-request path".into(),
        )),
    }
}

async fn read_connect_response(stream: &mut BoxStream) -> Result<(), ProxyError> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| ProxyError::Connect(error.to_string()))?;
        if read == 0 {
            return Err(ProxyError::Http(
                "BadStatusLine: empty CONNECT response".into(),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 16 * 1024 {
            return Err(ProxyError::Http(
                "CONNECT response headers too large".into(),
            ));
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| ProxyError::Http("BadStatusLine: malformed CONNECT response".into()))?;
    if status != 200 {
        return Err(ProxyError::Http(format!(
            "CONNECT tunnel failed with status {status}"
        )));
    }
    Ok(())
}

async fn socks4_handshake(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    remote_dns: bool,
    resolver: &dyn Resolver,
) -> Result<(), ProxyError> {
    let mut request = vec![4u8, 1, (port >> 8) as u8, port as u8];
    if remote_dns {
        request.extend_from_slice(&[0, 0, 0, 1]);
        request.push(0);
        request.extend_from_slice(host.as_bytes());
        request.push(0);
    } else {
        let address = first_address(resolve_host(host, port, resolver).await?)?;
        match address.ip() {
            std::net::IpAddr::V4(ip) => request.extend_from_slice(&ip.octets()),
            std::net::IpAddr::V6(_) => {
                return Err(ProxyError::Dns(
                    "socks4 requires a local IPv4 address".into(),
                ));
            }
        }
        request.push(0);
    }
    stream
        .write_all(&request)
        .await
        .map_err(|error| ProxyError::Connect(error.to_string()))?;
    let mut reply = [0u8; 8];
    stream
        .read_exact(&mut reply)
        .await
        .map_err(|error| ProxyError::Handshake(error.to_string()))?;
    if reply[1] != 0x5a {
        return Err(ProxyError::Handshake(format!(
            "socks4 rejected request: {}",
            reply[1]
        )));
    }
    Ok(())
}

async fn socks5_handshake(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    remote_dns: bool,
    resolver: &dyn Resolver,
) -> Result<(), ProxyError> {
    stream
        .write_all(&[5, 1, 0])
        .await
        .map_err(|error| ProxyError::Connect(error.to_string()))?;
    let mut method = [0u8; 2];
    stream
        .read_exact(&mut method)
        .await
        .map_err(|error| ProxyError::Handshake(error.to_string()))?;
    if method != [5, 0] {
        return Err(ProxyError::Handshake(format!(
            "socks5 selected method {}",
            method[1]
        )));
    }
    let mut request = vec![5u8, 1, 0];
    if remote_dns {
        if host.len() > 255 {
            return Err(ProxyError::Handshake("socks5 hostname too long".into()));
        }
        request.push(3);
        request.push(host.len() as u8);
        request.extend_from_slice(host.as_bytes());
    } else {
        let address = first_address(resolve_host(host, port, resolver).await?)?;
        match address.ip() {
            std::net::IpAddr::V4(ip) => {
                request.push(1);
                request.extend_from_slice(&ip.octets());
            }
            std::net::IpAddr::V6(ip) => {
                request.push(4);
                request.extend_from_slice(&ip.octets());
            }
        }
    }
    request.push((port >> 8) as u8);
    request.push(port as u8);
    stream
        .write_all(&request)
        .await
        .map_err(|error| ProxyError::Connect(error.to_string()))?;
    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .map_err(|error| ProxyError::Handshake(error.to_string()))?;
    if head[0] != 5 || head[1] != 0 {
        return Err(ProxyError::Handshake(format!(
            "socks5 rejected request: {}/{}",
            head[0], head[1]
        )));
    }
    let address_len = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream
                .read_exact(&mut len)
                .await
                .map_err(|error| ProxyError::Handshake(error.to_string()))?;
            len[0] as usize
        }
        other => {
            return Err(ProxyError::Handshake(format!(
                "socks5 unknown address type {other}"
            )));
        }
    };
    let mut rest = vec![0u8; address_len + 2];
    stream
        .read_exact(&mut rest)
        .await
        .map_err(|error| ProxyError::Handshake(error.to_string()))?;
    Ok(())
}

/// A deterministic DNS record used by tests and the compatibility probe.
#[derive(Clone, Default)]
pub struct MapResolver {
    pub map: BTreeMap<String, Vec<SocketAddr>>,
    pub calls: Arc<Mutex<Vec<String>>>,
}
impl MapResolver {
    pub fn new(map: Vec<(String, Vec<SocketAddr>)>) -> Self {
        Self {
            map: map.into_iter().collect(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Resolver for MapResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        self.calls.lock().unwrap().push(host.to_string());
        self.map
            .get(host)
            .map(|addresses| {
                addresses
                    .iter()
                    .map(|address| SocketAddr::new(address.ip(), port))
                    .collect()
            })
            .ok_or_else(|| format!("no synthetic record for {host}"))
    }
}

/// Test/probe resolver that returns one exact socket address, port included.
/// The reference tests need `e-hentai.org` routed to a local ephemeral port;
/// `MapResolver` deliberately keeps DNS port substitution, this helper does
/// not. Production sessions use [`SystemResolver`].
#[derive(Clone, Default)]
pub struct FixedResolver {
    pub map: BTreeMap<String, SocketAddr>,
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl FixedResolver {
    pub fn new(address: SocketAddr) -> Self {
        let mut map = BTreeMap::new();
        map.insert("e-hentai.org".to_string(), address);
        map.insert("exhentai.org".to_string(), address);
        map.insert("forums.e-hentai.org".to_string(), address);
        Self {
            map,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Resolver for FixedResolver {
    fn resolve(&self, host: &str, _port: u16) -> Result<Vec<SocketAddr>, String> {
        self.calls.lock().unwrap().push(host.to_string());
        self.map
            .get(host)
            .copied()
            .map(|address| vec![address])
            .ok_or_else(|| format!("no fixed record for {host}"))
    }
}

/// Parse a response header value as an integer the way `int()` does for the
/// reference's `content-length` handling.
pub fn parse_content_length(values: &[(String, String)]) -> Result<Option<usize>, String> {
    let Some(value) = values
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim())
    else {
        return Ok(None);
    };
    value
        .parse::<usize>()
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Format a socket address the way `requests` does for absolute-form requests
/// on a proxy (scheme default port omitted).
pub fn absolute_url(scheme: &str, host: &str, port: u16, path: &str) -> String {
    let default = default_port(scheme);
    let authority = if port == default {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    format!("{scheme}://{authority}{path}")
}

pub fn io_error(error: io::Error) -> ProxyError {
    ProxyError::Connect(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> Pool {
        Pool::new()
    }

    #[test]
    fn pool_selects_first_inserted_available_proxy() {
        let mut pool = pool();
        pool.add_proxy("http://127.0.0.1:8081").unwrap();
        pool.add_proxy("http://127.0.0.1:8082").unwrap();
        assert_eq!(pool.select().unwrap().addr, "http://127.0.0.1:8081");
        pool.not_good("http://127.0.0.1:8081", MAX_FAIL, 10.0);
        assert!(pool.is_disabled("http://127.0.0.1:8081").is_some());
        assert_eq!(pool.select().unwrap().addr, "http://127.0.0.1:8082");
    }

    #[test]
    fn max_fail_disables_permanently_and_reports_inverted_availability() {
        let mut pool = pool();
        pool.add_proxy("http://127.0.0.1:8081").unwrap();
        assert!(!pool.has_available_proxies());
        pool.trace("http://127.0.0.1:8081", Some(true), None);
        pool.trace("http://127.0.0.1:8081", Some(true), None);
        pool.trace("http://127.0.0.1:8081", Some(false), None);
        pool.trace("http://127.0.0.1:8081", Some(true), None);
        pool.trace("http://127.0.0.1:8081", Some(true), None);
        assert_eq!(pool.counts("http://127.0.0.1:8081"), Some((1, 4)));
        pool.trace("http://127.0.0.1:8081", Some(true), None);
        assert_eq!(pool.counts("http://127.0.0.1:8081"), Some((1, 5)));
        assert_eq!(pool.is_disabled("http://127.0.0.1:8081"), Some(0.0));
        assert!(pool.has_available_proxies());
        assert!(matches!(pool.select(), Err(ProxyError::NoProxy)));
    }

    #[test]
    fn expired_ban_returns_the_proxy_to_the_first_available_slot() {
        let clock = Arc::new(Mutex::new(100.0f64));
        let now = Arc::clone(&clock);
        let mut pool = Pool::with_policy_and_clock(
            Box::new(|_success, fails| fails >= MAX_FAIL),
            Box::new(move || *now.lock().unwrap()),
        );
        pool.add_proxy("socks5://127.0.0.1:1080").unwrap();
        pool.not_good("socks5://127.0.0.1:1080", MAX_FAIL, 10.0);
        assert!(pool.is_disabled("socks5://127.0.0.1:1080").is_some());
        *clock.lock().unwrap() = 111.0;
        assert_eq!(pool.select().unwrap().addr, "socks5://127.0.0.1:1080");
        assert!(pool.is_disabled("socks5://127.0.0.1:1080").is_none());
    }

    #[test]
    fn add_replaces_counts_but_keeps_insertion_position() {
        let mut pool = pool();
        pool.add_proxy("http://127.0.0.1:8081").unwrap();
        pool.add_proxy("http://127.0.0.1:8082").unwrap();
        pool.trace("http://127.0.0.1:8081", Some(true), None);
        pool.add_proxy("http://127.0.0.1:8081").unwrap();
        assert_eq!(pool.counts("http://127.0.0.1:8081"), Some((0, 0)));
        assert_eq!(pool.select().unwrap().addr, "http://127.0.0.1:8081");
    }

    #[test]
    fn addresses_are_classified_in_the_reference_order() {
        assert!(matches!(
            parse_proxy("socks4://127.0.0.1:1080").unwrap().kind,
            ProxyKind::Socks {
                version: 4,
                remote_dns: false
            }
        ));
        assert!(matches!(
            parse_proxy("socks4a://127.0.0.1:1080").unwrap().kind,
            ProxyKind::Socks {
                version: 4,
                remote_dns: true
            }
        ));
        assert!(matches!(
            parse_proxy("socks5://127.0.0.1:1080").unwrap().kind,
            ProxyKind::Socks {
                version: 5,
                remote_dns: false
            }
        ));
        assert!(matches!(
            parse_proxy("socks5h://127.0.0.1:1080").unwrap().kind,
            ProxyKind::Socks {
                version: 5,
                remote_dns: true
            }
        ));
        assert!(matches!(
            parse_proxy("http://127.0.0.1:8080").unwrap().kind,
            ProxyKind::Http { tls: false }
        ));
        assert!(matches!(
            parse_proxy("https://127.0.0.1:8443").unwrap().kind,
            ProxyKind::Http { tls: true }
        ));
        let glype = parse_proxy("http://proxy.test/glype/browse.php?u=x&b=2").unwrap();
        match glype.kind {
            ProxyKind::Glype(spec) => {
                assert_eq!(spec.argname, "u");
                assert_eq!(spec.bval, "2");
                assert_eq!(spec.inst_loc, "glype/");
                assert_eq!(spec.script, "browse.php");
            }
            other => panic!("expected glype, got {other:?}"),
        }
        assert!(parse_proxy("ftp://127.0.0.1").is_err());
        assert!(parse_proxy("http://host/path").is_err());
    }

    #[test]
    fn glype_mkurl_and_text_rewrite_follow_the_reference() {
        let spec = match parse_proxy("http://proxy.test/glype/browse.php?u=x&b=2")
            .unwrap()
            .kind
        {
            ProxyKind::Glype(spec) => spec,
            _ => unreachable!(),
        };
        assert_eq!(
            spec.mkurl("https://e-hentai.org/s/a b/1-2"),
            "http://proxy.test/glype/browse.php?u=https%3A%2F%2Fe-hentai.org%2Fs%2Fa+b%2F1-2&b=2&f=norefer"
        );
        let body = b"<a href=\"/glype/browse.php?u=https%3A%2F%2Fe-hentai.org%2Fg%2F1&b=2&f=norefer\">x</a>";
        let rewritten = spec.process_text(body).unwrap();
        assert_eq!(
            String::from_utf8(rewritten).unwrap(),
            "<a href=\"https://e-hentai.org/g/1\">x</a>"
        );
        let err = spec
            .process_text(b"<div id=\"error\">bad proxy &amp; thing</div>")
            .expect_err("error div");
        assert_eq!(
            err.to_string(),
            "glype returns: <div id=\"error\">bad proxy &amp; thing</div>"
        );
        assert!(matches!(
            spec.process_text(b"\xff"),
            Err(ProxyError::GlypeDecode(_))
        ));
    }

    #[test]
    fn glype_cookie_header_is_rekeyed_for_the_target_site() {
        let spec = match parse_proxy("http://proxy.test/browse.php?u=x&b=4")
            .unwrap()
            .kind
        {
            ProxyKind::Glype(spec) => spec,
            _ => unreachable!(),
        };
        let mut state = GlypeState {
            s: Some("session".into()),
        };
        let headers = spec.rewrite_headers(
            &[
                ("Cookie".into(), "a=1; b=2".into()),
                ("Referer".into(), "old".into()),
            ],
            "https://e-hentai.org/g/1/",
            &mut state,
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "Referer")
                .map(|(_, value)| value.as_str()),
            Some("http://proxy.test/browse.php")
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "Cookie")
                .map(|(_, value)| value.as_str()),
            Some("s=session;c[e-hentai.org][/][a]=1;c[e-hentai.org][/][b]=2")
        );
    }

    #[test]
    fn glype_reverse_set_cookie_mapping_extracts_short_names() {
        let spec = match parse_proxy("http://proxy.test/browse.php?u=x&b=4")
            .unwrap()
            .kind
        {
            ProxyKind::Glype(spec) => spec,
            _ => unreachable!(),
        };
        let _ = spec;
        let mut state = GlypeState { s: None };
        let mut headers = vec![(
            "Set-Cookie".into(),
            "c[proxy][/][s]=abc; c[site][/][foo]=bar".into(),
        )];
        apply_glype_response_cookies(&mut headers, &mut state);
        assert_eq!(state.s.as_deref(), Some("abc"));
        assert_eq!(
            headers,
            vec![(
                "Set-Cookie".to_string(),
                "c[proxy][/][s]=abc;c[site][/][foo]=bar;s=abc;foo=bar".to_string()
            )]
        );
    }
}
