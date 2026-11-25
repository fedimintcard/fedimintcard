//! The only way the payout module reaches the internet: one HTTPS GET of a JSON document, to a
//! public host.
//!
//! - HTTPS on port 443, a domain name (never an IP literal, never a name under a local-use
//!   suffix), no userinfo.
//! - The name is resolved here, and every address it resolves to must be public (no loopback,
//!   private, link-local, CGNAT, multicast, documentation, benchmarking, 6to4/Teredo/NAT64 or
//!   other special-purpose range). The connection is then pinned to exactly those addresses
//!   (`resolve_to_addrs`), so a second lookup cannot rebind the name to an inside address.
//! - No redirect is followed (a redirect is refused as unsafe), no proxy is used, and the body is
//!   read up to [`MAX_BODY`] bytes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Url;
use serde_json::Value;

pub const MAX_BODY: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Name suffixes that never name a public service.
const LOCAL_SUFFIXES: &[&str] =
    &["localhost", "local", "localdomain", "internal", "intranet", "lan", "home", "corp", "home.arpa", "onion", "invalid", "test", "arpa"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The URL, the name or an address it resolves to is not one the till may reach.
    Unsafe(String),
    /// Nothing answered (DNS, connect, timeout, TLS).
    Unreachable(String),
    /// It answered, but not with a usable JSON document.
    Refused(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unsafe(s) | FetchError::Unreachable(s) | FetchError::Refused(s) => f.write_str(s),
        }
    }
}

/// One GET of a JSON document. The real one is [`SafeFetch`]; tests stand in for the services.
pub trait Fetch: Send + Sync {
    fn get_json(&self, url: Url) -> BoxFuture<'_, Result<Value, FetchError>>;
}

pub fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let special = o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || o[0] >= 224
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (o[1] & 0xf0) == 16)
        || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113);
    !special
}

/// Global unicast (2000::/3) only, without the special-purpose blocks inside it. An IPv4-mapped
/// address is judged as the IPv4 address it maps.
pub fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    // 2001::/23 (IETF protocol assignments: Teredo, benchmarking, ORCHID...), 2001:db8::/32.
    if s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8) {
        return false;
    }
    // 6to4 relays reach IPv4 addresses of any kind.
    if s[0] == 0x2002 {
        return false;
    }
    // 3fff::/20, documentation (RFC 9637).
    if s[0] == 0x3fff && s[1] < 0x1000 {
        return false;
    }
    true
}

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

/// A public DNS name, lowercase, as the till accepts it: 2+ labels of `[a-z0-9-]`, not an IP
/// literal, a top-level label with a letter, and no local-use suffix.
pub fn check_domain(d: &str) -> Result<String, String> {
    let d = d.to_ascii_lowercase();
    if d.is_empty() || d.len() > 253 {
        return Err("the domain is empty or too long".into());
    }
    if d.parse::<IpAddr>().is_ok() || d.starts_with('[') {
        return Err("an IP address is not accepted; use a domain name".into());
    }
    let labels: Vec<&str> = d.split('.').collect();
    if labels.len() < 2 {
        return Err("the domain needs at least two labels".into());
    }
    for l in &labels {
        if l.is_empty()
            || l.len() > 63
            || l.starts_with('-')
            || l.ends_with('-')
            || !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(format!("{d} is not a valid domain name"));
        }
    }
    if !labels.last().expect("two labels").bytes().any(|b| b.is_ascii_alphabetic()) {
        return Err(format!("{d} is not a public domain name"));
    }
    if LOCAL_SUFFIXES.iter().any(|s| d == *s || d.ends_with(&format!(".{s}"))) {
        return Err(format!("{d} is a local-use name"));
    }
    Ok(d)
}

/// The host of a URL the till may fetch: HTTPS, port 443, no userinfo, a public domain name.
pub fn check_url(url: &Url) -> Result<String, FetchError> {
    if url.scheme() != "https" {
        return Err(FetchError::Unsafe("only HTTPS is accepted".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::Unsafe("a URL with credentials is not accepted".into()));
    }
    if !matches!(url.port(), None | Some(443)) {
        return Err(FetchError::Unsafe("only the standard HTTPS port is accepted".into()));
    }
    let host = url.host_str().ok_or_else(|| FetchError::Unsafe("the URL has no host".into()))?;
    check_domain(host).map_err(FetchError::Unsafe)
}

/// Every address `host` resolves to, each one public; an error if any is not.
pub fn check_addrs(host: &str, addrs: &[SocketAddr]) -> Result<(), FetchError> {
    if addrs.is_empty() {
        return Err(FetchError::Unreachable(format!("{host} does not resolve")));
    }
    if addrs.iter().any(|a| !is_public(a.ip())) {
        return Err(FetchError::Unsafe(format!("{host} resolves to a non-public address")));
    }
    Ok(())
}

/// The real fetch (see the module docs).
pub struct SafeFetch;

impl Fetch for SafeFetch {
    fn get_json(&self, url: Url) -> BoxFuture<'_, Result<Value, FetchError>> {
        Box::pin(safe_get(url))
    }
}

async fn safe_get(url: Url) -> Result<Value, FetchError> {
    let host = check_url(&url)?;
    let addrs: Vec<SocketAddr> = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host.as_str(), 443)))
        .await
        .map_err(|_| FetchError::Unreachable(format!("resolving {host} timed out")))?
        .map_err(|e| FetchError::Unreachable(format!("resolving {host}: {e}")))?
        .collect();
    check_addrs(&host, &addrs)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .https_only(true)
        .connect_timeout(DNS_TIMEOUT)
        .timeout(TIMEOUT)
        .resolve_to_addrs(&host, &addrs)
        .user_agent("fedimintcard-till")
        .build()
        .map_err(|e| FetchError::Unreachable(format!("{e}")))?;
    let mut resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| FetchError::Unreachable(format!("{host}: {}", e.without_url())))?;
    let status = resp.status();
    if status.is_redirection() {
        return Err(FetchError::Unsafe(format!("{host} answered a redirect, which is not followed")));
    }
    if !status.is_success() {
        return Err(FetchError::Refused(format!("{host} answered HTTP {}", status.as_u16())));
    }
    if resp.content_length().is_some_and(|l| l > MAX_BODY as u64) {
        return Err(FetchError::Refused(format!("{host} answered more than {MAX_BODY} bytes")));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| FetchError::Unreachable(format!("{host}: {}", e.without_url())))? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(FetchError::Refused(format!("{host} answered more than {MAX_BODY} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| FetchError::Refused(format!("{host} did not answer JSON")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_public_addresses_are_public() {
        for bad in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "2002:a00:1::1",
            "100::1",
            "3fff::1",
        ] {
            assert!(!is_public(ip(bad)), "{bad} is not public");
        }
        for good in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "100.128.0.1", "2606:4700::1111", "2a00:1450::1", "::ffff:1.1.1.1"] {
            assert!(is_public(ip(good)), "{good} is public");
        }
    }

    #[test]
    fn only_public_domain_names_are_accepted() {
        assert_eq!(check_domain("Example.COM").unwrap(), "example.com");
        assert!(check_domain("sub.wallet.example").is_ok());
        for bad in [
            "localhost",
            "127.0.0.1",
            "[::1]",
            "::1",
            "intranet",
            "a..b",
            "-a.com",
            "a-.com",
            "a.123",
            "x.local",
            "pay.internal",
            "abc.onion",
            "a.home.arpa",
            "x_y.com",
            "a.b/c",
            "",
        ] {
            assert!(check_domain(bad).is_err(), "{bad} is refused");
        }
    }

    #[test]
    fn urls_must_be_https_on_443_without_credentials() {
        let ok = Url::parse("https://pay.example.com/lnurlp/cb?x=1").unwrap();
        assert_eq!(check_url(&ok).unwrap(), "pay.example.com");
        for bad in [
            "http://example.com/x",
            "https://user:pw@example.com/x",
            "https://example.com:8443/x",
            "https://127.0.0.1/x",
            "https://0x7f.1/x",
            "https://[::1]/x",
            "https://localhost/x",
        ] {
            let u = Url::parse(bad).unwrap();
            assert!(matches!(check_url(&u), Err(FetchError::Unsafe(_))), "{bad} is refused");
        }
    }

    #[test]
    fn a_name_with_any_inside_address_is_refused() {
        let public: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let inside: SocketAddr = "10.0.0.5:443".parse().unwrap();
        assert!(check_addrs("x.example.com", &[public]).is_ok());
        assert!(matches!(check_addrs("x.example.com", &[public, inside]), Err(FetchError::Unsafe(_))));
        assert!(matches!(check_addrs("x.example.com", &[]), Err(FetchError::Unreachable(_))));
    }
}
