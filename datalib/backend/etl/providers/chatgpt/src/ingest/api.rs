//! ChatGPT API transport. Every request goes through
//! [`datalib_etl_web::http::latchkey_curl`], which captures the full
//! response (status, every header, body) and supports playback from
//! disk fixtures. Mirrors `src/ingest/chatgpt_web.py:_curl_get`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::{info, instrument};

use datalib_etl::events;
use datalib_etl_web::http::{
    latchkey_curl, HttpError, HttpRequest, HttpResponse, HttpService, LatchkeySettings,
};

pub const BASE: &str = "https://chatgpt.com";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);
const ME: &str = "/backend-api/me";
/// The latchkey service the wizard registers for ChatGPT.
const LATCHKEY_SERVICE: &str = "chatgpt";
/// A browser that still holds chatgpt.com's session signs in within
/// about ten seconds; the rest is room for a person who sees the window.
const RENEW_LIMIT: Duration = Duration::from_secs(120);

type Renew =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

/// Signs in again when the access token has expired. Tried once per
/// client: every request that saw the 401 waits on the one attempt.
struct Renewal {
    renew: Renew,
    outcome: OnceCell<Result<(), String>>,
}

#[derive(thiserror::Error, Debug)]
pub enum ChatGPTError {
    /// The shared retry loop respected rate limits / backed off but the
    /// orchestrator's give-up policy tripped (`reason` says which bound).
    /// Surfaced distinctly from `Permanent` so the caller can stop fetching
    /// cleanly and the user resumes later via the incremental-skip path.
    #[error("rate-limited on {path}; gave up retrying: {reason}")]
    RateLimited { path: String, reason: String },
    #[error("{0}")]
    Permanent(String),
}

/// Requests go out from `&self`, so the fetch loop's `Fetcher` can hold
/// one client; the counts are atomics for the same reason.
#[derive(Default)]
pub struct ChatGPTClient {
    requests: AtomicU64,
    network_ms: AtomicU64,
    /// The source's latchkey settings, forwarded onto every request this
    /// client issues (see `HttpRequest::latchkey`).
    latchkey: LatchkeySettings,
    /// The `/me` id this client signs in as: the one the store already
    /// holds, else the first `/me` answered. `me()` refuses any other,
    /// and a renewal that comes back as another is not used.
    account: OnceLock<String>,
    renewal: Option<Renewal>,
}

impl ChatGPTClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_latchkey(latchkey: LatchkeySettings) -> Self {
        Self {
            latchkey,
            ..Self::default()
        }
    }

    /// The `/me` id the store holds, which this client then insists on.
    pub fn holding_account(self, id: Option<String>) -> Self {
        if let Some(id) = id {
            let _ = self.account.set(id);
        }
        self
    }

    /// On a `401`, sign in again through latchkey's browser login, which
    /// mints a fresh token from the browser session latchkey keeps.
    pub fn renewing_by_browser_login(self) -> Self {
        let latchkey = self.latchkey.clone();
        self.renew_with(move || {
            let latchkey = latchkey.clone();
            async move {
                datalib_etl_web::latchkey::renew_by_browser_login(
                    LATCHKEY_SERVICE,
                    &latchkey,
                    RENEW_LIMIT,
                )
                .await
            }
        })
    }

    pub fn renew_with<F, Fut>(mut self, renew: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.renewal = Some(Renewal {
            renew: Box::new(move || Box::pin(renew())),
            outcome: OnceCell::new(),
        });
        self
    }

    /// The identity this client authenticates as, for the attachment
    /// bytes behind a signed URL, which the fetch builds as its own
    /// `HttpRequest`.
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
    pub async fn get(&self, path: &str) -> Result<Value, ChatGPTError> {
        let mut resp = self.send(path).await?;
        let mut renewal_failed = None;
        if resp.status == 401 {
            match self.renewed().await {
                Some(Ok(())) => resp = self.send(path).await?,
                Some(Err(why)) => renewal_failed = Some(why),
                None => {}
            }
        }
        match (json_body(path, &resp)?, renewal_failed) {
            (Some(value), _) => {
                events::item_fetched(
                    &format!("{BASE}{path}"),
                    resp.body.len() as u64,
                    resp.duration_ms,
                );
                Ok(value)
            }
            (None, why) => {
                let body_preview: String = resp.body_str().chars().take(300).collect();
                let mut message = format!(
                    "GET {path} -> HTTP {} cf-mitigated={:?} body={:?}",
                    resp.status,
                    resp.header("cf-mitigated"),
                    body_preview
                );
                if let Some(why) = why {
                    message.push_str(&format!("; signing in again did not help: {why}"));
                }
                Err(ChatGPTError::Permanent(message))
            }
        }
    }

    async fn send(&self, path: &str) -> Result<HttpResponse, ChatGPTError> {
        let req = HttpRequest::get(HttpService::Chatgpt, format!("{BASE}{path}"))
            .header("Accept", "application/json")
            .latchkey(self.latchkey.clone())
            .timeout(LATCHKEY_TIMEOUT);
        // 429 retry — `Retry-After`, exponential backoff, and the give-up
        // bound — is handled centrally in the shared chokepoint. When it
        // gives up, surface `RateLimited` (not `Permanent`) so the caller
        // stops cleanly and the user resumes later.
        let resp = latchkey_curl(&req).await.map_err(|e: HttpError| match e {
            HttpError::GaveUp { reason, .. } => ChatGPTError::RateLimited {
                path: path.to_string(),
                reason,
            },
            other => ChatGPTError::Permanent(format!("GET {path}: {other}")),
        })?;
        self.count(resp.duration_ms);
        Ok(resp)
    }

    /// `None` when this client does not renew, or does not yet know which
    /// account it is: a token for some other account is worse than none.
    async fn renewed(&self) -> Option<Result<(), String>> {
        let renewal = self.renewal.as_ref()?;
        let held = self.account.get()?;
        let outcome = renewal
            .outcome
            .get_or_init(|| self.renew_as(&renewal.renew, held))
            .await;
        Some(outcome.clone())
    }

    /// latchkey keeps one browser session for every account, and
    /// chatgpt.com's login cannot say which account it captured, so the
    /// new token counts only once `/me` says it is the one held.
    async fn renew_as(&self, renew: &Renew, held: &str) -> Result<(), String> {
        renew().await.map_err(|e| format!("{e:#}"))?;
        let resp = self.send(ME).await.map_err(|e| e.to_string())?;
        let me = json_body(ME, &resp)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{ME} still answers HTTP {}", resp.status))?;
        match account_id(&me) {
            Some(id) if id == held => {
                info!(
                    event = "chatgpt_signed_in_again",
                    "the access token had expired; signed in again from the browser"
                );
                Ok(())
            }
            other => Err(format!(
                "the browser is signed in to chatgpt.com account {:?}, \
                 not the {held:?} this source holds",
                other.unwrap_or("with no id")
            )),
        }
    }

    pub async fn me(&self) -> Result<Value, ChatGPTError> {
        let me = self.get(ME).await?;
        let id = account_id(&me)
            .ok_or_else(|| ChatGPTError::Permanent(format!("{ME} response missing id")))?;
        let held = self.account.get_or_init(|| id.to_string());
        if held != id {
            return Err(ChatGPTError::Permanent(format!(
                "latchkey's chatgpt credential signs in as account {id:?}, but this source holds \
                 {held:?}; sign in to that account again, or add a new source for this one"
            )));
        }
        Ok(me)
    }

    /// One page of the conversations, or of the archived ones: the
    /// plain listing leaves an archived conversation out.
    pub async fn list_conversations_page(
        &self,
        offset: usize,
        limit: usize,
        archived: bool,
    ) -> Result<Value, ChatGPTError> {
        let archived = if archived { "&is_archived=true" } else { "" };
        self.get(&format!(
            "/backend-api/conversations?offset={offset}&limit={limit}&order=updated{archived}"
        ))
        .await
    }

    pub async fn get_conversation(&self, conv_id: &str) -> Result<Value, ChatGPTError> {
        self.get(&format!("/backend-api/conversation/{conv_id}"))
            .await
    }
}

/// The parsed body of a `200`, `None` for any other status.
fn json_body(path: &str, resp: &HttpResponse) -> Result<Option<Value>, ChatGPTError> {
    if resp.status != 200 {
        return Ok(None);
    }
    let body = resp.body_str();
    serde_json::from_str(&body).map(Some).map_err(|e| {
        let preview: String = body.chars().take(200).collect();
        ChatGPTError::Permanent(format!(
            "GET {path}: 200 but non-JSON body: {e}; body[:200]={preview:?}"
        ))
    })
}

fn account_id(me: &Value) -> Option<&str> {
    me.get("id").and_then(Value::as_str)
}
