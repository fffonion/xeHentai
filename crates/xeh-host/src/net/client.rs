use std::net::IpAddr;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue};
use tokio_util::sync::CancellationToken;
use vm::HttpConfig;

use crate::dto::HeaderEntry;
use crate::net::policy::{admit_target, ResolvedTarget};
use crate::observe::Metrics;

pub struct OpenedResponse {
    pub response: reqwest::Response,
}

pub async fn open_get(
    http: &HttpConfig,
    url: &str,
    headers: &[HeaderEntry],
    cancel: &CancellationToken,
    metrics: &Metrics,
) -> Result<OpenedResponse, &'static str> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "policy_denied")?;
    reject_headers(http, headers)?;
    let target = admit_target(http, &parsed, cancel).await?;
    metrics.record_connect(target.address);
    let client = build_client(http, &target)?;
    let mut request = client.get(parsed);
    for header in headers {
        let name = HeaderName::from_bytes(&header.name).map_err(|_| "header_rejected")?;
        let value = HeaderValue::from_bytes(&header.value).map_err(|_| "header_rejected")?;
        request = request.header(name, value);
    }
    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            tokio::task::yield_now().await;
            return Err("cancelled");
        }
        result = request.send() => result.map_err(transport_code)?,
    };
    if response.status().is_redirection() {
        return Err("redirect_unsupported");
    }
    if !response.status().is_success() {
        return Err("http_status");
    }
    Ok(OpenedResponse { response })
}

fn reject_headers(http: &HttpConfig, headers: &[HeaderEntry]) -> Result<(), &'static str> {
    if headers.len() > http.max_request_header_count {
        return Err("header_rejected");
    }
    let mut total = 0usize;
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
        total = total.saturating_add(header.name.len().saturating_add(header.value.len()));
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

fn build_client(
    http: &HttpConfig,
    target: &ResolvedTarget,
) -> Result<reqwest::Client, &'static str> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(http.connect_timeout)
        .read_timeout(http.stream_idle_timeout)
        .timeout(overall_timeout(http))
        .pool_max_idle_per_host(0)
        .danger_accept_invalid_certs(false);
    if target.host.parse::<IpAddr>().is_err() {
        builder = builder.resolve(&target.host, target.address);
    }
    builder.build().map_err(|_| "transport_failed")
}

fn overall_timeout(http: &HttpConfig) -> Duration {
    http.request_timeout.min(http.max_stream_duration)
}

fn transport_code(error: reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "idle_timeout"
    } else if error.is_connect() {
        "connect_failed"
    } else {
        "transport_failed"
    }
}
