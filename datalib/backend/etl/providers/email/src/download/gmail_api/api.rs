//! Gmail REST API transport: the handful of endpoints we call, plus the
//! client-side quota throttle.
//!
//! Auth needs no configuration at all. latchkey ships a built-in
//! `google-gmail` service whose `baseApiUrls` is
//! `https://gmail.googleapis.com/`, and it routes by URL host — so the
//! ordinary [`latchkey_curl`] path every other HTTP provider in this tree
//! uses injects the bearer token, and refreshes it when it has expired.
//! Contrast the IMAP mode, which is not HTTP and needs the credential
//! extracted as values first.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::time::Instant;
use tracing::{debug, warn};

use datalib_etl::http::{latchkey_curl, HttpRequest, HttpResponse};

/// Playback / impersonation key. Not in `IMPERSONATE_PROVIDERS` — Google
/// does not front the API with a JA3 wall.
pub const PROVIDER: &str = "gmail";

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Quota unit costs, from Google's published per-method table. Used by
/// the throttle to spend the per-minute budget accurately rather than
/// counting requests, which would badly misprice a mixed workload.
pub const UNITS_MESSAGES_GET: u32 = 20;
pub const UNITS_MESSAGES_LIST: u32 = 5;
pub const UNITS_HISTORY_LIST: u32 = 2;
pub const UNITS_LABELS_LIST: u32 = 1;
pub const UNITS_GET_PROFILE: u32 = 1;

/// Client-side quota throttle: a leaky bucket over Gmail's per-user
/// "quota units per minute" limit.
///
/// Google's limit is 6000 units/minute per user, and `messages.get` costs
/// 20 — so the real ceiling on a backfill is ~300 messages/minute no
/// matter how much concurrency we throw at it. Throttling ourselves is
/// better than discovering the limit as a 429 storm: we keep the request
/// pattern polite, and the run's pace is predictable enough to report.
#[derive(Debug)]
pub struct QuotaThrottle {
    units_per_minute: u32,
    /// Units available right now. Refills continuously, not on a minute
    /// boundary, so a burst at 0:59 can't double-spend at 1:01.
    available: f64,
    last_refill: Instant,
    spent_total: u64,
}

impl QuotaThrottle {
    pub fn new(units_per_minute: u32) -> Self {
        let units = units_per_minute.max(1);
        Self {
            units_per_minute: units,
            // Start full: a fresh run should not wait before its first
            // request.
            available: f64::from(units),
            last_refill: Instant::now(),
            spent_total: 0,
        }
    }

    pub fn spent_total(&self) -> u64 {
        self.spent_total
    }

    /// Wait until `cost` units are available, then spend them.
    pub async fn acquire(&mut self, cost: u32) {
        // A single request costing more than the whole per-minute budget
        // would never be satisfiable; let it through rather than hang.
        let cost = f64::from(cost).min(f64::from(self.units_per_minute));
        loop {
            self.refill();
            if self.available >= cost {
                self.available -= cost;
                self.spent_total += cost as u64;
                return;
            }
            let seconds = wait_seconds(self.available, cost, self.units_per_minute);
            debug!(
                event = "gmail_quota_wait",
                seconds, cost, "throttling to stay under the per-user quota",
            );
            tokio::time::sleep(Duration::from_secs_f64(seconds.max(0.01))).await;
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed <= 0.0 {
            return;
        }
        self.last_refill = now;
        self.available = (self.available + elapsed * f64::from(self.units_per_minute) / 60.0)
            .min(f64::from(self.units_per_minute));
    }
}

/// How long to wait before `cost` units are available, given `available`
/// now and a refill rate of `units_per_minute`.
///
/// Split out from [`QuotaThrottle::acquire`] so the arithmetic can be
/// checked without a clock — the alternative is `tokio`'s `test-util`
/// paused-time, which this crate does not compile with.
fn wait_seconds(available: f64, cost: f64, units_per_minute: u32) -> f64 {
    let deficit = cost - available;
    if deficit <= 0.0 {
        return 0.0;
    }
    deficit * 60.0 / f64::from(units_per_minute.max(1))
}

/// One authenticated GET against the Gmail API, returning parsed JSON.
async fn get_json(url: &str, account: Option<&str>) -> Result<Value> {
    let req = HttpRequest::get(PROVIDER, url)
        .timeout(REQUEST_TIMEOUT)
        .account(account);
    let resp = latchkey_curl(&req).await.map_err(|e| anyhow!("{e}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(api_error(url, &resp));
    }
    serde_json::from_slice(&resp.body).with_context(|| format!("parsing the response to {url}"))
}

/// Distinguish the failures a caller has to act on differently from the
/// ones it can only report.
fn api_error(url: &str, resp: &HttpResponse) -> anyhow::Error {
    if resp.status == 404 {
        // The one status with load-bearing meaning here: a `startHistoryId`
        // outside the retained window. The caller falls back to a full sync.
        return anyhow::Error::new(GmailApiError::HistoryTooOld);
    }
    if resp.status == 401 || resp.status == 403 {
        let body = resp.body_str();
        // Scope problems and expired tokens both land here and have very
        // different fixes, so quote Google rather than guessing.
        warn!(event = "gmail_auth_error", status = resp.status, url);
        return anyhow!(
            "Gmail API {url} → HTTP {}: {body}\n\
             If this says ACCESS_TOKEN_SCOPE_INSUFFICIENT, re-run \
             `latchkey auth browser google-gmail` and approve every scope. \
             If it says the credential is missing, run that command for the first time.",
            resp.status,
        );
    }
    anyhow!(
        "Gmail API {url} → HTTP {}: {}",
        resp.status,
        resp.body_str()
    )
}

#[derive(Debug, thiserror::Error)]
pub enum GmailApiError {
    /// `history.list` returned 404: the stored `historyId` is older than
    /// Google's retention window (documented as "typically at least one
    /// week"), so partial sync is impossible and a full sync is required.
    #[error("the stored Gmail historyId is outside the retained window; a full sync is required")]
    HistoryTooOld,
}

/// `users.getProfile` — the account address and the mailbox's current
/// `historyId`, which is the cursor a first full sync will store.
pub async fn get_profile(user_id: &str, account: Option<&str>) -> Result<Profile> {
    let v = get_json(&format!("{BASE}/{user_id}/profile"), account).await?;
    Ok(Profile {
        email_address: str_field(&v, "emailAddress")
            .ok_or_else(|| anyhow!("users.getProfile returned no emailAddress"))?,
        history_id: str_field(&v, "historyId"),
        messages_total: v.get("messagesTotal").and_then(Value::as_u64),
    })
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub email_address: String,
    pub history_id: Option<String>,
    pub messages_total: Option<u64>,
}

/// `users.labels.list` — every label, so `labelIds` on a message can be
/// resolved to names.
pub async fn list_labels(user_id: &str, account: Option<&str>) -> Result<Vec<Label>> {
    let v = get_json(&format!("{BASE}/{user_id}/labels"), account).await?;
    Ok(v.get("labels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Label::from_json).collect())
        .unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub id: String,
    pub name: String,
    /// Google reports `"system"` or `"user"`. It matters: a *user* label
    /// literally named `INBOX` must not be folded onto the system inbox.
    pub is_system: bool,
}

impl Label {
    fn from_json(v: &Value) -> Option<Self> {
        Some(Label {
            id: str_field(v, "id")?,
            name: str_field(v, "name")?,
            is_system: str_field(v, "type").as_deref() == Some("system"),
        })
    }
}

/// One page of `users.messages.list`.
#[derive(Debug, Clone, Default)]
pub struct MessagePage {
    pub ids: Vec<String>,
    pub next_page_token: Option<String>,
}

/// `users.messages.list` — ids only. `include_spam_trash` is on: the
/// point of a mirror is everything, and the render-side label filter is
/// where a user narrows what they actually look at.
pub async fn list_messages(
    user_id: &str,
    account: Option<&str>,
    page_token: Option<&str>,
    page_size: u32,
) -> Result<MessagePage> {
    let mut url = format!("{BASE}/{user_id}/messages?maxResults={page_size}&includeSpamTrash=true");
    if let Some(token) = page_token {
        url.push_str("&pageToken=");
        url.push_str(token);
    }
    let v = get_json(&url, account).await?;
    Ok(MessagePage {
        ids: v
            .get("messages")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|m| str_field(m, "id")).collect())
            .unwrap_or_default(),
        next_page_token: str_field(&v, "nextPageToken"),
    })
}

/// `users.messages.get?format=RAW` — the metadata plus the complete
/// RFC 5322 source.
///
/// `format=RAW` is what makes this mode line up with every other one: the
/// `raw` field is the same `.eml` the mbox and IMAP paths store, so all
/// of them share one envelope-synthesis path and one CAS entry per
/// message.
pub async fn get_message_raw(
    user_id: &str,
    account: Option<&str>,
    id: &str,
) -> Result<GmailMessage> {
    let v = get_json(
        &format!("{BASE}/{user_id}/messages/{id}?format=RAW"),
        account,
    )
    .await?;
    GmailMessage::from_json(&v)
}

/// A Gmail API message, minus the parsed payload we never ask for.
#[derive(Debug, Clone)]
pub struct GmailMessage {
    pub id: String,
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub history_id: Option<String>,
    /// `internalDate`, epoch **milliseconds** as a string.
    pub internal_date_ms: Option<i64>,
    pub size_estimate: Option<u64>,
    /// The decoded RFC 5322 source.
    pub raw: Vec<u8>,
}

impl GmailMessage {
    fn from_json(v: &Value) -> Result<Self> {
        let id = str_field(v, "id").ok_or_else(|| anyhow!("messages.get returned no id"))?;
        let encoded = str_field(v, "raw")
            .ok_or_else(|| anyhow!("messages.get({id}) returned no raw field"))?;
        Ok(GmailMessage {
            raw: decode_base64url(&encoded)
                .with_context(|| format!("decoding the raw field of message {id}"))?,
            thread_id: str_field(v, "threadId").unwrap_or_else(|| id.clone()),
            label_ids: v
                .get("labelIds")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
            history_id: str_field(v, "historyId"),
            internal_date_ms: str_field(v, "internalDate").and_then(|s| s.parse().ok()),
            size_estimate: v.get("sizeEstimate").and_then(Value::as_u64),
            id,
        })
    }
}

/// What changed since a `historyId`.
#[derive(Debug, Clone, Default)]
pub struct HistoryPage {
    /// Message ids whose content is new to us (`messagesAdded`).
    pub added: Vec<String>,
    /// Message ids removed from the mailbox (`messagesDeleted`).
    pub deleted: Vec<String>,
    /// Message ids whose labels moved (`labelsAdded` / `labelsRemoved`).
    /// The bodies are unchanged, so only the envelope needs re-reading.
    pub relabeled: Vec<String>,
    pub next_page_token: Option<String>,
    pub history_id: Option<String>,
}

/// `users.history.list` — the incremental cursor.
///
/// Strictly better than IMAP's CONDSTORE for this job: deletions are
/// reported explicitly, where Gmail's IMAP advertises CONDSTORE but not
/// QRESYNC and so has no `VANISHED` — deletions there can only be found
/// by re-listing every UID.
///
/// Returns [`GmailApiError::HistoryTooOld`] when `start_history_id` has
/// aged out; the caller must fall back to a full sync.
pub async fn list_history(
    user_id: &str,
    account: Option<&str>,
    start_history_id: &str,
    page_token: Option<&str>,
) -> Result<HistoryPage> {
    let mut url = format!("{BASE}/{user_id}/history?startHistoryId={start_history_id}");
    if let Some(token) = page_token {
        url.push_str("&pageToken=");
        url.push_str(token);
    }
    let v = get_json(&url, account).await?;
    Ok(parse_history(&v))
}

pub fn parse_history(v: &Value) -> HistoryPage {
    let mut page = HistoryPage {
        next_page_token: str_field(v, "nextPageToken"),
        history_id: str_field(v, "historyId"),
        ..Default::default()
    };
    let Some(records) = v.get("history").and_then(Value::as_array) else {
        return page;
    };
    for record in records {
        collect_ids(record, "messagesAdded", &mut page.added);
        collect_ids(record, "messagesDeleted", &mut page.deleted);
        collect_ids(record, "labelsAdded", &mut page.relabeled);
        collect_ids(record, "labelsRemoved", &mut page.relabeled);
    }
    // A message added and then deleted inside one history window must not
    // be fetched: the id is gone and `messages.get` would 404.
    page.added.retain(|id| !page.deleted.contains(id));
    page.relabeled
        .retain(|id| !page.deleted.contains(id) && !page.added.contains(id));
    dedupe(&mut page.added);
    dedupe(&mut page.deleted);
    dedupe(&mut page.relabeled);
    page
}

/// Each history record type wraps its messages as
/// `[{ "message": { "id": … } }, …]`.
fn collect_ids(record: &Value, key: &str, out: &mut Vec<String>) {
    let Some(items) = record.get(key).and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if let Some(id) = item.get("message").and_then(|m| str_field(m, "id")) {
            out.push(id);
        }
    }
}

fn dedupe(ids: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// base64url → bytes. Gmail's `raw` field is base64url (RFC 4648 §5,
/// `-`/`_` instead of `+`/`/`) and unpadded, which the standard alphabet
/// would reject outright.
pub fn decode_base64url(s: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for (i, c) in s.bytes().enumerate() {
        // Gmail wraps long values with newlines in some responses.
        if c == b'\n' || c == b'\r' || c == b'=' {
            continue;
        }
        let six = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            // Accept the standard alphabet too: costs nothing, and makes
            // a hand-pasted fixture that used `+`/`/` work.
            b'+' => 62,
            b'/' => 63,
            other => return Err(anyhow!("invalid base64url byte {other:#x} at offset {i}")),
        };
        acc = (acc << 6) | u32::from(six);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_gmails_unpadded_base64url() {
        // "From: a@b\r\n\r\nhi" — the `-`/`_` alphabet, no padding.
        let raw = b"From: a@b\r\n\r\nhi".to_vec();
        let encoded = {
            // Encode with the same alphabet so the test is self-contained.
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in raw.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..=chunk.len() {
                    out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                }
            }
            out
        };
        assert_eq!(decode_base64url(&encoded).unwrap(), raw);
    }

    /// The URL-safe alphabet is the whole point: `-` and `_` are byte 62
    /// and 63, not errors.
    #[test]
    fn accepts_the_url_safe_alphabet() {
        assert_eq!(decode_base64url("--__").unwrap(), vec![0xfb, 0xef, 0xff]);
        assert!(decode_base64url("!!!!").is_err());
    }

    #[test]
    fn tolerates_padding_and_wrapping() {
        let a = decode_base64url("aGVsbG8=").unwrap();
        let b = decode_base64url("aGVs\r\nbG8").unwrap();
        assert_eq!(a, b"hello");
        assert_eq!(a, b);
    }

    fn history(records: Value) -> Value {
        json!({ "history": records, "historyId": "9999" })
    }

    fn msg(id: &str) -> Value {
        json!({ "message": { "id": id } })
    }

    #[test]
    fn parses_the_four_history_record_types() {
        let page = parse_history(&history(json!([
            { "messagesAdded":   [msg("a1")] },
            { "messagesDeleted": [msg("d1")] },
            { "labelsAdded":     [msg("r1")] },
            { "labelsRemoved":   [msg("r2")] },
        ])));
        assert_eq!(page.added, vec!["a1"]);
        assert_eq!(page.deleted, vec!["d1"]);
        assert_eq!(page.relabeled, vec!["r1", "r2"]);
        assert_eq!(page.history_id.as_deref(), Some("9999"));
    }

    /// A message added and then deleted within one window no longer
    /// exists; fetching it would 404 and fail the run.
    #[test]
    fn does_not_fetch_a_message_deleted_in_the_same_window() {
        let page = parse_history(&history(json!([
            { "messagesAdded":   [msg("x")] },
            { "messagesDeleted": [msg("x")] },
        ])));
        assert!(page.added.is_empty());
        assert_eq!(page.deleted, vec!["x"]);
    }

    /// A message that was added *and* relabeled needs one full fetch, not
    /// a fetch plus a redundant envelope refresh.
    #[test]
    fn prefers_a_full_fetch_over_a_relabel_for_the_same_id() {
        let page = parse_history(&history(json!([
            { "messagesAdded": [msg("x")] },
            { "labelsAdded":   [msg("x")] },
        ])));
        assert_eq!(page.added, vec!["x"]);
        assert!(page.relabeled.is_empty());
    }

    #[test]
    fn dedupes_repeated_ids_across_records() {
        let page = parse_history(&history(json!([
            { "labelsAdded": [msg("x"), msg("x")] },
            { "labelsRemoved": [msg("x")] },
        ])));
        assert_eq!(page.relabeled, vec!["x"]);
    }

    /// An empty history response means "nothing changed", not an error.
    #[test]
    fn reads_an_empty_history_as_no_changes() {
        let page = parse_history(&json!({ "historyId": "5" }));
        assert!(page.added.is_empty() && page.deleted.is_empty() && page.relabeled.is_empty());
        assert_eq!(page.history_id.as_deref(), Some("5"));
    }

    #[test]
    fn reads_a_label_list() {
        let labels: Vec<Label> = json!([
            { "id": "INBOX", "name": "INBOX", "type": "system" },
            { "id": "Label_7", "name": "Work/Projects", "type": "user" },
            { "id": "no-name" },
        ])
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Label::from_json)
        .collect();
        assert_eq!(labels.len(), 2, "a label with no name is unusable");
        assert!(labels[0].is_system);
        assert!(!labels[1].is_system);
        assert_eq!(labels[1].name, "Work/Projects");
    }

    /// A fresh throttle must not make the first request wait.
    #[tokio::test]
    async fn lets_the_first_request_through_immediately() {
        let mut t = QuotaThrottle::new(6_000);
        let start = std::time::Instant::now();
        t.acquire(UNITS_MESSAGES_GET).await;
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(t.spent_total(), u64::from(UNITS_MESSAGES_GET));
    }

    /// The arithmetic behind the wait. At Gmail's real 6000 units/minute,
    /// a `messages.get` costs 20, so a fully-drained bucket owes 0.2s —
    /// i.e. ~300 messages/minute, the number the docs imply.
    #[test]
    fn prices_the_wait_off_the_refill_rate() {
        assert_eq!(wait_seconds(0.0, 20.0, 6_000), 0.2);
        assert_eq!(wait_seconds(0.0, 6_000.0, 6_000), 60.0);
        // Halving the ceiling doubles the wait.
        assert_eq!(wait_seconds(0.0, 20.0, 3_000), 0.4);
    }

    /// Units already in the bucket are spent before any waiting.
    #[test]
    fn does_not_wait_while_the_bucket_still_covers_the_cost() {
        assert_eq!(wait_seconds(100.0, 20.0, 6_000), 0.0);
        assert_eq!(wait_seconds(20.0, 20.0, 6_000), 0.0);
        // Partial cover shortens the wait rather than ignoring it.
        assert!(wait_seconds(10.0, 20.0, 6_000) > 0.0);
        assert!(wait_seconds(10.0, 20.0, 6_000) < wait_seconds(0.0, 20.0, 6_000));
    }

    /// Spending the whole budget must actually block the next request, or
    /// the throttle is decorative. Sized so the wait is real but short:
    /// 20 units at 6000/min is 0.2s.
    #[tokio::test]
    async fn blocks_once_the_minute_budget_is_spent() {
        let mut t = QuotaThrottle::new(6_000);
        t.acquire(6_000).await;
        let start = std::time::Instant::now();
        t.acquire(20).await;
        assert!(
            start.elapsed() >= Duration::from_millis(150),
            "did not throttle: waited {:?}",
            start.elapsed()
        );
    }

    /// A cost larger than the entire per-minute budget would otherwise
    /// loop forever, since the bucket can never hold that much.
    #[tokio::test]
    async fn does_not_hang_on_a_cost_above_the_whole_budget() {
        let mut t = QuotaThrottle::new(6_000);
        t.acquire(1_000_000).await;
        assert!(t.spent_total() > 0);
    }
}
