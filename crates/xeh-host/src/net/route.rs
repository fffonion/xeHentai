//! Typed, immutable proxy route snapshots.
//!
//! A route is created from a trusted configured proxy endpoint before a job is
//! admitted. It owns the protocol, the proxy endpoint and the DNS delegation
//! grant; nothing here consults ambient proxy environment variables or a
//! mutable configuration document. Selection and scoring stay in the RSS
//! application; this module only carries the chosen endpoint and enforces the
//! protocol-level distinction between local and delegated target resolution.

use std::net::IpAddr;

/// Proxy protocol named by the configured address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    Http,
    Https,
    Socks4,
    Socks4a,
    Socks5,
    Socks5h,
    /// The source address regex accepts the suffix but the protocols library
    /// has no handler for it. Selecting one fails explicitly; it never falls
    /// back to a direct connection.
    Unsupported,
}

impl ProxyKind {
    pub fn code(self) -> &'static str {
        match self {
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
            ProxyKind::Socks4 => "socks4",
            ProxyKind::Socks4a => "socks4a",
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Socks5h => "socks5h",
            ProxyKind::Unsupported => "unsupported",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "http" => Some(ProxyKind::Http),
            "https" => Some(ProxyKind::Https),
            "socks4" => Some(ProxyKind::Socks4),
            "socks4a" => Some(ProxyKind::Socks4a),
            "socks5" => Some(ProxyKind::Socks5),
            "socks5h" => Some(ProxyKind::Socks5h),
            "unsupported" => Some(ProxyKind::Unsupported),
            _ => None,
        }
    }

    /// The proxy hop itself is wrapped in TLS for `https://` proxy addresses.
    pub fn outer_tls(self) -> bool {
        matches!(self, ProxyKind::Https)
    }

    /// Whether the target name (not merely the target port) crosses to the
    /// proxy without a local resolution step. Plain HTTP proxies always parse
    /// the destination from the absolute request target, so they are remote.
    pub fn delegates_target_dns(self) -> bool {
        matches!(
            self,
            ProxyKind::Http | ProxyKind::Https | ProxyKind::Socks4a | ProxyKind::Socks5h
        )
    }

    pub fn is_http_proxy(self) -> bool {
        matches!(self, ProxyKind::Http | ProxyKind::Https)
    }

    pub fn supported(self) -> bool {
        !matches!(self, ProxyKind::Unsupported)
    }
}

/// One immutable route snapshot captured before admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeProxyRoute {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// Set only by a trusted embedding capability that explicitly delegates
    /// destination DNS to this configured proxy endpoint. Remote-DNS modes
    /// fail closed without it.
    pub trusted_remote_dns: bool,
}

impl NativeProxyRoute {
    pub fn new(
        kind: ProxyKind,
        host: impl Into<String>,
        port: u16,
        trusted_remote_dns: bool,
    ) -> Result<Self, &'static str> {
        let host = host.into();
        if host.is_empty()
            || host.len() > 255
            || host
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b'/' || byte == b'@')
        {
            return Err("proxy_route_invalid");
        }
        if port == 0 {
            return Err("proxy_route_invalid");
        }
        Ok(Self {
            kind,
            host,
            port,
            trusted_remote_dns,
        })
    }

    pub fn proxy_ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }
}
