//! Functions from the reference `xeHentai/util/__init__.py`.
//!
//! The full reference surface is implemented and tested here; its callers
//! (tasks, proxy, filters) arrive in later milestones.
#![allow(dead_code)]

pub mod logger;

use std::sync::LazyLock;

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyPolicy {
    /// `RE_URL_IMAGE`: proxy only for images.
    ImageOnly,
    /// `RE_URL_ALL`: proxy every request.
    All,
    /// `RE_URL_WEBPAGE`: proxy only for web pages.
    Webpage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HtmlEscapeError {
    /// The reference implementation raises `ValueError` here.
    InvalidNumericEntity(String),
}

impl std::fmt::Display for HtmlEscapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidNumericEntity(value) => {
                write!(f, "invalid literal for int() with base 10: '{value}'")
            }
        }
    }
}

impl std::error::Error for HtmlEscapeError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathStyle {
    Posix,
    Windows,
}

#[allow(dead_code)]
impl PathStyle {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

/// Parse a `Cookie` header into ordered key/value pairs.
pub fn parse_cookie(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for part in text.split(';') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("secure") || part.eq_ignore_ascii_case("httponly") {
            continue;
        }
        let mut split = part.splitn(2, '=');
        let key = split.next().unwrap_or("");
        let value = split.next().unwrap_or("");
        if ["path", "expires", "domain", "max-age", "comment"]
            .iter()
            .any(|attribute| key.eq_ignore_ascii_case(attribute))
        {
            continue;
        }
        if let Some(existing) = out.iter_mut().find(|(name, _)| name == key) {
            existing.1 = value.to_string();
        } else {
            out.push((key.to_string(), value.to_string()));
        }
    }
    out
}

/// Serialize ordered cookie pairs in the reference format.
pub fn make_cookie(cookie: &[(String, String)]) -> String {
    cookie
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// Reference `make_ua` assembles a user-agent value but never returns it
/// (`ua = ...` without `return`). The observable result is `None`, so callers
/// must not send a generated User-Agent header. This quirk is preserved.
pub fn make_ua() -> Option<String> {
    None
}

pub fn get_proxy_policy(proxy_image_only: bool, proxy_image: bool) -> ProxyPolicy {
    if proxy_image_only {
        ProxyPolicy::ImageOnly
    } else if proxy_image {
        ProxyPolicy::All
    } else {
        ProxyPolicy::Webpage
    }
}

pub fn parse_human_time(text: &str) -> i64 {
    static DAYS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d+)\sdays*").expect("days regex"));
    static HOURS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d+)\shours*").expect("hours regex"));
    static MINUTES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d+)\sminutes*").expect("minutes regex"));
    let mut seconds = 0;
    if let Some(days) = DAYS.captures(text) {
        seconds += 86400 * days[1].parse::<i64>().unwrap_or(0);
    }
    if let Some(hours) = HOURS.captures(text) {
        seconds += 3600 * hours[1].parse::<i64>().unwrap_or(0);
    }
    match MINUTES.captures(text) {
        Some(minutes) => seconds + 60 * minutes[1].parse::<i64>().unwrap_or(0),
        None => seconds + 60,
    }
}

pub fn htmlescape(text: &str) -> Result<String, HtmlEscapeError> {
    static ENTITY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"&(#?)(\d{1,5}|\w{1,8}|[a-z]+);").expect("entity regex"));
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for captures in ENTITY.captures_iter(text) {
        let whole = captures.get(0).expect("whole match");
        out.push_str(&text[last..whole.start()]);
        let marker = &captures[1];
        let name = &captures[2];
        if marker == "#" {
            let code = name
                .parse::<u32>()
                .map_err(|_| HtmlEscapeError::InvalidNumericEntity(name.to_string()))?;
            let character = char::from_u32(code)
                .ok_or_else(|| HtmlEscapeError::InvalidNumericEntity(name.to_string()))?;
            out.push(character);
        } else {
            out.push_str(match name {
                "amp" => "&",
                "nbsp" => " ",
                "quot" => "\"",
                "lt" => "<",
                "gt" => ">",
                "copy" => "©",
                "reg" => "®",
                _ => "?",
            });
        }
        last = whole.end();
    }
    out.push_str(&text[last..]);
    Ok(out)
}

#[allow(dead_code)]
pub fn legalpath(text: &str) -> String {
    legalpath_with(text, PathStyle::current())
}

pub fn legalpath_with(text: &str, style: PathStyle) -> String {
    static POSIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[/:]").expect("posix filename regex"));
    static WINDOWS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"[|:?\\/*'"<>]"#).expect("windows filename regex"));
    match style {
        PathStyle::Posix => POSIX.replace_all(text, "").into_owned(),
        PathStyle::Windows => {
            let mut filtered = WINDOWS.replace_all(text, "").into_owned();
            // `\.+$` in the reference removes dots only at the very end (or
            // before one final newline); a trailing space protects them.
            if filtered.ends_with('\n') {
                filtered.pop();
            }
            filtered.trim_end_matches('.').trim_end().to_string()
        }
    }
}

pub fn human_size(size: i64) -> String {
    const MAXINT: i64 = i64::MAX;
    // The reference compares against MAXINT; for i64 that is equality.
    if size == MAXINT {
        return "UNL.".to_string();
    }
    let mut value = size as f64;
    for prefix in ["B", "KB", "MB", "GB", "TB"] {
        if value <= 1000.0 {
            let formatted = format!("{value:.2}");
            let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
            return format!("{trimmed} {prefix}");
        }
        value /= 1024.0;
    }
    format!("{value:.2} TB")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn cookie_parsing_matches_reference_rules() {
        let parsed = parse_cookie(
            "a=1; secure; b=x=y; Path=/; expires=Wed; c=2; c=3; HttpOnly; Domain=x; max-age=9; comment=c",
        );
        assert_eq!(parsed, pairs(&[("a", "1"), ("b", "x=y"), ("c", "3")]));
        assert_eq!(parse_cookie(""), pairs(&[("", "")]));
        assert_eq!(parse_cookie("secure=1"), pairs(&[("secure", "1")]));
        assert_eq!(parse_cookie("  k = v  "), pairs(&[("k ", " v")]));
    }

    #[test]
    fn cookie_serialization_keeps_order_and_equals() {
        assert_eq!(
            make_cookie(&pairs(&[("a", "1"), ("b", "x=y"), ("", "")])),
            "a=1;b=x=y;="
        );
        assert_eq!(make_cookie(&[]), "");
    }

    #[test]
    fn proxy_policy_matches_reference_precedence() {
        assert_eq!(get_proxy_policy(true, true), ProxyPolicy::ImageOnly);
        assert_eq!(get_proxy_policy(true, false), ProxyPolicy::ImageOnly);
        assert_eq!(get_proxy_policy(false, true), ProxyPolicy::All);
        assert_eq!(get_proxy_policy(false, false), ProxyPolicy::Webpage);
    }

    #[test]
    fn human_time_matches_reference_regexes() {
        assert_eq!(parse_human_time("1 day 2 hours 3 minutes"), 93780);
        assert_eq!(parse_human_time("1 day"), 86460);
        assert_eq!(parse_human_time("2 hours"), 7260);
        assert_eq!(parse_human_time("5 minutes"), 300);
        assert_eq!(parse_human_time("nothing"), 60);
        assert_eq!(parse_human_time("1days"), 60);
        assert_eq!(parse_human_time("10 days 5 minutes"), 864300);
    }

    #[test]
    fn html_escaping_matches_reference_and_is_single_pass() {
        assert_eq!(htmlescape("&amp;").unwrap(), "&");
        assert_eq!(htmlescape("&nbsp;").unwrap(), " ");
        assert_eq!(htmlescape("&#38;").unwrap(), "&");
        assert!(htmlescape("&#x27;").is_err());
        assert_eq!(htmlescape("&bogus;").unwrap(), "?");
        assert_eq!(htmlescape("&amp;lt;").unwrap(), "&lt;");
        assert_eq!(
            htmlescape("&copy; &reg; &quot; &lt; &gt;").unwrap(),
            "© ® \" < >"
        );
        assert_eq!(htmlescape("plain text").unwrap(), "plain text");
    }

    #[test]
    fn legal_path_filters_the_posix_set() {
        assert_eq!(
            legalpath_with("a/b:c*d?e\\f", PathStyle::Posix),
            "abc*d?e\\f"
        );
        assert_eq!(
            legalpath_with("中文/标题:01", PathStyle::Posix),
            "中文标题01"
        );
    }

    #[test]
    fn legal_path_filters_the_windows_set_and_trailing_dots() {
        assert_eq!(
            legalpath_with("a/b:c*d?\"e<f>g|h'", PathStyle::Windows),
            "abcdefgh"
        );
        assert_eq!(legalpath_with("name... ", PathStyle::Windows), "name...");
        assert_eq!(legalpath_with("name...", PathStyle::Windows), "name");
        assert_eq!(legalpath_with("a:b", PathStyle::Windows), "ab");
    }

    #[test]
    fn human_size_matches_reference_boundaries() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(1000), "1000 B");
        assert_eq!(human_size(1001), "0.98 KB");
        assert_eq!(human_size(1024), "1 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1_000_000), "976.56 KB");
        assert_eq!(human_size(1_048_576), "1 MB");
        assert_eq!(human_size(i64::MAX), "UNL.");
    }

    #[test]
    fn user_agent_keeps_the_reference_implicit_none_return() {
        assert_eq!(make_ua(), None);
    }
}
