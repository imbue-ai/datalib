//! Read-only account probe: "do these credentials reach chatgpt.com, and
//! which conversations does the account have?" One request for the
//! account and a few listing pages — no conversation is ever
//! detail-fetched.

use anyhow::{anyhow, Result};
use serde_json::Value;

use datalib_etl_chatgpt_config::ChatgptConfig;
use datalib_source_common::probe::{
    sort_newest_first, ProbeAccount, ProbeItem, ProbeItemKind, ProbeReport,
};

use crate::ingest::api::{ChatGPTClient, ChatGPTError};
use crate::ingest::PAGE_SIZE;

/// How many conversations the report carries back. The picker exists to
/// scope a first run, not to page an archive, and the whole report
/// crosses a pipe as one JSON document.
const MAX_ITEMS: usize = 500;

pub async fn probe(config: &ChatgptConfig) -> Result<ProbeReport> {
    config.validate()?;
    if config.api.is_none() {
        return Err(anyhow!(
            "this chatgpt source names no `api` table, so there is nothing to connect to."
        ));
    }

    let mut client = ChatGPTClient::with_latchkey(config.latchkey_settings.clone());
    let me = client.me().await.map_err(credential_hint)?;

    let mut items: Vec<Value> = Vec::new();
    let mut total: Option<u64> = None;
    let mut offset = 0usize;
    while items.len() < MAX_ITEMS {
        let page = client
            .list_conversations_page(offset, PAGE_SIZE)
            .await
            .map_err(|e| anyhow!("list conversations at offset {offset}: {e}"))?;
        let page_items: Vec<Value> = page
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        total = page.get("total").and_then(Value::as_u64).or(total);
        if page_items.is_empty() {
            break;
        }
        offset += page_items.len();
        items.extend(page_items);
        if total.is_some_and(|t| offset as u64 >= t) {
            break;
        }
    }

    Ok(build_report(&me, items, total))
}

/// The report, from the account payload and the listing pages already
/// read. Pure so it can be checked against the checked-in fixture
/// without a server.
fn build_report(me: &Value, listed: Vec<Value>, total: Option<u64>) -> ProbeReport {
    let mut items: Vec<ProbeItem> = listed
        .iter()
        .filter_map(|conv| {
            let id = conv.get("id").and_then(Value::as_str)?;
            Some(ProbeItem {
                title: conv
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
                updated_at: conv.get("update_time").and_then(update_time_iso),
                ..ProbeItem::new(id, ProbeItemKind::Conversation)
            })
        })
        .collect();

    sort_newest_first(&mut items);
    let mut notes = Vec::new();
    if let Some(total) = total {
        if items.len() < total as usize {
            notes.push(format!(
                "{total} conversations in all; the {} most recently updated are listed.",
                items.len()
            ));
        }
    }

    ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id: me
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            address: me.get("email").and_then(Value::as_str).map(str::to_string),
            display_name: me.get("name").and_then(Value::as_str).map(str::to_string),
            // Conversations, not messages: the listing reports no
            // message count without opening every conversation.
            message_estimate: None,
        },
        items,
        notes,
    }
}

/// The listing's `update_time` is a float epoch on a fresh account and
/// an RFC 3339 string on some older ones; either way the picker wants a
/// string that sorts by time.
fn update_time_iso(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => {
            let secs = n.as_f64()?;
            let t = chrono::DateTime::from_timestamp(secs.trunc() as i64, 0)?;
            Some(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        }
        _ => None,
    }
}

/// A failure that is really a setup problem — `chatgpt` never registered
/// with latchkey, or its access token missing or expired — gets a
/// pointer at the fix; anything else passes through unembellished.
fn credential_hint(e: ChatGPTError) -> anyhow::Error {
    let s = e.to_string();
    let setup_problem = s.contains("No service matches URL")
        || s.to_ascii_lowercase().contains("no credentials")
        || s.contains("HTTP 401")
        || s.contains("HTTP 403");
    if !setup_problem {
        return anyhow!("fetch /me: {s}");
    }
    let lk = datalib_etl::latchkey::latchkey_cli_hint();
    anyhow!(
        "chatgpt.com credentials are not set up: {s}\n\
         Sign in through latchkey, which captures the access token itself:\n  \
         {lk} auth browser chatgpt\n\
         If that says the service has no browser login, see the ChatGPT section of\n\
         docs/user/getting_your_data.md for the one-time registration."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let dir = std::env::var("CHATGPT_FIXTURE_DIR").unwrap_or_else(|_| {
            format!("{}/tests/fixtures/chatgpt_api", env!("CARGO_MANIFEST_DIR"))
        });
        let text = std::fs::read_to_string(format!("{dir}/{name}")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The report is read straight off the shapes `/backend-api/me` and
    /// the listing return, so the fixture is the contract.
    #[test]
    fn report_from_the_fixture_shapes() {
        let me = fixture("me.json");
        let listed = fixture("conversations.json").as_array().unwrap().clone();
        let total = listed.len() as u64;
        let report = build_report(&me, listed, Some(total));

        assert_eq!(report.mode, "api");
        assert_eq!(report.account.id, "user-FAKE0DATAANDROID0POSITRONIC1");
        assert_eq!(
            report.account.address.as_deref(),
            Some("data@enterprise.starfleet.test")
        );
        assert_eq!(
            report.account.display_name.as_deref(),
            Some("Lt. Cmdr. Data")
        );
        assert_eq!(report.items.len(), total as usize);
        assert!(report.notes.is_empty(), "{:?}", report.notes);

        let first = &report.items[0];
        assert_eq!(first.kind, ProbeItemKind::Conversation);
        assert!(first.title.is_some());
        // The float epoch became a sortable timestamp.
        assert!(
            first
                .updated_at
                .as_deref()
                .is_some_and(|t| t.ends_with('Z')),
            "{:?}",
            first.updated_at
        );
        // Newest first.
        let times: Vec<&str> = report
            .items
            .iter()
            .filter_map(|i| i.updated_at.as_deref())
            .collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(times, sorted);
    }

    #[test]
    fn a_truncated_listing_says_so() {
        let me = fixture("me.json");
        let listed = fixture("conversations.json").as_array().unwrap().clone();
        let report = build_report(&me, listed, Some(1_000));
        assert_eq!(report.notes.len(), 1);
        assert!(
            report.notes[0].starts_with("1000 conversations in all"),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn refuses_a_source_without_an_api_table() {
        let cfg = ChatgptConfig::default();
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(probe(&cfg))
            .expect_err("nothing to connect to")
            .to_string();
        assert!(err.contains("nothing to connect to"), "{err}");
    }

    #[test]
    fn a_403_names_the_browser_login() {
        let hint = credential_hint(ChatGPTError::Permanent(
            "GET /backend-api/me -> HTTP 403".into(),
        ))
        .to_string();
        assert!(hint.contains("auth browser chatgpt"), "{hint}");
        let plain = credential_hint(ChatGPTError::Permanent("timed out".into())).to_string();
        assert!(!plain.contains("auth browser"), "{plain}");
    }
}
