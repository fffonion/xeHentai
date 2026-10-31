//! Persistent async worker plus the native HTTP transport used by it.
//!
//! The reference `HttpReq` lives here: one session, explicit headers/cookies,
//! `retry=10` by default, `random()+0.618` backoff, manual no-follow redirects
//! with a 30-hop ceiling, the `_FakeResponse` 600 boundary and the proxy ban
//! re-route. HTTP/1.1 framing is done by the mature `hyper` client over
//! `tokio` sockets; TLS is `rustls` with verification always on. Responses stay
//! streaming; only bounded control pages are buffered.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Uri};
use http_body_util::Full;
use hyper::body::Body as _;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::r#const as consts;
use crate::proxy::{
    self, BoxStream, GlypeSpec, GlypeState, MapResolver, Pool, ProxyError, ProxyKind, ProxySpec,
    RequestForm, Resolver, SystemResolver, TlsClient,
};
use crate::util::{ProxyPolicy, parse_human_time};

pub const CONTROL_BODY_LIMIT: usize = 8 * 1024 * 1024;
pub const HEADER_LIMIT: usize = 128 * 1024;

// --- persistent worker -----------------------------------------------------

pub struct Worker {
    cancel: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

/// Spawn one persistent worker for `queue`.
pub fn spawn<T, F, Fut>(mut queue: mpsc::Receiver<T>, mut handler: F) -> Worker
where
    T: Send + 'static,
    F: FnMut(T) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (cancel, mut cancelled) = watch::channel(false);
    let handle = tokio::spawn(async move {
        loop {
            let item = tokio::select! {
                biased;
                _ = cancelled.changed() => break,
                item = queue.recv() => item,
            };
            let Some(item) = item else {
                break;
            };
            tokio::select! {
                biased;
                _ = cancelled.changed() => break,
                _ = handler(item) => {}
            }
        }
    });
    Worker { cancel, handle }
}

/// Spawn one persistent worker whose handler is not `Send` because it owns a
/// VM. It must run inside the caller's existing `LocalSet`.
pub fn spawn_local<T, F, Fut>(mut queue: mpsc::Receiver<T>, mut handler: F) -> Worker
where
    T: 'static,
    F: FnMut(T) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let (cancel, mut cancelled) = watch::channel(false);
    let handle = tokio::task::spawn_local(async move {
        loop {
            let item = tokio::select! {
                biased;
                _ = cancelled.changed() => break,
                item = queue.recv() => item,
            };
            let Some(item) = item else {
                break;
            };
            tokio::select! {
                biased;
                _ = cancelled.changed() => break,
                _ = handler(item) => {}
            }
        }
    });
    Worker { cancel, handle }
}

impl Worker {
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }

    /// Cancel any in-flight handler and wait for the task to finish.
    pub async fn shutdown(self) {
        self.cancel();
        let _ = self.handle.await;
    }

    /// Wait for the task to finish after the queue closes.
    pub async fn wait(self) {
        let _ = self.handle.await;
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

// --- clock -----------------------------------------------------------------

pub type SleepFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub trait HttpClock: Send + Sync {
    fn random(&self) -> f64;
    fn sleep<'a>(&'a self, seconds: f64) -> SleepFuture<'a>;
    fn now(&self) -> f64;
}

pub struct RealClock;

impl HttpClock for RealClock {
    fn random(&self) -> f64 {
        fastrand::f64()
    }

    fn sleep<'a>(&'a self, seconds: f64) -> SleepFuture<'a> {
        Box::pin(tokio::time::sleep(Duration::from_secs_f64(
            seconds.max(0.0),
        )))
    }

    fn now(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0)
    }
}

#[derive(Default)]
pub struct TestClock {
    pub random: f64,
    pub sleeps: Mutex<Vec<f64>>,
    pub now: f64,
}

impl TestClock {
    pub fn new(random: f64) -> Self {
        Self {
            random,
            sleeps: Mutex::new(Vec::new()),
            now: 1000.0,
        }
    }

    pub fn sleeps(&self) -> Vec<f64> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl HttpClock for TestClock {
    fn random(&self) -> f64 {
        self.random
    }

    fn sleep<'a>(&'a self, seconds: f64) -> SleepFuture<'a> {
        self.sleeps.lock().unwrap().push(seconds);
        Box::pin(std::future::ready(()))
    }

    fn now(&self) -> f64 {
        self.now
    }
}

/// Deterministic clock whose `now()` advances by a fixed step on every call.
/// It is a test/probe dependency, never production; the speed checker and the
/// monitor receive their clock exactly where the reference reads `time.time`.
pub struct StepClock {
    now: Mutex<f64>,
    step: Mutex<f64>,
    sleeps: Mutex<Vec<f64>>,
    random: f64,
}

impl StepClock {
    pub fn new(now: f64, step: f64) -> Self {
        Self {
            now: Mutex::new(now),
            step: Mutex::new(step),
            sleeps: Mutex::new(Vec::new()),
            random: 0.5,
        }
    }

    pub fn set_step(&self, step: f64) {
        *self.step.lock().unwrap() = step;
    }

    pub fn set_now(&self, now: f64) {
        *self.now.lock().unwrap() = now;
    }

    pub fn sleeps(&self) -> Vec<f64> {
        self.sleeps.lock().unwrap().clone()
    }
}

impl HttpClock for StepClock {
    fn random(&self) -> f64 {
        self.random
    }

    fn sleep<'a>(&'a self, seconds: f64) -> SleepFuture<'a> {
        self.sleeps.lock().unwrap().push(seconds);
        Box::pin(std::future::ready(()))
    }

    fn now(&self) -> f64 {
        let mut now = self.now.lock().unwrap();
        let value = *now;
        *now += *self.step.lock().unwrap();
        value
    }
}

/// Clock for the monitor loop: `sleep(seconds)` advances the same clock the
/// loop reads and yields to the LocalSet instead of really waiting. Ten
/// monitor ticks therefore cost 5.0 reference seconds without wall-clock
/// sleeps, while the production `RealClock` keeps the real cadence.
pub struct TickClock {
    now: Mutex<f64>,
    random: f64,
}

impl TickClock {
    pub fn new(now: f64) -> Self {
        Self {
            now: Mutex::new(now),
            random: 0.5,
        }
    }

    pub fn set_now(&self, value: f64) {
        *self.now.lock().unwrap() = value;
    }

    pub fn advance(&self, seconds: f64) {
        *self.now.lock().unwrap() += seconds;
    }
}

impl HttpClock for TickClock {
    fn random(&self) -> f64 {
        self.random
    }

    fn sleep<'a>(&'a self, seconds: f64) -> SleepFuture<'a> {
        *self.now.lock().unwrap() += seconds.max(0.0);
        Box::pin(tokio::task::yield_now())
    }

    fn now(&self) -> f64 {
        *self.now.lock().unwrap()
    }
}

// --- speed checker ---------------------------------------------------------

/// `speed_checker` from `worker.py:141-169`. A sample is only appended when
/// strictly more than one second of reference time passed since the previous
/// sample; `calc(full=True)` is zero until the five-sample ring is full.
pub struct SpeedChecker {
    pub cnt: usize,
    buffer: std::collections::VecDeque<f64>,
    last_tm: f64,
    last_bytes: i64,
    current_bytes: i64,
    current_tm: f64,
    clock: Arc<dyn HttpClock>,
}

impl SpeedChecker {
    pub fn new(cnt: usize, clock: Arc<dyn HttpClock>) -> Self {
        let mut checker = Self {
            cnt,
            buffer: std::collections::VecDeque::new(),
            last_tm: 0.0,
            last_bytes: 0,
            current_bytes: 0,
            current_tm: 0.0,
            clock,
        };
        checker.reset();
        checker
    }

    pub fn check(&mut self, len: usize) {
        self.current_bytes += len as i64;
        self.current_tm = self.clock.now();
        let elapsed = self.current_tm - self.last_tm;
        if elapsed > 1.0 {
            self.buffer
                .push_back((self.current_bytes - self.last_bytes) as f64 / elapsed);
            while self.buffer.len() > self.cnt {
                self.buffer.pop_front();
            }
            self.last_tm = self.current_tm;
            self.last_bytes = self.current_bytes;
        }
    }

    pub fn calc(&self, full: bool) -> f64 {
        if self.buffer.is_empty() {
            return 0.0;
        }
        if full && self.buffer.len() < self.cnt {
            return 0.0;
        }
        self.buffer.iter().sum::<f64>() / self.buffer.len() as f64
    }

    pub fn reset(&mut self) {
        self.last_tm = self.clock.now();
        self.last_bytes = 0;
        self.current_bytes = 0;
        self.current_tm = 0.0;
        if !self.buffer.is_empty() {
            self.buffer.clear();
        }
    }

    pub fn sample_count(&self) -> usize {
        self.buffer.len()
    }
}

/// `0 < calc(full=True) < threshold` exactly. A zero or not-yet-full buffer
/// must not stop an otherwise healthy download.
pub fn is_low_speed(full: f64, threshold_bytes: i64) -> bool {
    threshold_bytes > 0 && full > 0.0 && full < threshold_bytes as f64
}

// --- fallback IP map -------------------------------------------------------

#[derive(Clone)]
pub struct FallbackMap {
    pub map: BTreeMap<String, Vec<String>>,
    pub choose: Arc<dyn Fn(usize) -> usize + Send + Sync>,
}

impl Default for FallbackMap {
    fn default() -> Self {
        let mut map = BTreeMap::new();
        for (host, addresses) in consts::FALLBACK_IP_MAP {
            map.insert(
                host.to_string(),
                addresses
                    .iter()
                    .map(|address| address.to_string())
                    .collect(),
            );
        }
        Self {
            map,
            choose: Arc::new(|len| fastrand::usize(0..len.max(1))),
        }
    }
}

impl FallbackMap {
    pub fn empty() -> Self {
        Self {
            map: BTreeMap::new(),
            choose: Arc::new(|_| 0),
        }
    }

    pub fn with_host(mut self, host: &str, addresses: &[&str]) -> Self {
        self.map.insert(
            host.to_string(),
            addresses
                .iter()
                .map(|address| address.to_string())
                .collect(),
        );
        self
    }

    pub fn choose(&self, host: &str) -> Option<String> {
        let addresses = self.map.get(host)?;
        if addresses.is_empty() {
            return None;
        }
        Some(addresses[(self.choose)(addresses.len()) % addresses.len()].clone())
    }
}

// --- cookies ---------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct JarCookie {
    domain: String,
    host_only: bool,
    path: String,
    name: String,
    value: String,
    secure: bool,
    expires: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct CookieJar {
    entries: Vec<JarCookie>,
}

impl CookieJar {
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|cookie| (cookie.name.as_str(), cookie.value.as_str()))
    }

    fn remove_matching(&mut self, domain: &str, path: &str, name: &str) {
        self.entries.retain(|cookie| {
            !(cookie.name == name && cookie.domain == domain && cookie.path == path)
        });
    }

    pub fn store(&mut self, url: &url::Url, header: &[u8], now: f64) {
        let text = latin1(header);
        let mut parts = text.split(';');
        let Some(pair) = parts.next() else {
            return;
        };
        let Some((name, value)) = pair.split_once('=') else {
            return;
        };
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        if name.is_empty() {
            return;
        }
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let mut domain = host.clone();
        let mut host_only = true;
        let mut path = default_cookie_path(url.path());
        let mut secure = false;
        let mut expires: Option<f64> = None;
        for attribute in parts {
            let attribute = attribute.trim();
            let Some((key, value)) = attribute.split_once('=') else {
                if attribute.eq_ignore_ascii_case("secure") {
                    secure = true;
                }
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if key.eq_ignore_ascii_case("domain") {
                let domain_value = value.trim_start_matches('.').to_ascii_lowercase();
                if !domain_value.is_empty() {
                    domain = domain_value;
                    host_only = false;
                }
            } else if key.eq_ignore_ascii_case("path") {
                if value.starts_with('/') {
                    path = value.to_string();
                }
            } else if key.eq_ignore_ascii_case("secure") {
                secure = true;
            } else if key.eq_ignore_ascii_case("max-age")
                && let Ok(seconds) = value.parse::<i64>()
            {
                expires = Some(now + seconds as f64);
            }
        }
        self.remove_matching(&domain, &path, &name);
        if expires.is_some_and(|expire| expire <= now) {
            return;
        }
        self.entries.push(JarCookie {
            domain,
            host_only,
            path,
            name,
            value,
            secure,
            expires,
        });
    }

    pub fn header_for(&self, url: &url::Url, now: f64) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        let secure = url.scheme() == "https";
        let mut pairs: Vec<String> = Vec::new();
        for cookie in &self.entries {
            if cookie.expires.is_some_and(|expire| expire <= now) {
                continue;
            }
            if cookie.secure && !secure {
                continue;
            }
            let domain_match = if cookie.host_only {
                host == cookie.domain
            } else {
                host == cookie.domain || host.ends_with(&format!(".{}", cookie.domain))
            };
            if !domain_match || !path_matches(path, &cookie.path) {
                continue;
            }
            pairs.push(format!("{}={}", cookie.name, cookie.value));
        }
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }
}

fn default_cookie_path(path: &str) -> String {
    if !path.starts_with('/') || path == "/" {
        return "/".to_string();
    }
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => path[..index].to_string(),
    }
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')
}

// --- headers ---------------------------------------------------------------

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

fn latin1_bytes(value: &str) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::with_capacity(value.len());
    for character in value.chars() {
        let code = character as u32;
        if code > 0xFF {
            return Err(HttpError::Header(format!(
                "header value is not ISO-8859-1 encodable: {character:?}"
            )));
        }
        out.push(code as u8);
    }
    Ok(out)
}

fn validate_header_text(value: &str) -> Result<(), HttpError> {
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        return Err(HttpError::Header("header contains a control byte".into()));
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct ResponseHeaders {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

impl ResponseHeaders {
    pub fn from_raw(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        Self { entries }
    }

    pub fn raw(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.entries
    }

    /// The `requests` CaseInsensitiveDict view: names/values decoded
    /// ISO-8859-1, duplicates kept in wire order.
    pub fn business_view(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(name, value)| (latin1(name), latin1(value)))
            .collect()
    }

    pub fn joined(&self, name: &str) -> Option<String> {
        let mut out: Option<String> = None;
        for (header_name, value) in &self.entries {
            if latin1(header_name).eq_ignore_ascii_case(name) {
                match &mut out {
                    Some(current) => {
                        current.push_str(", ");
                        current.push_str(&latin1(value));
                    }
                    None => out = Some(latin1(value)),
                }
            }
        }
        out
    }

    pub fn get_all(&self, name: &str) -> Vec<Vec<u8>> {
        self.entries
            .iter()
            .filter(|(header_name, _)| latin1(header_name).eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .collect()
    }
}

// --- request/response ------------------------------------------------------

#[derive(Debug)]
pub enum HttpError {
    Url(String),
    Connect(String),
    Dns(String),
    Tls(String),
    Protocol(String),
    Header(String),
    BodyTooLarge { limit: usize },
    InvalidContentLength(String),
    Pool(ProxyError),
    Glype(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(message) => write!(f, "invalid URL: {message}"),
            Self::Connect(message) => write!(f, "connection error: {message}"),
            Self::Dns(message) => write!(f, "dns error: {message}"),
            Self::Tls(message) => write!(f, "tls error: {message}"),
            Self::Protocol(message) => write!(f, "http protocol error: {message}"),
            Self::Header(message) => write!(f, "header error: {message}"),
            Self::BodyTooLarge { limit } => {
                write!(f, "control response exceeds the {limit} byte budget")
            }
            Self::InvalidContentLength(value) => {
                write!(f, "invalid literal for int() with base 10: '{value}'")
            }
            Self::Pool(error) => write!(f, "{error}"),
            Self::Glype(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<ProxyError> for HttpError {
    fn from(error: ProxyError) -> Self {
        match error {
            ProxyError::NoProxy => Self::Pool(ProxyError::NoProxy),
            other => Self::Connect(other.to_string()),
        }
    }
}

impl HttpError {
    /// `requests.RequestException` classes retry; pool exhaustion and parser
    /// errors such as an invalid `int(content-length)` propagate directly.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Url(_) | Self::Connect(_) | Self::Dns(_) | Self::Tls(_) | Self::Protocol(_)
        )
    }

    pub fn is_bad_status_line(&self) -> bool {
        matches!(self, Self::Connect(message) if message.contains("BadStatusLine"))
    }
}

#[derive(Clone, Debug)]
pub struct AttemptRecord {
    pub url: String,
    pub method: String,
    pub status: Option<i64>,
    pub proxy: Option<String>,
    pub content_length: Option<i64>,
    pub error: Option<String>,
}

/// The hyper HTTP/1 connection driver is owned by the exchange or by the
/// `StreamBody`; it is never spawned, so dropping the request/response drops
/// the IO and closes the peer synchronously.
type ConnectionFuture = Pin<Box<dyn Future<Output = Result<(), hyper::Error>> + Send>>;

pub struct StreamBody {
    incoming: Option<hyper::body::Incoming>,
    timeout: Duration,
    read: i64,
    finished: bool,
    connection: Option<ConnectionFuture>,
    /// The hyper request sender stays alive while the response is being read;
    /// dropping it early makes hyper cancel the pending body.
    sender: Option<http1::SendRequest<Full<Bytes>>>,
}

/// Poll the body frame and the connection driver together. Hyper only makes
/// progress on the socket while its `Connection` future is polled; the request
/// and body futures therefore borrow the same owner.
async fn next_body_data(
    incoming: &mut hyper::body::Incoming,
    connection: &mut Option<ConnectionFuture>,
) -> Result<Option<Bytes>, String> {
    loop {
        let mut connection_error: Option<String> = None;
        let next = std::future::poll_fn(|cx| {
            if let Some(driver) = connection.as_mut()
                && let Poll::Ready(result) = driver.as_mut().poll(cx)
            {
                *connection = None;
                if let Err(error) = result {
                    connection_error = Some(error.to_string());
                }
            }
            Pin::new(&mut *incoming).poll_frame(cx)
        })
        .await;
        match next {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => {
                    if data.is_empty() {
                        continue;
                    }
                    return Ok(Some(data));
                }
                Err(_) => continue,
            },
            Some(Err(error)) => return Err(error.to_string()),
            None => {
                return match connection_error {
                    Some(error) => Err(error),
                    None => Ok(None),
                };
            }
        }
    }
}

impl StreamBody {
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.finished {
            return Ok(None);
        }
        let Some(incoming) = self.incoming.as_mut() else {
            self.release();
            return Ok(None);
        };
        let frame = match tokio::time::timeout(
            self.timeout,
            next_body_data(incoming, &mut self.connection),
        )
        .await
        {
            Ok(Ok(Some(data))) => data,
            Ok(Ok(None)) => {
                self.release();
                return Ok(None);
            }
            Ok(Err(error)) => {
                self.release();
                return Err(error);
            }
            Err(_) => {
                self.release();
                return Err("read timed out".into());
            }
        };
        self.read += frame.len() as i64;
        Ok(Some(frame.to_vec()))
    }

    /// Drop the response body, the owned connection driver and the request
    /// sender in one place. Any of these would keep the socket open.
    fn release(&mut self) {
        self.finished = true;
        self.incoming.take();
        self.connection.take();
        self.sender.take();
    }

    pub fn bytes_read(&self) -> i64 {
        self.read
    }

    /// Close without reading the rest of the body. The owned connection
    /// future (and therefore the socket) is dropped here; no driver task is
    /// left behind.
    pub async fn close(mut self) {
        self.release();
    }
}

pub struct FetchedResponse {
    pub status: i64,
    pub url: String,
    pub real_url: String,
    pub headers: ResponseHeaders,
    pub content_length: i64,
    pub body: Vec<u8>,
    pub text: String,
    pub stream: Option<StreamBody>,
    pub proxy: Option<String>,
}

impl FetchedResponse {
    pub fn has_stream(&self) -> bool {
        self.stream.is_some()
    }

    pub fn business_headers(&self) -> Vec<(String, String)> {
        self.headers.business_view()
    }
}

pub struct RequestOutcome {
    pub response: FetchedResponse,
    pub attempts: Vec<AttemptRecord>,
}

#[derive(Clone)]
pub struct SessionConfig {
    pub retry: usize,
    pub timeout: Duration,
    pub control_limit: usize,
    pub max_redirects: usize,
    pub tls: TlsClient,
    pub resolver: Arc<dyn Resolver>,
    pub fallback: FallbackMap,
    pub clock: Arc<dyn HttpClock>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            retry: 10,
            timeout: Duration::from_secs(20),
            control_limit: CONTROL_BODY_LIMIT,
            max_redirects: consts::DEFAULT_MAX_REDIRECTS,
            tls: TlsClient::production(),
            resolver: Arc::new(SystemResolver),
            fallback: FallbackMap::default(),
            clock: Arc::new(RealClock),
        }
    }
}

pub struct Session {
    pub headers: Vec<(String, Option<String>)>,
    pub jar: CookieJar,
    pub proxy: Option<Arc<Mutex<Pool>>>,
    pub proxy_policy: ProxyPolicy,
    pub config: SessionConfig,
}

impl Session {
    pub fn new(
        headers: Vec<(String, Option<String>)>,
        proxy: Option<Arc<Mutex<Pool>>>,
        proxy_policy: ProxyPolicy,
        config: SessionConfig,
    ) -> Self {
        Self {
            headers,
            jar: CookieJar::default(),
            proxy,
            proxy_policy,
            config,
        }
    }

    pub fn set_header(&mut self, name: &str, value: Option<String>) {
        if let Some(entry) = self
            .headers
            .iter_mut()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        {
            entry.1 = value;
        } else {
            self.headers.push((name.to_string(), value));
        }
    }

    /// One logical `HttpReq.request` call: manual redirects, retries, proxy
    /// re-selection on every hop and the 600 boundary.
    pub async fn request(
        &mut self,
        method: &str,
        url: &str,
        data: Option<Vec<(String, String)>>,
        stream: bool,
    ) -> Result<RequestOutcome, HttpError> {
        let mut attempts = Vec::new();
        let mut retry = 0usize;
        let mut current = url.to_string();
        let mut history = vec![current.clone()];
        while retry < self.config.retry {
            let selected = self.select_proxy(&current)?;
            let proxy_addr = selected.as_ref().map(|spec| spec.addr.clone());
            let hop = self
                .fetch_hop(method, &current, data.as_deref(), stream, selected.as_ref())
                .await;
            match hop {
                Ok(mut response) => {
                    if let Some(addr) = &proxy_addr {
                        self.record_proxy(addr, true);
                    }
                    let content_length = match response.headers.joined("content-length") {
                        Some(value) => match value.trim().parse::<i64>() {
                            Ok(length) => length,
                            Err(_) => {
                                attempts.push(AttemptRecord {
                                    url: response.url.clone(),
                                    method: method.to_string(),
                                    status: Some(response.status),
                                    proxy: proxy_addr.clone(),
                                    content_length: None,
                                    error: Some(format!("invalid content-length {value}")),
                                });
                                let message =
                                    format!("invalid literal for int() with base 10: '{value}'");
                                return Err(HttpError::InvalidContentLength(message));
                            }
                        },
                        None if !stream => response.body.len() as i64,
                        None => 0,
                    };
                    response.content_length = content_length;
                    attempts.push(AttemptRecord {
                        url: response.url.clone(),
                        method: method.to_string(),
                        status: Some(response.status),
                        proxy: proxy_addr.clone(),
                        content_length: Some(content_length),
                        error: None,
                    });
                    for set_cookie in response.headers.get_all("set-cookie") {
                        self.jar.store(
                            &url::Url::parse(&response.url).unwrap_or_else(|_| {
                                url::Url::parse("http://invalid/").expect("static url")
                            }),
                            &set_cookie,
                            self.config.clock.now(),
                        );
                    }
                    if (300..400).contains(&response.status)
                        && let Some(location) = response.headers.joined("location")
                    {
                        if let Some(stream) = response.stream.take() {
                            stream.close().await;
                        }
                        history.push(current.clone());
                        if history.len() > self.config.max_redirects {
                            return Ok(RequestOutcome {
                                response: fake_response(&history[0]),
                                attempts,
                            });
                        }
                        current = location;
                        continue;
                    }
                    if let Some(addr) = &proxy_addr
                        && response.content_length < 1024
                        && !stream
                        && response
                            .text
                            .starts_with("Your IP address has been temporarily banned")
                    {
                        let expire = parse_human_time(&response.text) as f64;
                        self.not_good(addr, expire);
                        continue;
                    }
                    response.real_url = history.last().cloned().unwrap_or_default();
                    response.proxy = proxy_addr;
                    return Ok(RequestOutcome { response, attempts });
                }
                Err(error) => {
                    attempts.push(AttemptRecord {
                        url: current.clone(),
                        method: method.to_string(),
                        status: None,
                        proxy: proxy_addr.clone(),
                        content_length: None,
                        error: Some(error.to_string()),
                    });
                    if let Some(addr) = &proxy_addr
                        && error.retryable()
                        && !error.is_bad_status_line()
                    {
                        self.record_proxy(addr, false);
                    }
                    if error.retryable() {
                        self.config
                            .clock
                            .sleep(self.config.clock.random() + 0.618)
                            .await;
                        retry += 1;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Ok(RequestOutcome {
            response: fake_response(&history[0]),
            attempts,
        })
    }

    fn select_proxy(&self, url: &str) -> Result<Option<ProxySpec>, HttpError> {
        let Some(pool) = &self.proxy else {
            return Ok(None);
        };
        if !policy_matches(self.proxy_policy, url) {
            return Ok(None);
        }
        let mut pool = pool.lock().unwrap();
        pool.select().map(Some).map_err(HttpError::Pool)
    }

    fn record_proxy(&self, addr: &str, success: bool) {
        if let Some(pool) = &self.proxy
            && let Ok(mut pool) = pool.lock()
        {
            pool.trace(addr, Some(!success), None);
        }
    }

    fn not_good(&self, addr: &str, expire: f64) {
        if let Some(pool) = &self.proxy
            && let Ok(mut pool) = pool.lock()
        {
            pool.not_good(addr, proxy::MAX_FAIL, expire);
        }
    }

    fn session_headers(&self, url: &url::Url, host_header: &str) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut has_cookie = false;
        for (name, value) in &self.headers {
            let Some(value) = value else {
                continue;
            };
            if name.eq_ignore_ascii_case("Cookie") {
                has_cookie = true;
            }
            headers.push((name.clone(), value.clone()));
        }
        if !has_cookie && let Some(cookie) = self.jar.header_for(url, self.config.clock.now()) {
            headers.push(("Cookie".into(), cookie));
        }
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("Host"))
        {
            headers.push(("Host".into(), host_header.to_string()));
        }
        headers
    }

    fn business_headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter_map(|(name, value)| value.as_ref().map(|value| (name.clone(), value.clone())))
            .collect()
    }

    async fn fetch_hop(
        &mut self,
        method: &str,
        url_text: &str,
        data: Option<&[(String, String)]>,
        stream: bool,
        selected: Option<&ProxySpec>,
    ) -> Result<FetchedResponse, HttpError> {
        let url = url::Url::parse(url_text).map_err(|error| HttpError::Url(error.to_string()))?;
        let scheme = url.scheme().to_string();
        if scheme != "http" && scheme != "https" {
            return Err(HttpError::Url(format!("unsupported scheme {scheme}")));
        }
        let host = url
            .host_str()
            .ok_or_else(|| HttpError::Url("missing host".into()))?
            .to_string();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| HttpError::Url("missing port".into()))?;
        let target_tls = scheme == "https";

        if let Some(ProxySpec {
            kind: ProxyKind::Glype(spec),
            glype,
            ..
        }) = selected
        {
            let state = glype
                .clone()
                .ok_or_else(|| HttpError::Glype("glype state missing".into()))?;
            return self
                .fetch_glype_hop(&url, spec, Arc::clone(&state), method, data, stream)
                .await;
        }

        let fallback = selected
            .is_none()
            .then(|| self.config.fallback.choose(&host))
            .flatten();
        let fallback_port = url.port().unwrap_or(443);
        let host_header = if fallback.is_some() || port == default_port(&scheme) {
            host.clone()
        } else {
            format!("{host}:{port}")
        };

        let mut headers = self.session_headers(&url, &host_header);
        let mut body = Vec::new();
        if let Some(pairs) = data {
            body = url::form_urlencoded::Serializer::<String>::new(String::new())
                .extend_pairs(pairs)
                .finish()
                .into_bytes();
            if !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
            {
                headers.push((
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                ));
            }
            if !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))
            {
                headers.push(("Content-Length".into(), body.len().to_string()));
            }
        }
        let path = {
            let mut path = url.path().to_string();
            if path.is_empty() {
                path.push('/');
            }
            if let Some(query) = url.query() {
                path.push('?');
                path.push_str(query);
            }
            path
        };

        let (io, form) = if let Some(ip) = fallback {
            let address: IpAddr = ip
                .parse()
                .map_err(|error| HttpError::Dns(format!("{ip}: {error}")))?;
            let socket = SocketAddr::new(address, fallback_port);
            // DNS is skipped for a pinned fallback IP, but TCP + TLS still
            // share the one configured route deadline.
            match tokio::time::timeout(self.config.timeout, async {
                let stream = connect_socket(socket, self.config.timeout).await?;
                let tls = self
                    .config
                    .tls
                    .connect(&host, stream)
                    .await
                    .map_err(|error| HttpError::Tls(error.to_string()))?;
                Ok::<BoxStream, HttpError>(Box::new(tls))
            })
            .await
            {
                Ok(result) => (result?, RequestForm::Origin),
                Err(_) => {
                    return Err(HttpError::Connect("route establishment timed out".into()));
                }
            }
        } else if let Some(spec) = selected {
            let absolute = proxy::absolute_url(&scheme, &host, port, &path);
            proxy::open_route(
                Some(spec),
                &absolute,
                &host,
                port,
                target_tls,
                self.config.resolver.as_ref(),
                &self.config.tls,
                self.config.timeout,
            )
            .await?
        } else {
            proxy::open_route(
                None,
                "",
                &host,
                port,
                target_tls,
                self.config.resolver.as_ref(),
                &self.config.tls,
                self.config.timeout,
            )
            .await?
        };
        let absolute = proxy::absolute_url(&scheme, &host, port, &path);
        let exchange = self
            .exchange(io, form, method, &path, &absolute, &headers, body, stream)
            .await?;
        Ok(FetchedResponse {
            status: exchange.status,
            url: url.to_string(),
            real_url: url.to_string(),
            headers: exchange.headers,
            content_length: 0,
            body: exchange.body,
            text: exchange.text,
            stream: exchange.stream,
            proxy: None,
        })
    }

    async fn fetch_glype_hop(
        &mut self,
        url: &url::Url,
        spec: &GlypeSpec,
        state: Arc<Mutex<GlypeState>>,
        method: &str,
        data: Option<&[(String, String)]>,
        stream: bool,
    ) -> Result<FetchedResponse, HttpError> {
        let server =
            url::Url::parse(&spec.server).map_err(|error| HttpError::Url(error.to_string()))?;
        let host = server
            .host_str()
            .ok_or_else(|| HttpError::Url("glype host".into()))?
            .to_string();
        let port = server.port_or_known_default().unwrap_or(80);
        let proxy_tls = server.scheme() == "https";

        let mut headers = {
            let mut guard = state.lock().unwrap();
            spec.rewrite_headers(&self.business_headers(), url.as_str(), &mut guard)
        };
        // `requests` prepares the caller `data=` as a form body for every hop
        // the glype handler forwards, including the sslagree follow-up. The
        // reference rewrites only args[1], so the method and body survive.
        let body = match data {
            Some(pairs) => {
                let body = url::form_urlencoded::Serializer::<String>::new(String::new())
                    .extend_pairs(pairs)
                    .finish()
                    .into_bytes();
                if !headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
                {
                    headers.push((
                        "Content-Type".into(),
                        "application/x-www-form-urlencoded".into(),
                    ));
                }
                if !headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("Content-Length"))
                {
                    headers.push(("Content-Length".into(), body.len().to_string()));
                }
                body
            }
            None => Vec::new(),
        };
        let mut tried = 0usize;
        let mut exchange = loop {
            if tried == 2 {
                return Err(HttpError::Glype("can't bypass glype https warning".into()));
            }
            let target = spec.mkurl(url.as_str());
            let target_url =
                url::Url::parse(&target).map_err(|error| HttpError::Url(error.to_string()))?;
            let path = path_and_query(&target_url);
            let (io, form) = proxy::open_route(
                None,
                "",
                &host,
                port,
                proxy_tls,
                self.config.resolver.as_ref(),
                &self.config.tls,
                self.config.timeout,
            )
            .await?;
            let exchange = self
                .exchange(
                    io,
                    form,
                    method,
                    &path,
                    &target,
                    &headers,
                    body.clone(),
                    stream,
                )
                .await?;
            if exchange.text.contains(proxy::ssl_agree_marker()) {
                drop(exchange);
                let follow_url = spec.process_php_url();
                let follow_parsed = url::Url::parse(&follow_url)
                    .map_err(|error| HttpError::Url(error.to_string()))?;
                let (io, form) = proxy::open_route(
                    None,
                    "",
                    &host,
                    port,
                    proxy_tls,
                    self.config.resolver.as_ref(),
                    &self.config.tls,
                    self.config.timeout,
                )
                .await?;
                let follow = self
                    .exchange(
                        io,
                        form,
                        "GET",
                        &path_and_query(&follow_parsed),
                        &follow_url,
                        &headers,
                        body.clone(),
                        false,
                    )
                    .await?;
                if let Some(cookie) = follow.headers.joined("set-cookie") {
                    merge_glype_cookie(&mut headers, &cookie, &state);
                }
                tried += 1;
                continue;
            }
            break exchange;
        };

        let content_type = exchange.headers.joined("content-type");
        match content_type {
            None => {
                return Err(HttpError::Glype(
                    ProxyError::GlypeMissingContentType.to_string(),
                ));
            }
            Some(value) if value.starts_with("text") => {
                if exchange.stream.is_some() {
                    if let Some(stream_body) = exchange.stream.take() {
                        stream_body.close().await;
                    }
                    return Err(HttpError::Glype(
                        "'NoneType' object has no attribute 'decode'".into(),
                    ));
                }
                let processed = spec
                    .process_text(&exchange.body)
                    .map_err(|error| HttpError::Glype(error.to_string()))?;
                exchange.body = processed;
                exchange.text = String::from_utf8_lossy(&exchange.body).into_owned();
            }
            Some(_) => {}
        }

        let mut business = exchange.headers.business_view();
        {
            let mut guard = state.lock().unwrap();
            proxy::apply_glype_response_cookies(&mut business, &mut guard);
        }
        let mut entries = Vec::with_capacity(business.len());
        for (name, value) in business {
            entries.push((latin1_bytes(&name)?, latin1_bytes(&value)?));
        }
        exchange.headers = ResponseHeaders::from_raw(entries);
        Ok(FetchedResponse {
            status: exchange.status,
            url: url.to_string(),
            real_url: url.to_string(),
            headers: exchange.headers,
            content_length: 0,
            body: exchange.body,
            text: exchange.text,
            stream: exchange.stream,
            proxy: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn exchange(
        &mut self,
        io: BoxStream,
        form: RequestForm,
        method: &str,
        path: &str,
        absolute: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
        stream: bool,
    ) -> Result<Exchange, HttpError> {
        let uri: Uri = match form {
            RequestForm::Origin => Uri::try_from(path),
            RequestForm::Absolute => Uri::try_from(absolute),
        }
        .map_err(|error: http::uri::InvalidUri| HttpError::Url(error.to_string()))?;
        let capture = Arc::new(Mutex::new(Capture::default()));
        let io = TokioIo::new(CaptureIo {
            inner: io,
            capture: Arc::clone(&capture),
        });
        let (mut sender, connection) =
            tokio::time::timeout(self.config.timeout, http1::handshake::<_, Full<Bytes>>(io))
                .await
                .map_err(|_| HttpError::Connect("handshake timed out".into()))?
                .map_err(|error| HttpError::Protocol(error.to_string()))?;
        // The connection future owns the socket and is polled locally: no
        // spawned driver exists, so any return/drop below closes the peer.
        let mut connection: ConnectionFuture = Box::pin(connection);
        let mut builder = Request::builder()
            .method(
                Method::from_bytes(method.as_bytes())
                    .map_err(|error| HttpError::Protocol(error.to_string()))?,
            )
            .uri(uri);
        for (name, value) in headers {
            validate_header_text(value)?;
            let value = latin1_bytes(value)?;
            let header = HeaderValue::from_bytes(&value)
                .map_err(|error| HttpError::Header(error.to_string()))?;
            builder = builder.header(name.as_str(), header);
        }
        let request = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| HttpError::Protocol(error.to_string()))?;
        let mut sending = Box::pin(sender.send_request(request));
        let response = match tokio::time::timeout(self.config.timeout, async {
            tokio::select! {
                biased;
                result = &mut sending => {
                    result.map_err(|error| HttpError::Protocol(error.to_string()))
                }
                result = &mut connection => {
                    // The connection future may complete in the same poll
                    // that delivers a fully buffered response; give the
                    // sender future its chance to yield that response before
                    // reporting the connection as closed.
                    match (&mut sending).await {
                        Ok(response) => Ok(response),
                        Err(error) => Err(HttpError::Protocol(match result {
                            Ok(()) => format!("connection closed before the response: {error}"),
                            Err(error) => error.to_string(),
                        })),
                    }
                }
            }
        })
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(HttpError::Connect("request timed out".into())),
        };
        drop(sending);
        let status = response.status().as_u16() as i64;
        let headers = {
            let captured = capture.lock().unwrap().clone();
            parse_raw_headers(&captured.bytes).unwrap_or_else(|| {
                let mut entries = Vec::new();
                for (name, value) in response.headers() {
                    entries.push((name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()));
                }
                ResponseHeaders::from_raw(entries)
            })
        };
        let text_response = headers
            .joined("content-type")
            .is_some_and(|value| value.starts_with("text"));
        let (_, incoming) = response.into_parts();
        if stream && !text_response {
            return Ok(Exchange {
                status,
                headers,
                body: Vec::new(),
                text: String::new(),
                stream: Some(StreamBody {
                    incoming: Some(incoming),
                    timeout: self.config.timeout,
                    read: 0,
                    finished: false,
                    connection: Some(connection),
                    sender: Some(sender),
                }),
            });
        }
        let body = read_bounded(
            incoming,
            self.config.timeout,
            self.config.control_limit,
            Some(connection),
        )
        .await?;
        let text = String::from_utf8_lossy(&body).into_owned();
        Ok(Exchange {
            status,
            headers,
            body,
            text,
            stream: None,
        })
    }
}

struct Exchange {
    status: i64,
    headers: ResponseHeaders,
    body: Vec<u8>,
    text: String,
    stream: Option<StreamBody>,
}

fn path_and_query(url: &url::Url) -> String {
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

fn merge_glype_cookie(
    headers: &mut Vec<(String, String)>,
    set_cookie: &str,
    state: &Arc<Mutex<GlypeState>>,
) {
    let index = headers
        .iter()
        .position(|(name, _)| name.eq_ignore_ascii_case("Cookie"));
    let mut pairs = index
        .map(|index| crate::util::parse_cookie(&headers[index].1))
        .unwrap_or_default();
    for (key, value) in crate::util::parse_cookie(&set_cookie.replace(',', ";")) {
        if let Some(existing) = pairs.iter_mut().find(|(name, _)| *name == key) {
            existing.1 = value.clone();
        } else {
            pairs.push((key.clone(), value.clone()));
        }
        if key == "s" {
            state.lock().unwrap().s = Some(value);
        }
    }
    let rebuilt = crate::util::make_cookie(&pairs);
    match index {
        Some(index) => headers[index].1 = rebuilt,
        None => headers.push(("Cookie".into(), rebuilt)),
    }
}

fn fake_response(url: &str) -> FetchedResponse {
    FetchedResponse {
        status: 600,
        url: url.to_string(),
        real_url: url.to_string(),
        headers: ResponseHeaders::default(),
        content_length: 0,
        body: Vec::new(),
        text: String::new(),
        stream: None,
        proxy: None,
    }
}

fn default_port(scheme: &str) -> u16 {
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

fn policy_matches(policy: ProxyPolicy, url: &str) -> bool {
    use std::sync::OnceLock;
    static WEBPAGE: OnceLock<regex::Regex> = OnceLock::new();
    let webpage = WEBPAGE.get_or_init(|| {
        regex::Regex::new(r"^https*://([^.]+\.)*(?:[g.]*e-|ex)hentai\.org")
            .expect("webpage policy regex")
    });
    match policy {
        ProxyPolicy::All => true,
        ProxyPolicy::Webpage => webpage.is_match(url),
        ProxyPolicy::ImageOnly => !webpage.is_match(url),
    }
}

async fn connect_socket(address: SocketAddr, timeout: Duration) -> Result<TcpStream, HttpError> {
    match tokio::time::timeout(timeout, TcpStream::connect(address)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(HttpError::Connect(error.to_string())),
        Err(_) => Err(HttpError::Connect("connect timed out".into())),
    }
}

async fn read_bounded(
    mut incoming: hyper::body::Incoming,
    timeout: Duration,
    limit: usize,
    mut connection: Option<ConnectionFuture>,
) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let frame =
            match tokio::time::timeout(timeout, next_body_data(&mut incoming, &mut connection))
                .await
            {
                Ok(Ok(Some(data))) => data,
                Ok(Ok(None)) => return Ok(body),
                Ok(Err(error)) => return Err(HttpError::Protocol(error)),
                Err(_) => return Err(HttpError::Connect("read timed out".into())),
            };
        if body.len() + frame.len() > limit {
            return Err(HttpError::BodyTooLarge { limit });
        }
        body.extend_from_slice(&frame);
    }
}

// --- raw header capture ----------------------------------------------------

#[derive(Clone, Default)]
struct Capture {
    bytes: Vec<u8>,
    done: bool,
}

struct CaptureIo<T> {
    inner: T,
    capture: Arc<Mutex<Capture>>,
}

impl<T: AsyncRead + Unpin> AsyncRead for CaptureIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let filled = buf.filled();
            if filled.len() > before {
                let mut capture = self.capture.lock().unwrap();
                if !capture.done {
                    capture.bytes.extend_from_slice(&filled[before..]);
                    if capture.bytes.windows(4).any(|window| window == b"\r\n\r\n")
                        || capture.bytes.len() >= HEADER_LIMIT
                    {
                        capture.done = true;
                    }
                }
            }
        }
        poll
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for CaptureIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn parse_raw_headers(bytes: &[u8]) -> Option<ResponseHeaders> {
    let end = bytes.windows(4).position(|window| window == b"\r\n\r\n")?;
    let block = &bytes[..end + 4];
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(block) {
        Ok(httparse::Status::Complete(_)) => {
            let mut entries = Vec::new();
            for header in response.headers {
                entries.push((header.name.as_bytes().to_vec(), header.value.to_vec()));
            }
            Some(ResponseHeaders::from_raw(entries))
        }
        Ok(httparse::Status::Partial) | Err(_) => None,
    }
}

// --- test/probe helpers ----------------------------------------------------

/// Test/probe helper: build a session that resolves everything through an
/// explicit map and never sleeps.
pub fn probe_session(
    headers: Vec<(String, Option<String>)>,
    resolver: Arc<MapResolver>,
    fallback: FallbackMap,
    clock: Arc<dyn HttpClock>,
    proxy: Option<Arc<Mutex<Pool>>>,
    policy: ProxyPolicy,
) -> Session {
    Session::new(
        headers,
        proxy,
        policy,
        SessionConfig {
            retry: 3,
            timeout: Duration::from_secs(5),
            tls: TlsClient::production(),
            resolver,
            fallback,
            clock,
            ..SessionConfig::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Barrier, oneshot};

    use super::*;

    fn quiet_session() -> Session {
        Session::new(
            vec![("User-Agent".into(), Some("xeHentai-test".into()))],
            None,
            crate::util::ProxyPolicy::All,
            SessionConfig {
                retry: 2,
                timeout: Duration::from_secs(2),
                tls: TlsClient::production(),
                resolver: Arc::new(MapResolver::default()),
                fallback: FallbackMap::empty(),
                clock: Arc::new(TestClock::new(0.0)),
                ..SessionConfig::default()
            },
        )
    }

    async fn serve_once(
        listener: TcpListener,
        response: Vec<u8>,
    ) -> tokio::task::JoinHandle<Vec<u8>> {
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(&response).await.unwrap();
            socket.shutdown().await.unwrap();
            request
        })
    }

    #[tokio::test]
    async fn transport_get_preserves_header_order_duplicates_and_cookies() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = serve_once(
            listener,
            b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1; Path=/\r\nContent-Type: text/html; charset=utf-8\r\nX-Dup: one\r\nX-Dup: two\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
        )
        .await;
        let mut session = quiet_session();
        let outcome = session
            .request("GET", &format!("http://127.0.0.1:{port}/page"), None, false)
            .await
            .expect("request");
        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.response.text, "hello");
        assert_eq!(outcome.response.content_length, 5);
        let headers = outcome.response.business_headers();
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Set-Cookie",
                "Content-Type",
                "X-Dup",
                "X-Dup",
                "Content-Length"
            ]
        );
        assert_eq!(
            outcome.response.headers.joined("x-dup").as_deref(),
            Some("one, two")
        );
        let request = server.await.unwrap();
        let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
        assert!(request.starts_with("get /page http/1.1\r\n"));
        assert!(request.contains("host: 127.0.0.1:"), "{request}");
        assert!(!request.contains("cookie:"));

        // The Set-Cookie jar is attached to a second request on the same
        // session because no explicit Cookie header exists.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = serve_once(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        )
        .await;
        let outcome = session
            .request("GET", &format!("http://127.0.0.1:{port}/next"), None, false)
            .await
            .expect("second request");
        assert_eq!(outcome.response.text, "ok");
        let request = String::from_utf8_lossy(&server.await.unwrap()).to_ascii_lowercase();
        assert!(request.contains("cookie: a=1"), "{request}");
    }

    #[tokio::test]
    async fn transport_manual_redirects_keep_real_url_history() {
        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_port = first.local_addr().unwrap().port();
        let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_port = second.local_addr().unwrap().port();
        let second_server = serve_once(
            second,
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlast".to_vec(),
        )
        .await;
        let location = format!("http://127.0.0.1:{second_port}/final");
        let response =
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n");
        let first_server = serve_once(first, response.into_bytes()).await;
        let mut session = quiet_session();
        let outcome = session
            .request(
                "GET",
                &format!("http://127.0.0.1:{first_port}/start"),
                None,
                false,
            )
            .await
            .expect("redirect");
        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.response.text, "last");
        assert_eq!(outcome.response.url, location);
        assert_eq!(
            outcome.response.real_url,
            format!("http://127.0.0.1:{first_port}/start")
        );
        assert_eq!(outcome.attempts.len(), 2);
        first_server.await.unwrap();
        second_server.await.unwrap();
    }

    #[tokio::test]
    async fn transport_retries_use_random_plus_0618_backoff() {
        // A port with no listener produces a connection error every attempt.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let clock = Arc::new(TestClock::new(0.25));
        let mut session = Session::new(
            vec![],
            None,
            crate::util::ProxyPolicy::All,
            SessionConfig {
                retry: 2,
                timeout: Duration::from_secs(1),
                tls: TlsClient::production(),
                resolver: Arc::new(MapResolver::default()),
                fallback: FallbackMap::empty(),
                clock: clock.clone(),
                ..SessionConfig::default()
            },
        );
        let outcome = session
            .request("GET", &format!("http://127.0.0.1:{port}/x"), None, false)
            .await
            .expect("fake response");
        assert_eq!(outcome.response.status, 600);
        assert_eq!(outcome.attempts.len(), 2);
        assert_eq!(clock.sleeps(), vec![0.868, 0.868]);
        assert_eq!(outcome.response.url, format!("http://127.0.0.1:{port}/x"));
        assert_eq!(outcome.response.real_url, outcome.response.url);
    }

    #[tokio::test]
    async fn transport_streams_binary_without_buffering_and_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = vec![7u8; 1024 * 1024];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let server = serve_once(listener, response).await;
        let mut session = quiet_session();
        let outcome = session
            .request("GET", &format!("http://127.0.0.1:{port}/image"), None, true)
            .await
            .expect("stream");
        assert!(outcome.response.has_stream());
        assert_eq!(outcome.response.content_length, body.len() as i64);
        assert!(outcome.response.body.is_empty());
        let mut stream = outcome.response.stream.expect("stream");
        let mut read = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.expect("chunk") {
            read.extend_from_slice(&chunk);
        }
        assert_eq!(read, body);
        assert_eq!(stream.bytes_read(), body.len() as i64);
        stream.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn transport_control_body_limit_is_explicit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = vec![b'x'; 200];
        let mut response =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 200\r\n\r\n".to_vec();
        response.extend_from_slice(&body);
        let server = serve_once(listener, response).await;
        let mut session = quiet_session();
        session.config.control_limit = 64;
        let error = match session
            .request("GET", &format!("http://127.0.0.1:{port}/big"), None, false)
            .await
        {
            Ok(_) => panic!("expected body limit"),
            Err(error) => error,
        };
        assert!(matches!(error, HttpError::BodyTooLarge { .. }), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn one_worker_processes_items_sequentially() {
        let (items_tx, items_rx) = mpsc::channel::<i32>(8);
        let (done_tx, mut done_rx) = mpsc::channel::<i32>(8);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let worker = spawn(items_rx, {
            let seen = Arc::clone(&seen);
            let done_tx = done_tx.clone();
            move |item| {
                let seen = Arc::clone(&seen);
                let done_tx = done_tx.clone();
                async move {
                    seen.lock().unwrap().push(item);
                    done_tx.send(item).await.unwrap();
                }
            }
        });
        items_tx.send(1).await.unwrap();
        items_tx.send(2).await.unwrap();
        drop(items_tx);
        worker.wait().await;
        assert_eq!(done_rx.recv().await, Some(1));
        assert_eq!(done_rx.recv().await, Some(2));
        assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
    }

    async fn connect_and_wait(addr: std::net::SocketAddr) -> usize {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(b"GET\n").await.unwrap();
        let mut buffer = [0u8; 8];
        let read = stream.read(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..read], b"ok\n");
        stream.shutdown().await.unwrap();
        read
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_workers_have_requests_in_flight_at_the_same_time() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let server = tokio::spawn({
            let barrier = Arc::clone(&barrier);
            async move {
                for _ in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let barrier = Arc::clone(&barrier);
                    tokio::spawn(async move {
                        let mut request = [0u8; 16];
                        let read = socket.read(&mut request).await.unwrap();
                        assert_eq!(&request[..read], b"GET\n");
                        barrier.wait().await;
                        socket.write_all(b"ok\n").await.unwrap();
                        socket.shutdown().await.unwrap();
                    });
                }
            }
        });
        let (items_a, queue_a) = mpsc::channel::<usize>(2);
        let (items_b, queue_b) = mpsc::channel::<usize>(2);
        let (done_tx, mut done_rx) = mpsc::channel::<usize>(4);
        let make_worker = |queue, done_tx: mpsc::Sender<usize>| {
            spawn(queue, move |item| {
                let done_tx = done_tx.clone();
                async move {
                    connect_and_wait(addr).await;
                    done_tx.send(item).await.unwrap();
                }
            })
        };
        let worker_a = make_worker(queue_a, done_tx.clone());
        let worker_b = make_worker(queue_b, done_tx.clone());
        items_a.send(1).await.unwrap();
        items_b.send(2).await.unwrap();
        let mut finished = Vec::new();
        for _ in 0..2 {
            finished.push(
                tokio::time::timeout(Duration::from_secs(5), done_rx.recv())
                    .await
                    .expect("both workers must complete")
                    .expect("worker result"),
            );
        }
        finished.sort_unstable();
        assert_eq!(finished, vec![1, 2]);
        drop(items_a);
        drop(items_b);
        worker_a.wait().await;
        worker_b.wait().await;
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_waits_for_the_task_and_closes_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 16];
            let read = socket.read(&mut request).await.unwrap();
            assert_eq!(&request[..read], b"GET\n");
            accepted_tx.send(()).unwrap();
            let mut rest = Vec::new();
            let _ = socket.read_to_end(&mut rest).await;
            closed_tx.send(()).unwrap();
        });
        let (items_tx, items_rx) = mpsc::channel::<i32>(4);
        let worker = spawn(items_rx, move |_item| async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(b"GET\n").await.unwrap();
            let mut buffer = [0u8; 16];
            let _ = stream.read(&mut buffer).await.unwrap();
        });
        items_tx.send(1).await.unwrap();
        accepted_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), worker.shutdown())
            .await
            .expect("shutdown must finish");
        tokio::time::timeout(Duration::from_secs(3), closed_rx)
            .await
            .expect("connection must close")
            .expect("server signal");
        server.await.unwrap();
        drop(items_tx);
    }
}
