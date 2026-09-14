//! Garmin Connect credentials: the year-long OAuth1 token a login
//! produces, the hour-long OAuth2 bearer every API call carries, and
//! the OAuth1-signed exchange that turns the first into the second.
//!
//! Files are in garth's format (`oauth1_token.json`, `oauth2_token.json`
//! under one directory), so a token minted by `garth login` and one
//! minted by `datalib-step login garmin` are interchangeable.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;

/// The Connect mobile app's OAuth1 consumer, published by the garth
/// project at `https://thegarth.s3.amazonaws.com/oauth_consumer.json`.
/// Pinned here rather than fetched: it has not changed since garth began
/// mirroring it, and a fetch on every token refresh would make an S3
/// bucket a dependency of every sync.
pub const CONSUMER_KEY: &str = "fc3e99d2-118c-44b8-8ae3-03370dde24c0";
pub const CONSUMER_SECRET: &str = "E08WAR897WEy2knn7aFBrvegVAf0AFdWBBF";

pub const OAUTH1_FILE: &str = "oauth1_token.json";
pub const OAUTH2_FILE: &str = "oauth2_token.json";
pub const DEFAULT_TOKEN_DIR: &str = "~/.garth";

/// The user agent the OAuth endpoints expect to see beside this consumer.
pub const OAUTH_USER_AGENT: &str = "com.garmin.android.apps.connectmobile";

/// A bearer is treated as expired this long before Garmin says so, so a
/// request issued right at the boundary does not go out with a token
/// that dies in flight.
const EXPIRY_MARGIN_SECS: i64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuth1Token {
    pub oauth_token: String,
    pub oauth_token_secret: String,
    #[serde(default)]
    pub mfa_token: Option<String>,
    #[serde(default)]
    pub mfa_expiration_timestamp: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
}

impl OAuth1Token {
    pub fn domain(&self) -> &str {
        self.domain.as_deref().unwrap_or("garmin.com")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuth2Token {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub jti: String,
    #[serde(default)]
    pub token_type: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    pub expires_in: i64,
    pub expires_at: i64,
    #[serde(default)]
    pub refresh_token_expires_in: i64,
    #[serde(default)]
    pub refresh_token_expires_at: i64,
}

impl OAuth2Token {
    pub fn is_expired(&self, now_unix: i64) -> bool {
        self.expires_at - EXPIRY_MARGIN_SECS <= now_unix
    }
}

pub fn expand_token_dir(configured: Option<&str>) -> PathBuf {
    let raw = configured.unwrap_or(DEFAULT_TOKEN_DIR);
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

pub fn load_oauth1(dir: &Path) -> Result<OAuth1Token> {
    let path = dir.join(OAUTH1_FILE);
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "no Garmin OAuth1 token at {}; run `datalib-step login garmin` (or `garth login`) first",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn load_oauth2(dir: &Path) -> Option<OAuth2Token> {
    let bytes = std::fs::read(dir.join(OAUTH2_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_tokens(dir: &Path, oauth1: Option<&OAuth1Token>, oauth2: &OAuth2Token) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    if let Some(t) = oauth1 {
        write_private(&dir.join(OAUTH1_FILE), &serde_json::to_vec_pretty(t)?)?;
    }
    write_private(&dir.join(OAUTH2_FILE), &serde_json::to_vec_pretty(oauth2)?)?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

/// A live credential: hands out a bearer, refreshing it through the
/// OAuth1 exchange when the cached one has expired, and writes the
/// refreshed bearer back beside the OAuth1 token so the next process
/// starts warm.
#[derive(Debug, Clone)]
pub struct Credentials {
    dir: Option<PathBuf>,
    oauth1: Option<OAuth1Token>,
    oauth2: Option<OAuth2Token>,
}

impl Credentials {
    pub fn load(dir: &Path) -> Result<Self> {
        let oauth1 = load_oauth1(dir)?;
        Ok(Self {
            dir: Some(dir.to_path_buf()),
            oauth1: Some(oauth1),
            oauth2: load_oauth2(dir),
        })
    }

    /// A fixed bearer that is never refreshed — for playback, where no
    /// request reaches Garmin and the token's value is never inspected.
    pub fn fixed(bearer: &str) -> Self {
        Self {
            dir: None,
            oauth1: None,
            oauth2: Some(OAuth2Token {
                scope: String::new(),
                jti: String::new(),
                token_type: "bearer".into(),
                access_token: bearer.to_string(),
                refresh_token: String::new(),
                expires_in: i64::MAX / 4,
                expires_at: i64::MAX / 4,
                refresh_token_expires_in: 0,
                refresh_token_expires_at: 0,
            }),
        }
    }

    pub fn domain(&self) -> &str {
        self.oauth1
            .as_ref()
            .map(OAuth1Token::domain)
            .unwrap_or("garmin.com")
    }

    /// Forget the cached bearer so the next [`Self::bearer`] exchanges
    /// for a new one. A no-op on a fixed credential.
    pub fn force_refresh(&mut self) {
        if self.oauth1.is_some() {
            self.oauth2 = None;
        }
    }

    pub async fn bearer(&mut self) -> Result<String> {
        let now = unix_now();
        if let Some(t) = &self.oauth2 {
            if !t.is_expired(now) {
                return Ok(t.access_token.clone());
            }
        }
        let oauth1 = self
            .oauth1
            .as_ref()
            .ok_or_else(|| anyhow!("the fixed Garmin bearer expired, which cannot happen"))?;
        let fresh = exchange(oauth1, false).await?;
        if let Some(dir) = &self.dir {
            write_tokens(dir, None, &fresh)?;
        }
        let bearer = fresh.access_token.clone();
        self.oauth2 = Some(fresh);
        Ok(bearer)
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `POST /oauth-service/oauth/exchange/user/2.0`, signed with the OAuth1
/// token: the one call that mints a bearer. `login` is true only on the
/// call that follows a fresh SSO login, where the mobile app names its
/// audience; a routine refresh sends nothing but the MFA token.
pub async fn exchange(oauth1: &OAuth1Token, login: bool) -> Result<OAuth2Token> {
    let url = format!(
        "https://connectapi.{}/oauth-service/oauth/exchange/user/2.0",
        oauth1.domain()
    );
    let mut form: Vec<(String, String)> = Vec::new();
    if login {
        form.push(("audience".into(), "GARMIN_CONNECT_MOBILE_ANDROID_DI".into()));
    }
    if let Some(mfa) = &oauth1.mfa_token {
        form.push(("mfa_token".into(), mfa.clone()));
    }
    let auth = oauth1_header(
        "POST",
        &url,
        &form,
        Some((&oauth1.oauth_token, &oauth1.oauth_token_secret)),
        &Nonce::fresh(),
    );
    let body = form_encode(&form);
    let resp = curl(&[
        "-sS",
        "-X",
        "POST",
        "-H",
        &format!("User-Agent: {OAUTH_USER_AGENT}"),
        "-H",
        &format!("Authorization: {auth}"),
        "-H",
        "Content-Type: application/x-www-form-urlencoded",
        "--data-binary",
        &body,
        &url,
    ])
    .await
    .context("Garmin OAuth2 exchange")?;
    if resp.status != 200 {
        bail!(
            "Garmin OAuth2 exchange -> HTTP {}: {}. The OAuth1 token has probably expired \
             (they last about a year); run `datalib-step login garmin` again",
            resp.status,
            resp.body.chars().take(300).collect::<String>()
        );
    }
    let raw: serde_json::Value =
        serde_json::from_str(&resp.body).context("Garmin OAuth2 exchange: not JSON")?;
    let now = unix_now();
    let expires_in = raw["expires_in"].as_i64().unwrap_or(3600);
    let refresh_in = raw["refresh_token_expires_in"].as_i64().unwrap_or(0);
    Ok(OAuth2Token {
        scope: raw["scope"].as_str().unwrap_or_default().to_string(),
        jti: raw["jti"].as_str().unwrap_or_default().to_string(),
        token_type: raw["token_type"].as_str().unwrap_or("bearer").to_string(),
        access_token: raw["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("Garmin OAuth2 exchange: no access_token in reply"))?
            .to_string(),
        refresh_token: raw["refresh_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        expires_in,
        expires_at: now + expires_in,
        refresh_token_expires_in: refresh_in,
        refresh_token_expires_at: now + refresh_in,
    })
}

// ── OAuth 1.0a (RFC 5849) HMAC-SHA1 signing ───────────────────────────

/// The two per-request values a signature folds in. A struct so a test
/// can pin them and check the signature against a known-good one.
pub struct Nonce {
    pub nonce: String,
    pub timestamp: i64,
}

impl Nonce {
    pub fn fresh() -> Self {
        Self {
            nonce: uuid::Uuid::new_v4().simple().to_string(),
            timestamp: unix_now(),
        }
    }
}

/// RFC 3986 §2.3 percent-encoding: everything but `A-Z a-z 0-9 - . _ ~`.
pub fn pct(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn form_encode(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build the `Authorization: OAuth …` header for one request.
///
/// `url` may carry a query string; its parameters join `form` in the
/// signature base, as the spec requires. `token` is `(key, secret)` for
/// a token-bearing request and `None` for the pre-authorization call
/// that has only the consumer.
pub fn oauth1_header(
    method: &str,
    url: &str,
    form: &[(String, String)],
    token: Option<(&str, &str)>,
    nonce: &Nonce,
) -> String {
    let (base_url, query) = match url.split_once('?') {
        Some((b, q)) => (b.to_string(), q.to_string()),
        None => (url.to_string(), String::new()),
    };
    let mut oauth: Vec<(String, String)> = vec![
        ("oauth_consumer_key".into(), CONSUMER_KEY.into()),
        ("oauth_nonce".into(), nonce.nonce.clone()),
        ("oauth_signature_method".into(), "HMAC-SHA1".into()),
        ("oauth_timestamp".into(), nonce.timestamp.to_string()),
        ("oauth_version".into(), "1.0".into()),
    ];
    if let Some((key, _)) = token {
        oauth.push(("oauth_token".into(), key.to_string()));
    }

    let mut all: Vec<(String, String)> = oauth.clone();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        all.push((
            urlencoding::decode(k)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| k.to_string()),
            urlencoding::decode(v)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| v.to_string()),
        ));
    }
    all.extend(form.iter().cloned());
    let mut encoded: Vec<(String, String)> = all.iter().map(|(k, v)| (pct(k), pct(v))).collect();
    encoded.sort();
    let normalized = encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let base = format!(
        "{}&{}&{}",
        method.to_ascii_uppercase(),
        pct(&base_url),
        pct(&normalized)
    );
    let key = format!(
        "{}&{}",
        pct(CONSUMER_SECRET),
        pct(token.map(|(_, s)| s).unwrap_or(""))
    );
    let mut mac =
        <Hmac<Sha1> as KeyInit>::new_from_slice(key.as_bytes()).expect("hmac accepts any key");
    mac.update(base.as_bytes());
    let sig = base64_encode(&mac.finalize().into_bytes());
    oauth.push(("oauth_signature".into(), sig));

    let fields = oauth
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("OAuth {fields}")
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ── a curl small enough for the two auth endpoints ────────────────────

pub struct CurlResponse {
    pub status: u16,
    pub body: String,
}

/// Plain `curl`, not the latchkey shim: these calls carry their own
/// `Authorization` and, for the SSO login, a cookie jar — neither of
/// which the shared transport models.
pub async fn curl(args: &[&str]) -> Result<CurlResponse> {
    let out = tokio::process::Command::new("curl")
        .args(args)
        .arg("-w")
        .arg("\n%{http_code}")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn curl")?
        .wait_with_output()
        .await?;
    if !out.status.success() {
        bail!(
            "curl exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    Ok(CurlResponse {
        status: code.trim().parse().unwrap_or(0),
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_is_rfc3986() {
        assert_eq!(pct("a b/c~d-e_f.g"), "a%20b%2Fc~d-e_f.g");
        assert_eq!(
            pct("https://x.y/?a=1&b=2"),
            "https%3A%2F%2Fx.y%2F%3Fa%3D1%26b%3D2"
        );
    }

    fn signature_of(header: &str) -> &str {
        header
            .split("oauth_signature=\"")
            .nth(1)
            .and_then(|s| s.strip_suffix('"'))
            .expect("a signature field")
    }

    /// Known answers computed with python-oauthlib (the library garth
    /// signs with) against the same inputs. The second is the RFC 5849
    /// §3.4.1.3.1 example with our consumer, so the query-string and
    /// form-body handling is pinned too, not just the HMAC.
    #[test]
    fn signatures_match_oauthlib() {
        let header = oauth1_header(
            "POST",
            "https://connectapi.garmin.com/oauth-service/oauth/exchange/user/2.0",
            &[("mfa_token".into(), "m".into())],
            Some(("tok", "sec")),
            &Nonce {
                nonce: "abc123".into(),
                timestamp: 1_700_000_000,
            },
        );
        assert_eq!(signature_of(&header), "NLxYWkOOxBCIv%2FSQ3CG3bicL9ug%3D");
        assert!(
            header.starts_with("OAuth oauth_consumer_key=\""),
            "{header}"
        );
        assert!(header.contains("oauth_token=\"tok\""), "{header}");

        let header = oauth1_header(
            "POST",
            "http://example.com/request?b5=%3D%253D&a3=a&c%40=&a2=r%20b",
            &[("c2".into(), "".into()), ("a3".into(), "2 q".into())],
            Some(("kkk9d7dh3k39sjv7", "secret")),
            &Nonce {
                nonce: "7d8f3e4a".into(),
                timestamp: 137131201,
            },
        );
        assert_eq!(signature_of(&header), "7PAEbk9wufiGO1GzlKjQJavK2MQ%3D");
    }

    #[test]
    fn token_files_round_trip_in_garth_format() {
        let dir = tempfile::tempdir().unwrap();
        let o1 = OAuth1Token {
            oauth_token: "t".into(),
            oauth_token_secret: "s".into(),
            mfa_token: Some("m".into()),
            mfa_expiration_timestamp: Some("2027-03-18T00:00:00".into()),
            domain: Some("garmin.com".into()),
        };
        let o2 = OAuth2Token {
            scope: "CONNECT_READ".into(),
            jti: "j".into(),
            token_type: "bearer".into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: 3600,
            expires_at: 1_700_003_600,
            refresh_token_expires_in: 2_591_999,
            refresh_token_expires_at: 1_702_591_999,
        };
        write_tokens(dir.path(), Some(&o1), &o2).unwrap();
        assert_eq!(load_oauth1(dir.path()).unwrap(), o1);
        assert_eq!(load_oauth2(dir.path()).unwrap(), o2);
        // garth's own file: no expires fields we add beyond its own, and
        // nulls where it has none.
        std::fs::write(
            dir.path().join(OAUTH1_FILE),
            r#"{"oauth_token": "t", "oauth_token_secret": "s", "mfa_token": null, "mfa_expiration_timestamp": null, "domain": "garmin.com"}"#,
        )
        .unwrap();
        let loaded = load_oauth1(dir.path()).unwrap();
        assert_eq!(loaded.mfa_token, None);
    }

    #[test]
    fn expiry_has_a_margin() {
        let t = OAuth2Token {
            scope: String::new(),
            jti: String::new(),
            token_type: String::new(),
            access_token: "a".into(),
            refresh_token: String::new(),
            expires_in: 3600,
            expires_at: 10_000,
            refresh_token_expires_in: 0,
            refresh_token_expires_at: 0,
        };
        assert!(!t.is_expired(9_000));
        assert!(t.is_expired(10_000 - EXPIRY_MARGIN_SECS));
        assert!(t.is_expired(10_001));
    }
}
