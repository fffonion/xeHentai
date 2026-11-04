//! The updater's own native HTTP transport.
//!
//! This is the ONLY network route of an update check. Each hop is one
//! `worker::Session::get_once` (a direct GET: no retry, proxy, cookie jar,
//! redirect or fallback), and this module owns everything above it:
//!
//! * the complete trusted origin tuples from `github.rss` — never a
//!   re-combinable set of schemes, hosts and ports — plus the one explicitly
//!   authorized CDN origin, which is admitted ONLY as a redirect target of a
//!   package request;
//! * per-hop public-unicast address classification (URL literals before the
//!   request, DNS answers before the connect) with a loopback exception only
//!   for the one binding that is itself a loopback literal;
//! * the 5-redirect chain, relative `Location`, and an immediate refusal on the
//!   sixth redirect in a row;
//! * the response body bound (`MAX_PAYLOAD_BYTES` + 1 MiB) and strict UTF-8 for
//!   text consumers, with raw bytes preserved for binary ones;
//! * one 30 s transport deadline for the whole chain (connect 10 s), expressed
//!   as one plain future so cancellation just drops the locally held socket.
//!
//! There is no fallback, no implicit retry, no second HTTP route and no
//! application-visible transport options.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ip::traits::Address as _;
use url::{Host, Url};

use crate::proxy::{LookupFuture, Resolver, SystemResolver, TlsClient};
use crate::updater::UpdateError;
use crate::updater::{SourceBinding, UPDATER_RSS};
use crate::worker::{FallbackMap, RealClock, Session, SessionConfig};

use super::package;

/// The response bound of one updater transport: the largest signed payload plus
/// one mebibyte of sidecar/descriptor headroom, exactly as before.
pub(crate) const fn body_limit() -> usize {
    package::MAX_PAYLOAD_BYTES + 1024 * 1024
}

/// One transport deadline for the whole redirect chain, as the reference
/// request deadline: not a per-step poll and not a VM timer.
pub(crate) const CHAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Core follows at most five redirects and refuses the sixth redirect response
/// without requesting the next URL.
pub(crate) const MAX_REDIRECTS: usize = 5;

const TIMEOUT_ERROR: &str = "update transport deadline exceeded";

/// What a request is FOR. The same fixture origin can legitimately be the API
/// origin and the package origin, so the purpose — never an origin match — is
/// what authorizes the CDN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    /// Commit metadata: exactly the API origin, initial URL and every redirect.
    Api,
    /// The signed package (descriptor, signature, payload): the package origin
    /// initially, and on a redirect either the package origin or the explicit
    /// CDN origin.
    Package,
}

impl Purpose {
    fn name(self) -> &'static str {
        match self {
            Self::Api => "commit metadata",
            Self::Package => "package download",
        }
    }
}

/// The trusted source: the API tuple, the package tuple and the one explicit
/// CDN tuple.
pub(crate) struct Transport {
    api: Origin,
    package: Origin,
    cdn: Option<Origin>,
    resolver: Arc<dyn Resolver>,
    limit: usize,
}

#[derive(Debug)]
pub(crate) struct Response {
    pub status: i64,
    pub body: Vec<u8>,
}

impl Response {
    /// Strict text view: the reference `bytes::to_utf8` contract, never the
    /// worker's lossy `text`.
    pub(crate) fn text(&self) -> Result<String, UpdateError> {
        String::from_utf8(self.body.clone())
            .map_err(|error| runtime_error(format!("response body is not valid UTF-8: {error}")))
    }
}

/// One parsed origin: scheme, typed host and effective port.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: HostKind,
    port: u16,
    /// True only for an origin that is itself a loopback IP literal (an owned
    /// fixture binding). A DNS name never inherits this.
    loopback_literal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostKind {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    Name(String),
}

impl Origin {
    fn parse(origin: &str, what: &'static str) -> Result<Self, UpdateError> {
        let url = Url::parse(origin).map_err(|error| {
            UpdateError::Binding(format!("{what} origin {origin:?} is not a URL: {error}"))
        })?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(UpdateError::Binding(format!(
                "{what} origin must not carry credentials"
            )));
        }
        if url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
            return Err(UpdateError::Binding(format!(
                "{what} origin must be scheme://host[:port] without a path"
            )));
        }
        let scheme = url.scheme().to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(UpdateError::Binding(format!(
                "{what} origin scheme {scheme:?} is not http(s)"
            )));
        }
        let host = match url.host() {
            Some(Host::Ipv4(address)) => HostKind::V4(address),
            Some(Host::Ipv6(address)) => HostKind::V6(address),
            Some(Host::Domain(name)) => HostKind::Name(name.to_ascii_lowercase()),
            None => {
                return Err(UpdateError::Binding(format!("{what} origin has no host")));
            }
        };
        let port = url
            .port_or_known_default()
            .ok_or_else(|| UpdateError::Binding(format!("{what} origin has no known port")))?;
        let loopback_literal = match host {
            HostKind::V4(address) => address.is_loopback(),
            HostKind::V6(address) => address.is_loopback(),
            HostKind::Name(_) => false,
        };
        Ok(Self {
            scheme,
            host,
            port,
            loopback_literal,
        })
    }
}

impl Transport {
    pub(crate) fn from_binding(binding: &SourceBinding) -> Result<Self, UpdateError> {
        Self::build(
            &binding.api_origin,
            &binding.package_download_origin,
            binding.package_cdn_origin.as_deref(),
            body_limit(),
            Arc::new(SystemResolver),
        )
    }

    fn build(
        api_origin: &str,
        package_origin: &str,
        cdn_origin: Option<&str>,
        limit: usize,
        resolver: Arc<dyn Resolver>,
    ) -> Result<Self, UpdateError> {
        Ok(Self {
            api: Origin::parse(api_origin, "api")?,
            package: Origin::parse(package_origin, "package download")?,
            cdn: match cdn_origin {
                Some(origin) => Some(Origin::parse(origin, "package cdn")?),
                None => None,
            },
            resolver,
            limit,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        api_origin: &str,
        package_origin: &str,
        cdn_origin: Option<&str>,
        limit: usize,
        resolver: Arc<dyn Resolver>,
    ) -> Result<Self, UpdateError> {
        Self::build(api_origin, package_origin, cdn_origin, limit, resolver)
    }

    /// Commit metadata: exactly the API origin. The CDN is never authorized
    /// here, neither as an initial URL nor as a redirect target.
    pub(crate) async fn get_api(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Response, UpdateError> {
        self.get(url, headers, Purpose::Api).await
    }

    /// One signed package fetch. The initial URL is the package origin; only a
    /// redirect of this request may use the explicitly bound CDN origin.
    pub(crate) async fn get_package(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Response, UpdateError> {
        self.get(url, headers, Purpose::Package).await
    }

    /// One logical GET: at most five redirects, every hop re-validated for the
    /// request's purpose and re-classified before it connects.
    async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        purpose: Purpose,
    ) -> Result<Response, UpdateError> {
        match tokio::time::timeout(CHAIN_TIMEOUT, self.follow(url, headers, purpose)).await {
            Ok(result) => result,
            Err(_) => Err(runtime_error(TIMEOUT_ERROR.into())),
        }
    }

    async fn follow(
        &self,
        url: &str,
        headers: &[(String, String)],
        purpose: Purpose,
    ) -> Result<Response, UpdateError> {
        let mut current = Url::parse(url)
            .map_err(|error| UpdateError::Binding(format!("request URL {url:?}: {error}")))?;
        let mut followed = 0usize;
        loop {
            let allow_loopback = self.check_origin(&current, purpose, followed > 0)?;
            let hop = self.hop(&current, headers, allow_loopback).await?;
            if matches!(hop.status, 301 | 302 | 303 | 307 | 308) {
                if followed == MAX_REDIRECTS {
                    return Err(runtime_error(format!(
                        "redirect limit exceeded after {MAX_REDIRECTS} redirects"
                    )));
                }
                let location = hop.location.ok_or_else(|| {
                    runtime_error("redirect response without a Location header".into())
                })?;
                current = current.join(&location).map_err(|error| {
                    runtime_error(format!("redirect Location {location:?}: {error}"))
                })?;
                followed += 1;
                continue;
            }
            return Ok(Response {
                status: hop.status,
                body: hop.body,
            });
        }
    }

    /// The URL must be exactly one of the tuples this purpose authorizes. The
    /// CDN is authorized only for a package redirect, never for an initial URL
    /// and never for commit metadata. An IP-literal host is classified here,
    /// before any request or DNS; a name is classified after its real
    /// resolution.
    fn check_origin(
        &self,
        url: &Url,
        purpose: Purpose,
        redirected: bool,
    ) -> Result<bool, UpdateError> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(UpdateError::Binding(
                "update request URL must not carry credentials".into(),
            ));
        }
        let scheme = url.scheme().to_ascii_lowercase();
        let host = match url.host() {
            Some(Host::Ipv4(address)) => HostKind::V4(address),
            Some(Host::Ipv6(address)) => HostKind::V6(address),
            Some(Host::Domain(name)) => HostKind::Name(name.to_ascii_lowercase()),
            None => {
                return Err(UpdateError::Binding(
                    "update request URL has no host".into(),
                ));
            }
        };
        let port = url
            .port_or_known_default()
            .ok_or_else(|| UpdateError::Binding("update request URL has no known port".into()))?;
        let mut allowed: Vec<&Origin> = Vec::with_capacity(3);
        match purpose {
            Purpose::Api => allowed.push(&self.api),
            Purpose::Package => {
                allowed.push(&self.package);
                if redirected && let Some(cdn) = &self.cdn {
                    allowed.push(cdn);
                }
            }
        }
        for origin in allowed {
            if origin.scheme != scheme || origin.host != host || origin.port != port {
                continue;
            }
            match &origin.host {
                HostKind::V4(address) => {
                    if !origin.loopback_literal && !is_public_address(IpAddr::V4(*address)) {
                        return Err(UpdateError::Binding(format!(
                            "bound address {address} is not a public unicast address"
                        )));
                    }
                }
                HostKind::V6(address) => {
                    if !origin.loopback_literal && !is_public_address(IpAddr::V6(*address)) {
                        return Err(UpdateError::Binding(format!(
                            "bound address {address} is not a public unicast address"
                        )));
                    }
                }
                HostKind::Name(_) => {}
            }
            return Ok(origin.loopback_literal);
        }
        Err(UpdateError::Binding(format!(
            "{}://{}{} is not a trusted origin for this {} request",
            scheme,
            url.host_str().unwrap_or(""),
            url.port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default(),
            purpose.name()
        )))
    }

    async fn hop(
        &self,
        url: &Url,
        headers: &[(String, String)],
        allow_loopback: bool,
    ) -> Result<Hop, UpdateError> {
        let config = SessionConfig {
            // A fresh session owns exactly one hop: no retry, no proxy, no
            // fallback, no cookie store.
            retry: 1,
            timeout: Duration::from_secs(30),
            control_limit: self.limit,
            tls: TlsClient::production(),
            resolver: Arc::new(GuardedResolver {
                inner: Arc::clone(&self.resolver),
                allow_loopback,
            }),
            fallback: FallbackMap::empty(),
            clock: Arc::new(RealClock),
            ..SessionConfig::default()
        };
        let mut session = Session::new(
            headers
                .iter()
                .map(|(name, value)| (name.clone(), Some(value.clone())))
                .collect(),
            None,
            crate::util::ProxyPolicy::All,
            config,
        );
        let response = session
            .get_once(url.as_str())
            .await
            .map_err(|error| runtime_error(format!("request failed: {error}")))?;
        Ok(Hop {
            status: response.status,
            location: response.headers.joined("location"),
            body: response.body,
        })
    }
}

struct Hop {
    status: i64,
    location: Option<String>,
    body: Vec<u8>,
}

fn runtime_error(message: String) -> UpdateError {
    UpdateError::Runtime {
        script: UPDATER_RSS,
        message: format!("transport: {message}"),
    }
}

/// True only for a public unicast address. An IPv4-mapped IPv6 address is
/// normalized to IPv4 first, so a mapped private address is never classified as
/// an IPv6 special case.
pub(crate) fn is_public_address(address: IpAddr) -> bool {
    let canonical = match address {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    };
    ip::any::Address::from(canonical).is_unicast_global()
}

/// The trusted resolver for one hop: the real lookup is filtered before the
/// connect, and a loopback answer is admitted only for the one origin that is
/// itself a loopback literal. An empty set is a failure, never a retry.
struct GuardedResolver {
    inner: Arc<dyn Resolver>,
    allow_loopback: bool,
}

impl GuardedResolver {
    fn filter(&self, mut addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
        addresses.retain(|address| {
            let ip = address.ip();
            (self.allow_loopback && ip.is_loopback()) || is_public_address(ip)
        });
        if addresses.is_empty() {
            return Err("no public unicast address for the trusted origin".into());
        }
        Ok(addresses)
    }
}

impl Resolver for GuardedResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let addresses = self.inner.resolve(host, port)?;
        self.filter(addresses)
    }

    fn lookup<'a>(&'a self, host: &'a str, port: u16) -> Option<LookupFuture<'a>> {
        let inner = self.inner.lookup(host, port)?;
        Some(Box::pin(async move {
            let addresses = inner.await?;
            self.filter(addresses)
        }))
    }
}
