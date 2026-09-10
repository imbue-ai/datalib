//! Per-method response-shape knowledge for raw-API capture.

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
