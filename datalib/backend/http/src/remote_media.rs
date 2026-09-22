//! Remote media (issue #648): the allow-list of what a person let a
//! document load, and the download CAS the bytes land in. The app page
//! may not reach a remote host itself (`embed::APP_CSP`), so a
//! reference the person chose to load comes through `GET
//! /api/remote_media?url=…`, which is `'self'`: the first time the URL
//! is fetched — no cookie, no referrer, no browser fingerprint — and
//! kept under `system/remote_media/<sha256>`; after that it is served
//! from there and the host never hears of it again. A target that
//! resolves to a loopback, private or link-local address is refused:
//! with the token the route is reachable by anything the page runs,
//! and it must not become a way to read from the machine's own network.
//!
//! The allow rows are the one policy, and it is decided here: nothing
//! is fetched or served that no row covers, and the page learns which
//! of a document's references are covered by asking
//! (`POST /api/remote_media/check`) rather than by reading the rows
//! itself. What a `document` or `source` row covers is what the caller
//! says it is loading for; the rows name the person's decisions, and
//! the token names the person.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use app_schema::remote_media::allow::RemoteMediaAllowRow;
use app_schema::remote_media::media::RemoteMediaRow;
use app_schema::remote_media::AllowScope;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::{IntoResponse, Json};
use datalib_columns::{ColumnSpec, ColumnType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::embed::{is_scriptable_document, DOCUMENT_SANDBOX_CSP};
use crate::AppState;

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
                tracing::warn!(
                    "{ALLOW_PRIVATE_ENV}=1: /api/remote_media may reach private addresses"
                );
            }
            RemotePolicy { allow_private }
        })
    }
}

/// Why a fetch did not produce media. Each maps to one status: a
/// malformed request is the caller's (400), a private target or a URL
/// no allow row covers is refused (403), and everything upstream did
/// or failed to do is a bad gateway (502) with the reason in the body.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    NotHttp(String),
    NoHost,
    NotAllowed(String),
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
            Refusal::PrivateAddress(_) | Refusal::NotAllowed(_) => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotHttp(u) => write!(f, "not an http(s) URL: {u}"),
            Refusal::NoHost => write!(f, "the URL names no host"),
            Refusal::NotAllowed(u) => write!(f, "refused: no rule lets {u} load"),
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

// ── The policy: which row lets a URL load ────────────────────────────

/// What the caller is loading for. A `document` row covers its
/// `markdown_uuid`, a `source` row the source's id.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Context {
    #[serde(default)]
    pub document: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// The host as a `host` row names it — the same spelling the page's
/// `URL.host` gives: the port only when it is written and not the
/// scheme's own.
pub fn host_key(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        (None, _) => String::new(),
    }
}

/// The row that lets `url` load in `ctx`, widest first — so the
/// answer names the broadest reason — or `None`.
pub fn covered<'a>(
    url: &Url,
    ctx: &Context,
    rows: &'a [RemoteMediaAllowRow],
) -> Option<&'a RemoteMediaAllowRow> {
    let scoped = |scope: AllowScope, key: Option<&str>| {
        let key = key?;
        rows.iter()
            .find(|r| AllowScope::parse(&r.scope) == Some(scope) && r.key == key)
    };
    let host = host_key(url);
    scoped(AllowScope::Source, ctx.source.as_deref())
        .or_else(|| scoped(AllowScope::Document, ctx.document.as_deref()))
        .or_else(|| {
            scoped(
                AllowScope::Host,
                Some(host.as_str()).filter(|h| !h.is_empty()),
            )
        })
        .or_else(|| scoped(AllowScope::Url, Some(url.as_str())))
}

#[derive(Deserialize)]
pub struct CheckRequest {
    #[serde(flatten)]
    pub context: Context,
    pub urls: Vec<String>,
}

#[derive(Serialize)]
pub struct Covered {
    /// The URL as the caller wrote it.
    pub url: String,
    pub rule: RemoteMediaAllowRow,
}

#[derive(Serialize)]
pub struct CheckResponse {
    /// Every URL asked about that a row covers, with the row. A URL not
    /// here is held.
    pub allowed: Vec<Covered>,
}

pub async fn check(State(s): State<AppState>, Json(req): Json<CheckRequest>) -> Response<Body> {
    let rows = match s.app.list_remote_allows().await {
        Ok(rows) => rows,
        Err(e) => return internal(e),
    };
    let allowed = req
        .urls
        .iter()
        .filter_map(|raw| {
            let url = target(raw).ok()?;
            let rule = covered(&url, &req.context, &rows)?;
            Some(Covered {
                url: raw.clone(),
                rule: rule.clone(),
            })
        })
        .collect();
    Json(CheckResponse { allowed }).into_response()
}

// ── The route: the CAS first, the host once ──────────────────────────

#[derive(Deserialize)]
pub struct MediaQuery {
    pub url: String,
    #[serde(flatten)]
    pub context: Context,
}

fn cas_path(root: &std::path::Path, sha256: &str) -> std::path::PathBuf {
    datalib_core::layout::remote_media_dir(root).join(sha256)
}

/// The bytes for a URL some row lets load in `ctx`: from the CAS when
/// it has been fetched before, else fetched, kept, and recorded.
async fn bytes_for(s: &AppState, q: &MediaQuery) -> Result<(String, Vec<u8>), Refusal> {
    let parsed = target(&q.url)?;
    let rows = s
        .app
        .list_remote_allows()
        .await
        .map_err(|e| Refusal::Transport(format!("read the allow-list: {e}")))?;
    if covered(&parsed, &q.context, &rows).is_none() {
        return Err(Refusal::NotAllowed(parsed.to_string()));
    }
    let url = parsed.to_string();
    if let Ok(Some(row)) = s.app.get_remote_media(&url).await {
        if let Ok(body) = tokio::fs::read(cas_path(&s.root, &row.sha256)).await {
            return Ok((row.content_type, body));
        }
        tracing::warn!(
            url,
            sha256 = row.sha256,
            "remote media: recorded but not in the CAS; fetching again"
        );
    }
    let fetched = fetch(&url, &RemotePolicy::current()).await?;
    let sha256 = hex(&Sha256::digest(&fetched.body));
    let dir = datalib_core::layout::remote_media_dir(&s.root);
    let io = |e: std::io::Error| Refusal::Transport(format!("keep in the CAS: {e}"));
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    // Written beside and renamed over, so a reader never sees half a file.
    let tmp = dir.join(format!(".{sha256}.{}", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, &fetched.body).await.map_err(io)?;
    tokio::fs::rename(&tmp, cas_path(&s.root, &sha256))
        .await
        .map_err(io)?;
    let (fetched_at_utc, tz_offset) =
        datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    s.app
        .record_remote_media(RemoteMediaRow {
            url: url.clone(),
            sha256,
            content_type: fetched.content_type.clone(),
            byte_size: fetched.body.len() as i64,
            fetched_at_utc,
            tz_offset: Some(tz_offset),
        })
        .await
        .map_err(|e| Refusal::Transport(format!("record the fetch: {e}")))?;
    Ok((fetched.content_type, fetched.body))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn response(content_type: &str, body: Vec<u8>) -> Response<Body> {
    let mut resp = Response::new(Body::from(body));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
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
    if is_scriptable_document(content_type) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(DOCUMENT_SANDBOX_CSP),
        );
    }
    resp
}

pub async fn get_media(State(s): State<AppState>, Query(q): Query<MediaQuery>) -> Response<Body> {
    match bytes_for(&s, &q).await {
        Ok((content_type, body)) => response(&content_type, body),
        Err(refusal) => {
            tracing::info!(url = %q.url, "remote media: {refusal}");
            (refusal.status(), refusal.to_string()).into_response()
        }
    }
}

// ── The allow-list, and both tables as data ──────────────────────────

/// A typed table (`docs/dev/cards.md` § "Typed tables") over one of
/// the store's tables, so `tableView({url})` shows it as it is.
#[derive(Debug, Serialize)]
pub struct TableResponse<T: Serialize> {
    pub columns: Vec<ColumnSpec>,
    pub row_key: &'static str,
    pub rows: Vec<T>,
}

fn allow_columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("scope", "Scope", ColumnType::Text).describe(
            "What the row lets load: one url, everything in one document, everything on one \
             host, or everything from one source.",
        ),
        ColumnSpec::new("key", "Key", ColumnType::Text)
            .describe("The URL, the document's markdown uuid, the host, or the source's id."),
        ColumnSpec::new("created_at_utc", "Allowed", ColumnType::Timestamp),
        ColumnSpec::new("allow_uuid", "Id", ColumnType::Text).hidden(),
    ]
}

fn media_columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("url", "URL", ColumnType::Text),
        ColumnSpec::new("content_type", "Type", ColumnType::Text),
        ColumnSpec::new("byte_size", "Size", ColumnType::Bytes),
        ColumnSpec::new("fetched_at_utc", "Fetched", ColumnType::Timestamp),
        ColumnSpec::new("sha256", "Content hash", ColumnType::Text)
            .describe("The file under system/remote_media/ holding the bytes.")
            .hidden(),
    ]
}

fn internal(e: impl std::fmt::Display) -> Response<Body> {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

pub async fn list_allows(State(s): State<AppState>) -> Response<Body> {
    match s.app.list_remote_allows().await {
        Ok(rows) => Json(TableResponse {
            columns: allow_columns(),
            row_key: "allow_uuid",
            rows,
        })
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct AllowRequest {
    pub scope: AllowScope,
    pub key: String,
}

pub async fn post_allow(
    State(s): State<AppState>,
    Json(req): Json<AllowRequest>,
) -> Response<Body> {
    let key = req.key.trim();
    if key.is_empty() {
        return (StatusCode::BAD_REQUEST, "an allow needs a key").into_response();
    }
    match s.app.allow_remote(req.scope, key).await {
        Ok(row) => (StatusCode::CREATED, Json::<RemoteMediaAllowRow>(row)).into_response(),
        Err(e) => internal(e),
    }
}

pub async fn delete_allow(
    State(s): State<AppState>,
    Path(allow_uuid): Path<String>,
) -> Response<Body> {
    match s.app.delete_remote_allow(&allow_uuid).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(e),
    }
}

pub async fn list_media(State(s): State<AppState>) -> Response<Body> {
    match s.app.list_remote_media().await {
        Ok(rows) => Json(TableResponse {
            columns: media_columns(),
            row_key: "url",
            rows,
        })
        .into_response(),
        Err(e) => internal(e),
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

    fn row(scope: AllowScope, key: &str) -> RemoteMediaAllowRow {
        RemoteMediaAllowRow {
            allow_uuid: format!("{}:{key}", scope.as_str()),
            scope: scope.as_str().to_string(),
            key: key.to_string(),
            created_at_utc: String::new(),
            tz_offset: None,
        }
    }

    /// The four scopes, widest first; a `document` or `source` row
    /// only for the context the caller names.
    #[test]
    fn a_row_covers_by_url_host_document_or_source() {
        let url = Url::parse("https://cdn.example/a.png").unwrap();
        let ctx = Context {
            document: Some("doc-1".into()),
            source: Some("mail".into()),
        };
        assert!(covered(&url, &ctx, &[]).is_none());
        let by = |rows: &[RemoteMediaAllowRow]| covered(&url, &ctx, rows).map(|r| r.scope.clone());
        assert_eq!(
            by(&[row(AllowScope::Url, url.as_str())]).as_deref(),
            Some("url")
        );
        assert_eq!(
            by(&[row(AllowScope::Host, "cdn.example")]).as_deref(),
            Some("host")
        );
        assert_eq!(
            by(&[row(AllowScope::Document, "doc-1")]).as_deref(),
            Some("document")
        );
        assert!(by(&[row(AllowScope::Document, "doc-2")]).is_none());
        assert_eq!(
            by(&[row(AllowScope::Source, "mail")]).as_deref(),
            Some("source")
        );
        assert!(covered(
            &url,
            &Context::default(),
            &[row(AllowScope::Source, "mail")]
        )
        .is_none());
        assert_eq!(
            by(&[
                row(AllowScope::Url, url.as_str()),
                row(AllowScope::Host, "cdn.example"),
                row(AllowScope::Source, "mail"),
            ])
            .as_deref(),
            Some("source")
        );
        // A row this build cannot read covers nothing.
        let mut unknown = row(AllowScope::Host, "cdn.example");
        unknown.scope = "planet".into();
        assert!(by(&[unknown]).is_none());
    }

    #[test]
    fn a_host_key_carries_only_a_written_non_default_port() {
        let key = |u: &str| host_key(&Url::parse(u).unwrap());
        assert_eq!(key("https://cdn.example/a.png"), "cdn.example");
        assert_eq!(key("https://cdn.example:443/a.png"), "cdn.example");
        assert_eq!(key("http://cdn.example:8080/a.png"), "cdn.example:8080");
        assert_eq!(key("https://CDN.Example/a.png"), "cdn.example");
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
