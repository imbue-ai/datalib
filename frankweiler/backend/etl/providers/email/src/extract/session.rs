//! JMAP session discovery + account selection.
//!
//! Loads `https://<hostname>/.well-known/jmap` (RFC 8620 §2.2), picks an
//! account by id (or falls back to `primaryAccounts['urn:ietf:params:jmap:mail']`),
//! and exposes the `apiUrl` / `downloadUrl` / `uploadUrl` templates the
//! transport layer interpolates into.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use frankweiler_etl::http::{latchkey_curl, HttpRequest};

/// JMAP capability URIs we list in every `using:` array. Mail is the
/// only capability we currently exercise; the core uri is required by
/// RFC 8620.
pub const CAP_CORE: &str = "urn:ietf:params:jmap:core";
pub const CAP_MAIL: &str = "urn:ietf:params:jmap:mail";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Raw `.well-known/jmap` response. Carried whole so future code can
    /// reach for capabilities we don't model yet.
    pub raw: Value,
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    pub event_source_url: Option<String>,
    /// `primaryAccounts['urn:ietf:params:jmap:mail']`.
    pub primary_mail_account: Option<String>,
    /// `accounts[<id>]` for every account the session reports — keyed
    /// by JMAP account id.
    pub accounts: Vec<(String, Value)>,
}

impl Session {
    /// Discover the session for `hostname` (e.g. `api.fastmail.com`).
    ///
    /// RFC 8620 §2.2 specifies `https://<hostname>/.well-known/jmap` as
    /// the discovery URL, and servers are allowed (encouraged, even) to
    /// 30x-redirect it to their real session endpoint —
    /// e.g. Fastmail redirects to `https://api.fastmail.com/jmap/session`.
    /// `latchkey_curl` issues `curl -sS` without `-L`, so we walk
    /// redirect hops here instead of expecting the transport to follow
    /// them silently.
    pub async fn discover(hostname: &str) -> Result<Self> {
        let mut url = format!("https://{hostname}/.well-known/jmap");
        // Bounded loop: real-world JMAP discovery is at most one hop;
        // cap at 5 to defend against a misconfigured server pointing at
        // itself.
        for _ in 0..5 {
            let req = HttpRequest::get("jmap", &url).timeout(Duration::from_secs(30));
            let resp = latchkey_curl(&req).await.map_err(|e| anyhow!("{e}"))?;
            if (300..400).contains(&resp.status) {
                let loc = resp.header("location").ok_or_else(|| {
                    anyhow!(
                        "JMAP discovery {url} returned {} with no Location header",
                        resp.status
                    )
                })?;
                url = resolve_redirect(&url, loc)?;
                continue;
            }
            if !(200..300).contains(&resp.status) {
                return Err(anyhow!(
                    "JMAP session discovery {url} returned HTTP {}: {}",
                    resp.status,
                    resp.body_str(),
                ));
            }
            let raw: Value =
                serde_json::from_slice(&resp.body).context("parse JMAP session JSON")?;
            return Self::from_value(raw);
        }
        Err(anyhow!("JMAP session discovery: too many redirects starting at https://{hostname}/.well-known/jmap"))
    }

    pub fn from_value(raw: Value) -> Result<Self> {
        let api_url = raw
            .get("apiUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("session missing apiUrl"))?
            .to_string();
        let download_url = raw
            .get("downloadUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("session missing downloadUrl"))?
            .to_string();
        let upload_url = raw
            .get("uploadUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let event_source_url = raw
            .get("eventSourceUrl")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let primary_mail_account = raw
            .get("primaryAccounts")
            .and_then(|v| v.get(CAP_MAIL))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let accounts = raw
            .get("accounts")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(Self {
            raw,
            api_url,
            download_url,
            upload_url,
            event_source_url,
            primary_mail_account,
            accounts,
        })
    }

    /// Pick an account id: explicit override if non-empty, else the
    /// session's primary mail account. Errors if neither is available.
    pub fn pick_account(&self, override_id: Option<&str>) -> Result<String> {
        if let Some(id) = override_id {
            if !id.is_empty() {
                if !self.accounts.iter().any(|(k, _)| k == id) {
                    return Err(anyhow!(
                        "configured account_id {id:?} not present in JMAP session"
                    ));
                }
                return Ok(id.to_string());
            }
        }
        self.primary_mail_account.clone().ok_or_else(|| {
            anyhow!("JMAP session has no primaryAccounts[mail] and no override given")
        })
    }

    /// Interpolate `{accountId}` / `{blobId}` / `{name}` / `{type}` into
    /// `downloadUrl`. Empty `name` / `type` are fine — the server uses
    /// them only to set response headers.
    pub fn download_url_for(
        &self,
        account_id: &str,
        blob_id: &str,
        name: &str,
        content_type: &str,
    ) -> String {
        self.download_url
            .replace("{accountId}", account_id)
            .replace("{blobId}", blob_id)
            .replace("{name}", name)
            .replace("{type}", content_type)
    }
}

/// Resolve a `Location` header (absolute, scheme-relative, or path) against
/// the URL that returned it. Minimal: we only need the cases real JMAP
/// servers produce — absolute URLs and path-relative redirects on the
/// same origin. Query strings get dropped on path-only Locations, which
/// matches `curl -L`'s behavior.
fn resolve_redirect(base: &str, location: &str) -> Result<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    // Strip scheme://host[:port] from base.
    let after_scheme = base
        .split_once("://")
        .ok_or_else(|| anyhow!("base url {base} missing scheme"))?
        .1;
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let origin = &base[..base.len() - after_scheme.len() + host_end];
    if location.starts_with("//") {
        // Scheme-relative.
        let scheme = base.split_once("://").unwrap().0;
        return Ok(format!("{scheme}:{location}"));
    }
    if location.starts_with('/') {
        return Ok(format!("{origin}{location}"));
    }
    // Path-relative: rare in practice, but fall back to origin + raw.
    Ok(format!("{origin}/{location}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minimal_session() {
        let s = Session::from_value(json!({
            "apiUrl": "https://api.fastmail.com/jmap/api/",
            "downloadUrl": "https://api.fastmail.com/jmap/download/{accountId}/{blobId}/{name}?type={type}",
            "uploadUrl": "https://api.fastmail.com/jmap/upload/{accountId}/",
            "eventSourceUrl": "https://api.fastmail.com/jmap/event/",
            "primaryAccounts": {"urn:ietf:params:jmap:mail": "A1"},
            "accounts": {"A1": {"name": "thad@fastmail.com", "isPersonal": true}},
        }))
        .unwrap();
        assert_eq!(s.api_url, "https://api.fastmail.com/jmap/api/");
        assert_eq!(s.primary_mail_account.as_deref(), Some("A1"));
        assert_eq!(s.pick_account(None).unwrap(), "A1");
        assert_eq!(s.pick_account(Some("A1")).unwrap(), "A1");
        assert!(s.pick_account(Some("A2")).is_err());
    }

    #[test]
    fn resolve_redirect_absolute() {
        assert_eq!(
            resolve_redirect(
                "https://api.fastmail.com/.well-known/jmap",
                "https://api.fastmail.com/jmap/session",
            )
            .unwrap(),
            "https://api.fastmail.com/jmap/session",
        );
    }

    #[test]
    fn resolve_redirect_path_relative() {
        assert_eq!(
            resolve_redirect("https://api.fastmail.com/.well-known/jmap", "/jmap/session",)
                .unwrap(),
            "https://api.fastmail.com/jmap/session",
        );
    }

    #[test]
    fn resolve_redirect_scheme_relative() {
        assert_eq!(
            resolve_redirect(
                "https://api.fastmail.com/.well-known/jmap",
                "//api.fastmail.com/jmap/session",
            )
            .unwrap(),
            "https://api.fastmail.com/jmap/session",
        );
    }

    #[test]
    fn download_url_interpolates_all_placeholders() {
        let s = Session::from_value(json!({
            "apiUrl": "x",
            "downloadUrl": "https://h/{accountId}/{blobId}/{name}?type={type}",
        }))
        .unwrap();
        assert_eq!(
            s.download_url_for("A1", "B1", "doc.pdf", "application/pdf"),
            "https://h/A1/B1/doc.pdf?type=application/pdf",
        );
    }
}
