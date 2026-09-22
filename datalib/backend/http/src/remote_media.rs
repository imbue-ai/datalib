//! `GET /api/remote?url=…`: fetches one remote image or media file on
//! the page's behalf. The app page may not reach a remote host itself
//! (`embed::APP_CSP`), so a reference the person chose to load comes
//! through here, which is `'self'`. The fetch carries no cookie, no
//! referrer and no browser fingerprint. A target that resolves to a
//! loopback, private or link-local address is refused: with the token
//! the route is reachable by anything the page runs, and it must not
//! become a way to read from the machine's own network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Query;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use url::{Host, Url};

use crate::embed::{is_scriptable_document, DOCUMENT_SANDBOX_CSP};

/// Set to `1` to let the route reach loopback and private addresses —
/// for a test whose stand-in remote host is on this machine. Logged
/// when it is on, because it removes the one guard this route has.
pub const ALLOW_PRIVATE_ENV: &str = "DATALIB_REMOTE_MEDIA_ALLOW_PRIVATE";

/// Larger than any image an email or a chat embeds; a body past this
/// is not media but a mistake or an attack on the server's memory.
const MAX_BYTES: usize = 50 << 20;
const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Default)]
pub struct RemotePolicy {
    pub allow_private: bool,
}

impl RemotePolicy {
    /// This process's policy, read from the environment once.
    pub fn current() -> Self {
        static POLICY: OnceLock<RemotePolicy> = OnceLock::new();
        *POLICY.get_or_init(|| {
            let allow_private = std::env::var(ALLOW_PRIVATE_ENV).is_ok_and(|v| v == "1");
            if allow_private {
                tracing::warn!("{ALLOW_PRIVATE_ENV}=1: /api/remote may reach private addresses");
            }
            RemotePolicy { allow_private }
        })
    }
}

/// Why a fetch did not produce media. Each maps to one status: a
/// malformed request is the caller's (400), a private target is refused
/// (403), and everything upstream did or failed to do is a bad gateway
/// (502) with the reason in the body.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    NotHttp(String),
    NoHost,
    PrivateAddress(IpAddr),
    Unresolvable(String),
    TooManyRedirects,
    UpstreamStatus(u16),
    NotMedia(String),
    TooLarge,
    Transport(String),
}

impl Refusal {
    pub fn status(&self) -> StatusCode {
        match self {
            Refusal::NotHttp(_) | Refusal::NoHost => StatusCode::BAD_REQUEST,
            Refusal::PrivateAddress(_) => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotHttp(u) => write!(f, "not an http(s) URL: {u}"),
            Refusal::NoHost => write!(f, "the URL names no host"),
            Refusal::PrivateAddress(ip) => {
                write!(f, "refused: the host resolves to a private address ({ip})")
            }
            Refusal::Unresolvable(h) => write!(f, "could not resolve {h}"),
            Refusal::TooManyRedirects => write!(f, "more than {MAX_REDIRECTS} redirects"),
            Refusal::UpstreamStatus(s) => write!(f, "the remote host answered {s}"),
            Refusal::NotMedia(ct) => write!(f, "not an image or media file: {ct}"),
            Refusal::TooLarge => write!(f, "larger than {} MiB", MAX_BYTES >> 20),
            Refusal::Transport(e) => write!(f, "fetch failed: {e}"),
        }
    }
}

pub struct Fetched {
    /// `type/subtype` only, lowercased; the parameters upstream sent
    /// are not forwarded.
    pub content_type: String,
    pub body: Vec<u8>,
}

/// An address the route may fetch from: routable on the public
/// internet. Everything the machine or its LAN answers on is refused —
/// loopback, RFC 1918 and the carrier-grade range, link-local (which is
/// where cloud metadata services live), and the v6 equivalents, with a
/// v4-mapped v6 address judged as the v4 it wraps.
pub fn is_public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // 100.64.0.0/10, carrier-grade NAT: a LAN as far as this is concerned.
        || (a == 100 && (64..=127).contains(&b))
        // 0.0.0.0/8 "this network", and 240.0.0.0/4 reserved.
        || a == 0
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // fc00::/7 unique local, fe80::/10 link local.
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        // 2001:db8::/32 documentation.
        || (first == 0x2001 && ip.segments()[1] == 0x0db8))
}

/// The URL as something this route will fetch: http or https, with a
/// host. Nothing about the address is decided here; that needs a
/// lookup.
pub fn target(raw: &str) -> Result<Url, Refusal> {
    let url = Url::parse(raw).map_err(|_| Refusal::NotHttp(raw.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refusal::NotHttp(raw.to_string()));
    }
    if url.host().is_none() {
        return Err(Refusal::NoHost);
    }
    Ok(url)
}

fn media_type(content_type: &str) -> Result<String, Refusal> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime.starts_with("image/") || mime.starts_with("video/") || mime.starts_with("audio/") {
        Ok(mime)
    } else {
        Err(Refusal::NotMedia(content_type.to_string()))
    }
}

/// Every address the host resolves to, each checked against the policy.
/// A name answering with one public and one private address is refused
/// as a whole: the request is then pinned to exactly these addresses,
/// so a second lookup at connect time cannot answer differently.
async fn resolve(url: &Url, policy: &RemotePolicy) -> Result<Vec<SocketAddr>, Refusal> {
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = match url.host().ok_or(Refusal::NoHost)? {
        Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        Host::Domain(name) => tokio::net::lookup_host((name, port))
            .await
            .map_err(|e| Refusal::Unresolvable(format!("{name}: {e}")))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(Refusal::Unresolvable(
            url.host_str().unwrap_or("").to_string(),
        ));
    }
    if !policy.allow_private {
        if let Some(bad) = addrs.iter().find(|a| !is_public_address(a.ip())) {
            return Err(Refusal::PrivateAddress(bad.ip()));
        }
    }
    Ok(addrs)
}

pub async fn fetch(raw: &str, policy: &RemotePolicy) -> Result<Fetched, Refusal> {
    let mut url = target(raw)?;
    for _ in 0..=MAX_REDIRECTS {
        let addrs = resolve(&url, policy).await?;
        let host = url.host_str().ok_or(Refusal::NoHost)?.to_string();
        // One client per hop, because a redirect to another host has to
        // be resolved and judged again before anything connects to it.
        let client = reqwest::Client::builder()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .referer(false)
            .user_agent(concat!("datalib/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Refusal::Transport(e.to_string()))?;
        let mut resp = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| Refusal::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_redirection() {
            let next = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(Refusal::UpstreamStatus(status.as_u16()))?;
            url = url
                .join(next)
                .map_err(|_| Refusal::NotHttp(next.to_string()))?;
            url = target(url.as_str())?;
            continue;
        }
        if !status.is_success() {
            return Err(Refusal::UpstreamStatus(status.as_u16()));
        }
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let content_type = media_type(&content_type)?;
        if resp.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
            return Err(Refusal::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| Refusal::Transport(e.to_string()))?
        {
            if body.len() + chunk.len() > MAX_BYTES {
                return Err(Refusal::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        return Ok(Fetched { content_type, body });
    }
    Err(Refusal::TooManyRedirects)
}

#[derive(Deserialize)]
pub struct RemoteQuery {
    pub url: String,
}

pub fn response(fetched: Fetched) -> Response<Body> {
    let mut resp = Response::new(Body::from(fetched.body));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&fetched.content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // An SVG is an image in an `<img>` and a document with scripts when
    // navigated to; the sandbox keeps the second case out of this origin.
    if is_scriptable_document(&fetched.content_type) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(DOCUMENT_SANDBOX_CSP),
        );
    }
    resp
}

pub async fn get(Query(q): Query<RemoteQuery>) -> Response<Body> {
    let policy = RemotePolicy::current();
    match fetch(&q.url, &policy).await {
        Ok(fetched) => response(fetched),
        Err(refusal) => {
            tracing::info!(url = %q.url, "remote media: {refusal}");
            (refusal.status(), refusal.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_the_machine_or_its_lan_answer_on_are_not_public() {
        for ip in [
            "127.0.0.1",
            "127.8.8.8",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_public_address(ip), "{ip} should be refused");
        }
        for ip in [
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_public_address(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn only_http_urls_with_a_host_are_targets() {
        assert!(target("https://example.com/a.png").is_ok());
        assert!(target("http://example.com:8080/a.png").is_ok());
        assert_eq!(
            target("ftp://example.com/a.png"),
            Err(Refusal::NotHttp("ftp://example.com/a.png".into()))
        );
        assert_eq!(
            target("data:image/png;base64,AAAA"),
            Err(Refusal::NotHttp("data:image/png;base64,AAAA".into()))
        );
        assert_eq!(
            target("file:///etc/passwd"),
            Err(Refusal::NotHttp("file:///etc/passwd".into()))
        );
        assert_eq!(
            target("blobs/x.png"),
            Err(Refusal::NotHttp("blobs/x.png".into()))
        );
    }

    #[test]
    fn media_types_are_image_video_or_audio_without_parameters() {
        assert_eq!(media_type("image/png").unwrap(), "image/png");
        assert_eq!(
            media_type("IMAGE/JPEG; charset=binary").unwrap(),
            "image/jpeg"
        );
        assert_eq!(media_type("video/mp4").unwrap(), "video/mp4");
        assert_eq!(media_type("audio/ogg").unwrap(), "audio/ogg");
        assert_eq!(
            media_type("text/html; charset=utf-8"),
            Err(Refusal::NotMedia("text/html; charset=utf-8".into()))
        );
        assert_eq!(
            media_type("application/octet-stream"),
            Err(Refusal::NotMedia("application/octet-stream".into()))
        );
        assert_eq!(media_type(""), Err(Refusal::NotMedia(String::new())));
    }

    #[test]
    fn each_refusal_has_the_status_its_kind_deserves() {
        assert_eq!(
            Refusal::NotHttp(String::new()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            Refusal::PrivateAddress("127.0.0.1".parse().unwrap()).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            Refusal::UpstreamStatus(404).status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            Refusal::NotMedia(String::new()).status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn a_loopback_target_is_refused_before_anything_connects() {
        // Port 1 answers nothing; a refusal that came from connecting
        // would be a Transport error, not this.
        let err = fetch("http://127.0.0.1:1/a.png", &RemotePolicy::default())
            .await
            .err()
            .unwrap();
        assert_eq!(err, Refusal::PrivateAddress("127.0.0.1".parse().unwrap()));
    }
}
