//! Per-method response-shape knowledge for raw-API capture.
//!
//! [`items_in_response`] is the single place that knows how to extract
//! `(item_key, item_value)` pairs from a Slack response. Used at both
//! save time (to decide whether a page is a content-dup) and at startup
//! (to rebuild the dedup index from the on-disk JSONL).
//!
//! Item-key namespaces:
//!   * `conversations.list`    `<channel_id>`
//!   * `users.list`            `<user_id>`
//!   * `conversations.history` `<channel_param>\t<msg.ts>`
//!   * `conversations.replies` `<channel_param>\t<thread_ts_param>\t<msg.ts>`
//!   * `auth.test`             `<response.user_id>`

use std::collections::BTreeMap;

use serde_json::Value;

pub const M_AUTH_TEST: &str = "auth.test";
pub const M_CHANNELS: &str = "conversations.list";
pub const M_USERS: &str = "users.list";
pub const M_HISTORY: &str = "conversations.history";
pub const M_REPLIES: &str = "conversations.replies";

pub fn items_in_response(
    method: &str,
    params: &BTreeMap<String, String>,
    response: &Value,
) -> Vec<(String, Value)> {
    match method {
        M_AUTH_TEST => response
            .get("user_id")
            .and_then(|v| v.as_str())
            .map(|id| vec![(id.to_string(), response.clone())])
            .unwrap_or_default(),
        M_CHANNELS => array_items(response, "channels", |c| {
            c.get("id").and_then(|v| v.as_str()).map(str::to_string)
        }),
        M_USERS => array_items(response, "members", |u| {
            u.get("id").and_then(|v| v.as_str()).map(str::to_string)
        }),
        M_HISTORY => {
            let channel = params.get("channel").cloned().unwrap_or_default();
            array_items(response, "messages", |m| {
                m.get("ts")
                    .and_then(|v| v.as_str())
                    .map(|ts| format!("{}\t{}", channel, ts))
            })
        }
        M_REPLIES => {
            let channel = params.get("channel").cloned().unwrap_or_default();
            let thread_ts = params.get("ts").cloned().unwrap_or_default();
            array_items(response, "messages", |m| {
                m.get("ts")
                    .and_then(|v| v.as_str())
                    .map(|ts| format!("{}\t{}\t{}", channel, thread_ts, ts))
            })
        }
        _ => Vec::new(),
    }
}

fn array_items(
    response: &Value,
    field: &str,
    key_of: impl Fn(&Value) -> Option<String>,
) -> Vec<(String, Value)> {
    response
        .get(field)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| key_of(item).map(|k| (k, item.clone())))
                .collect()
        })
        .unwrap_or_default()
}

/// Walk all `conversations.history` keys in the index, projecting out
/// `channel -> max(ts)`. The resume cursor on the next forward pass.
pub fn latest_ts_by_channel<'a, I: Iterator<Item = &'a str>>(keys: I) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for k in keys {
        let mut parts = k.split('\t');
        let cid = parts.next().unwrap_or("");
        let ts = parts.next().unwrap_or("");
        if cid.is_empty() || ts.is_empty() {
            continue;
        }
        let entry = out.entry(cid.to_string()).or_default();
        if ts > entry.as_str() {
            *entry = ts.to_string();
        }
    }
    out
}

/// Walk all `conversations.replies` keys, projecting out
/// `(channel, thread_ts) -> max(reply_ts)`. Used to skip threads whose
/// latest reply we've already captured.
pub fn latest_reply_by_thread<'a, I: Iterator<Item = &'a str>>(
    keys: I,
) -> BTreeMap<(String, String), String> {
    let mut out: BTreeMap<(String, String), String> = BTreeMap::new();
    for k in keys {
        let mut parts = k.split('\t');
        let cid = parts.next().unwrap_or("").to_string();
        let tts = parts.next().unwrap_or("").to_string();
        let rts = parts.next().unwrap_or("").to_string();
        if cid.is_empty() || tts.is_empty() || rts.is_empty() {
            continue;
        }
        let entry = out.entry((cid, tts)).or_default();
        if rts.as_str() > entry.as_str() {
            *entry = rts;
        }
    }
    out
}
