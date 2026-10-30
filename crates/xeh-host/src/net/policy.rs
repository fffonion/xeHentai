use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use url::Url;

use crate::dto::{CommitGate, TransportHooks};

pub fn is_restricted_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_restricted_v4(ip),
        IpAddr::V6(ip) => is_restricted_v6(ip),
    }
}

fn is_restricted_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    matches!(octets[0], 0 | 10 | 127)
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 169 && octets[1] == 254)
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192
            && matches!(
                (octets[1], octets[2]),
                (0, 0) | (0, 2) | (31, 196) | (52, 193) | (88, 99) | (168, _) | (175, 48)
            ))
        || (octets[0] == 198
            && ((18..=19).contains(&octets[1]) || (octets[1] == 51 && octets[2] == 100)))
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 224
}

fn is_restricted_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_restricted_v4(mapped);
    }
    let segments = ip.segments();
    let outside_global_unicast = segments[0] & 0xe000 != 0x2000;
    let protocol_assignments = segments[0] == 0x2001 && segments[1] <= 0x01ff;
    let documentation = (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0);
    let six_to_four = segments[0] == 0x2002;
    let direct_delegation_as112 =
        segments[0] == 0x2620 && segments[1] == 0x004f && segments[2] == 0x8000;
    outside_global_unicast
        || protocol_assignments
        || documentation
        || six_to_four
        || direct_delegation_as112
}

pub struct ResolvedTarget {
    pub host: String,
    pub address: SocketAddr,
    pub candidates: Vec<SocketAddr>,
}

pub struct ResolveLimits<'a> {
    pub cancel: &'a CancellationToken,
    pub connect: Instant,
    pub dns_hold: Option<&'a CommitGate>,
    pub decoy: Option<SocketAddr>,
}

pub async fn admit_target(
    http: &vm::HttpConfig,
    url: &Url,
    limits: ResolveLimits<'_>,
) -> Result<ResolvedTarget, &'static str> {
    http.validate().map_err(|_| "policy_denied")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("policy_denied");
    }
    let scheme = url.scheme();
    if !http
        .allowed_schemes
        .iter()
        .any(|item| item.eq_ignore_ascii_case(scheme))
    {
        return Err("policy_denied");
    }
    let host = url.host_str().ok_or("policy_denied")?;
    if !http
        .allowed_hosts
        .iter()
        .any(|item| item.eq_ignore_ascii_case(host))
    {
        return Err("policy_denied");
    }
    let port = url.port_or_known_default().ok_or("policy_denied")?;
    if !http.allowed_ports.contains(&port) {
        return Err("policy_denied");
    }
    if let Some(hold) = limits.dns_hold {
        hold.arrive();
        hold.wait_bounded(limits.cancel, limits.connect)?;
    }
    let mut addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let looked = select_deadline(limits.cancel, limits.connect, async {
            tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| "resolution_failed")
        })
        .await?;
        looked.collect::<Vec<_>>()
    };
    if let Some(decoy) = limits.decoy {
        addresses.insert(0, decoy);
    }
    if addresses.is_empty() {
        return Err("resolution_failed");
    }
    if !http.allow_private_ips
        && addresses
            .iter()
            .any(|address| is_restricted_ip(address.ip()))
    {
        return Err("policy_denied");
    }
    let connectable = addresses
        .iter()
        .copied()
        .find(|address| limits.decoy != Some(*address))
        .unwrap_or(addresses[0]);
    Ok(ResolvedTarget {
        host: host.to_string(),
        address: connectable,
        candidates: addresses,
    })
}

/// Authorizes a request target whose name is delegated to a trusted proxy.
///
/// The destination hostname, scheme, port and literal-IP restriction are
/// checked exactly like a direct request, but no local name resolution is
/// performed. A caller must have an explicit trusted remote-DNS grant before
/// reaching this function; it does not weaken the direct path's pinning.
pub fn admit_remote_target(
    http: &vm::HttpConfig,
    url: &Url,
) -> Result<(String, u16), &'static str> {
    http.validate().map_err(|_| "policy_denied")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("policy_denied");
    }
    let scheme = url.scheme();
    if !http
        .allowed_schemes
        .iter()
        .any(|item| item.eq_ignore_ascii_case(scheme))
    {
        return Err("policy_denied");
    }
    let host = url.host_str().ok_or("policy_denied")?;
    if !http
        .allowed_hosts
        .iter()
        .any(|item| item.eq_ignore_ascii_case(host))
    {
        return Err("policy_denied");
    }
    let port = url.port_or_known_default().ok_or("policy_denied")?;
    if !http.allowed_ports.contains(&port) {
        return Err("policy_denied");
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !http.allow_private_ips && is_restricted_ip(ip) {
            return Err("policy_denied");
        }
    }
    Ok((host.to_string(), port))
}

pub async fn select_deadline<T>(
    cancel: &CancellationToken,
    deadline: Instant,
    work: impl std::future::Future<Output = Result<T, &'static str>>,
) -> Result<T, &'static str> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("cancelled"),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Err("deadline"),
        result = work => result,
    }
}

pub async fn until_deadline<T>(
    cancel: &CancellationToken,
    deadline: Instant,
    work: impl std::future::Future<Output = T>,
) -> Result<T, &'static str> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("cancelled"),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Err("deadline"),
        result = work => Ok(result),
    }
}

pub fn hooks_decoy(hooks: &TransportHooks) -> Option<SocketAddr> {
    hooks.dns_decoy
}
