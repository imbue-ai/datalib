//! Claude (claude.ai) API transport. Every request goes through
//! [`datalib_etl_web::http::latchkey_curl`], which captures the full
//! response (status + every header + body) and supports playback from
//! disk fixtures. Mirrors `src/ingest/claude_web.py:_get`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tracing::{info_span, instrument, Instrument};

use datalib_etl::upstream::Upstream;
use datalib_etl_web::http::{
    latchkey_curl, percent_encode, HttpError, HttpRequest, HttpService, LatchkeySettings,
};

pub const BASE: &str = "https://claude.ai/api";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);
pub const CLAUDE_ORIGIN: &str = "https://claude.ai";

/// Where claude.ai's own client reads Cowork tasks: the sessions API it
/// shares with Claude Code on the web, under the same cookie.
pub const SESSIONS_BASE: &str = "https://claude.ai/v1/code/sessions";
/// The listing shows a Cowork task only when asked for its tag.
pub const COWORK_TAG: &str = "cowork-remote";
const SESSIONS_PAGE: usize = 100;
const EVENTS_PAGE: usize = 500;

/// One page of an org's Cowork tasks.
pub fn cowork_sessions_url(cursor: Option<&str>) -> String {
    let mut url = format!("{SESSIONS_BASE}?tags={COWORK_TAG}&limit={SESSIONS_PAGE}");
    if let Some(c) = cursor {
        url.push_str(&format!("&cursor={}", percent_encode(c)));
    }
    url
}

/// One page of a Cowork task's log, oldest first.
pub fn cowork_events_url(session_id: &str, cursor: Option<&str>) -> String {
    let mut url = format!(
        "{SESSIONS_BASE}/{}/events?limit={EVENTS_PAGE}&sort_order=asc",
        percent_encode(session_id)
    );
    if let Some(c) = cursor {
        url.push_str(&format!("&cursor={}", percent_encode(c)));
    }
    url
}

/// The sessions API answers only with its version, its beta and the org
/// named in a header; the cookie alone is refused.
pub fn sessions_request(url: &str, org_uuid: &str) -> HttpRequest {
    HttpRequest::get(HttpService::Claude, url)
        .header("Accept", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "ccr-byoc-2025-07-29")
        .header("x-organization-uuid", org_uuid)
}

#[derive(thiserror::Error, Debug)]
pub enum ClaudeError {
    #[error("forbidden: {0}")]
    Forbidden(String, #[source] Upstream),
    /// The shared retry loop's give-up policy tripped on a rate limit or
    /// an outage; every request after it is refused the same way, so the
    /// caller stops.
    #[error("gave up retrying: {0}")]
    RateLimited(String, #[source] Option<Upstream>),
    #[error("{0}")]
    Permanent(String, #[source] Option<Upstream>),
}

impl ClaudeError {
    pub fn upstream(&self) -> Option<Upstream> {
        match self {
            ClaudeError::Forbidden(_, why) => Some(*why),
            ClaudeError::RateLimited(_, why) | ClaudeError::Permanent(_, why) => *why,
        }
    }

    fn permanent(message: String) -> Self {
        ClaudeError::Permanent(message, None)
    }
}

/// Requests go out from `&self`, so the fetch loop's `Fetcher` can hold
/// one client; the counts are atomics for the same reason.
#[derive(Default)]
pub struct ClaudeClient {
    requests: AtomicU64,
    network_ms: AtomicU64,
    /// The source's latchkey settings, forwarded onto every request this
    /// client issues (see `HttpRequest::latchkey`).
    latchkey: LatchkeySettings,
}

impl ClaudeClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_latchkey(latchkey: LatchkeySettings) -> Self {
        Self {
            latchkey,
            ..Self::default()
        }
    }

    /// The identity this client authenticates as, for a file's bytes,
    /// which the fetch builds as its own `HttpRequest`.
    pub fn latchkey(&self) -> &LatchkeySettings {
        &self.latchkey
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn network_seconds(&self) -> f64 {
        self.network_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Counts a request this client did not make through [`Self::get`].
    pub fn count(&self, duration_ms: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.network_ms.fetch_add(duration_ms, Ordering::Relaxed);
    }

    #[instrument(skip(self), fields(path = path))]
    pub async fn get(&self, path: &str) -> Result<Value, ClaudeError> {
        let url = format!("{BASE}{path}");
        let req = HttpRequest::get(HttpService::Claude, &url).header("Accept", "application/json");
        self.send(req, path).await
    }

    async fn send(&self, req: HttpRequest, path: &str) -> Result<Value, ClaudeError> {
        let req = req
            .latchkey(self.latchkey.clone())
            .timeout(LATCHKEY_TIMEOUT);
        let resp = latchkey_curl(&req).await.map_err(map_transport_error)?;
        self.count(resp.duration_ms);

        let body = resp.body_str();
        if resp.status == 403 {
            // Cloudflare's bot wall is a 403 too; its marker is what tells
            // a block from a credential that may not do this.
            return Err(ClaudeError::Forbidden(
                format!(
                    "GET {path} -> HTTP 403 cf-mitigated={:?}",
                    resp.header("cf-mitigated")
                ),
                Upstream::CredentialRefused,
            ));
        }
        if resp.status != 200 {
            return Err(ClaudeError::Permanent(
                format!(
                    "GET {path} -> HTTP {}: {:?}",
                    resp.status,
                    body.chars().take(300).collect::<String>()
                ),
                Upstream::of_status(resp.status),
            ));
        }
        let value: Value = serde_json::from_str(&body).map_err(|e| {
            let preview: String = body.chars().take(200).collect();
            ClaudeError::permanent(format!(
                "GET {path}: invalid JSON: {e}; body[:200]={preview:?}"
            ))
        })?;
        Ok(value)
    }

    pub async fn current_account(&self) -> Result<Value, ClaudeError> {
        self.get("/account").await
    }

    pub async fn list_orgs(&self) -> Result<Vec<Value>, ClaudeError> {
        let v = self.get("/organizations").await?;
        v.as_array()
            .cloned()
            .ok_or_else(|| ClaudeError::permanent("/organizations: expected array".to_string()))
    }

    pub async fn list_conversations(&self, org_uuid: &str) -> Result<Vec<Value>, ClaudeError> {
        let v = self
            .get(&format!("/organizations/{org_uuid}/chat_conversations"))
            .await?;
        v.as_array().cloned().ok_or_else(|| {
            ClaudeError::permanent(format!(
                "/organizations/{org_uuid}/chat_conversations: expected array"
            ))
        })
    }

    /// `GET /organizations/{org}/projects` — every project in one org.
    /// Like the conversation listing, a `403` here means "no project
    /// permission for this org" and is the caller's to swallow.
    pub async fn list_projects(&self, org_uuid: &str) -> Result<Vec<Value>, ClaudeError> {
        let v = self
            .get(&format!("/organizations/{org_uuid}/projects"))
            .await?;
        v.as_array().cloned().ok_or_else(|| {
            ClaudeError::permanent(format!(
                "/organizations/{org_uuid}/projects: expected array"
            ))
        })
    }

    /// `GET /organizations/{org}/projects/{project}/docs` — the
    /// project's knowledge documents. Unlike chat attachments, each
    /// doc carries its full text inline in `content`, so there is no
    /// second fetch and nothing to put in the blob CAS.
    pub async fn list_project_docs(
        &self,
        org_uuid: &str,
        project_uuid: &str,
    ) -> Result<Vec<Value>, ClaudeError> {
        let path = format!("/organizations/{org_uuid}/projects/{project_uuid}/docs");
        let v = self.get(&path).await?;
        v.as_array()
            .cloned()
            .ok_or_else(|| ClaudeError::permanent(format!("{path}: expected array")))
    }

    pub async fn get_conversation(
        &self,
        org_uuid: &str,
        conv_uuid: &str,
    ) -> Result<Value, ClaudeError> {
        let q = "tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong";
        self.get(&format!(
            "/organizations/{org_uuid}/chat_conversations/{conv_uuid}?{q}"
        ))
        .await
    }

    /// Every Cowork task of one org that is a session only, page by
    /// page. A task claude.ai has turned into a chat is not listed here.
    pub async fn list_cowork_sessions(&self, org_uuid: &str) -> Result<Vec<Value>, ClaudeError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let url = cowork_sessions_url(cursor.as_deref());
            let page = self
                .send(sessions_request(&url, org_uuid), "/v1/code/sessions")
                .instrument(info_span!("claude_cowork_listing"))
                .await?;
            out.extend(data_of(&page, "/v1/code/sessions")?);
            match next_cursor(&page) {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
    }

    /// One Cowork task as one request answers it, by id.
    pub async fn get_cowork_session(
        &self,
        org_uuid: &str,
        session_id: &str,
    ) -> Result<Value, ClaudeError> {
        let url = format!("{SESSIONS_BASE}/{}", percent_encode(session_id));
        let v = self
            .send(sessions_request(&url, org_uuid), "/v1/code/sessions/{id}")
            .await?;
        // Answered wrapped; the listing's entries are not.
        Ok(v.get("response_shape").cloned().unwrap_or(v))
    }

    /// A Cowork task's whole log, oldest first.
    pub async fn cowork_events(
        &self,
        org_uuid: &str,
        session_id: &str,
    ) -> Result<Vec<Value>, ClaudeError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let url = cowork_events_url(session_id, cursor.as_deref());
            let page = self
                .send(
                    sessions_request(&url, org_uuid),
                    "/v1/code/sessions/{id}/events",
                )
                .await?;
            let data = data_of(&page, "/v1/code/sessions/{id}/events")?;
            let empty = data.is_empty();
            out.extend(data);
            match next_cursor(&page) {
                Some(next) if !empty && Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
    }
}

fn data_of(page: &Value, what: &str) -> Result<Vec<Value>, ClaudeError> {
    page.get("data")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| ClaudeError::permanent(format!("{what}: expected a `data` array")))
}

fn next_cursor(page: &Value) -> Option<String> {
    page.get("next_cursor")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn map_transport_error(e: HttpError) -> ClaudeError {
    match e {
        HttpError::GaveUp { .. } => ClaudeError::RateLimited(e.to_string(), e.upstream()),
        other => ClaudeError::Permanent(other.to_string(), other.upstream()),
    }
}
