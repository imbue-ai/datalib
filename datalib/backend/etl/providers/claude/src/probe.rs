//! Read-only account probe: "do these credentials reach claude.ai, and
//! which conversations does the account have?" One request for the
//! account, one for the org list, and one per org for its conversation
//! listing — no conversation is ever detail-fetched.

use anyhow::{anyhow, Result};
use serde_json::Value;

use datalib_etl_claude_config::ClaudeConfig;
use datalib_source_common::probe::{
    sort_newest_first, ProbeAccount, ProbeItem, ProbeItemKind, ProbeReport,
};

use crate::ingest::api::ClaudeClient;

/// How many conversations the report carries back. The picker exists to
/// scope a first run, not to page an archive, and the whole report
/// crosses a pipe as one JSON document.
const MAX_ITEMS: usize = 500;

pub async fn probe(config: &ClaudeConfig) -> Result<ProbeReport> {
    config.validate()?;
    if config.api.is_none() {
        return Err(anyhow!(
            "this claude source reads an unpacked export off disk, so there is no connection \
             to test. Set `api` to mirror the live claude.ai account instead."
        ));
    }

    let mut client = ClaudeClient::with_latchkey(config.latchkey_settings.clone());
    let account = client
        .current_account()
        .await
        .map_err(crate::ingest::credential_hint)?;
    let orgs = client
        .list_orgs()
        .await
        .map_err(crate::ingest::credential_hint)?;

    let mut items: Vec<ProbeItem> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut listed = 0usize;
    for org in &orgs {
        let Some(org_uuid) = org.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        let org_name = org
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(org_uuid)
            .to_string();
        // A 403 here is one org this credential has no chat permission
        // for, which is ordinary on an account that belongs to a team.
        // Say so and keep going; a credential that reaches nothing at
        // all has already failed above.
        let convs = match client.list_conversations(org_uuid).await {
            Ok(convs) => convs,
            Err(e) => {
                notes.push(format!("{org_name}: no conversations listed ({e})"));
                continue;
            }
        };
        listed += convs.len();
        for conv in &convs {
            let Some(uuid) = conv.get("uuid").and_then(Value::as_str) else {
                continue;
            };
            items.push(ProbeItem {
                title: conv
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
                updated_at: conv
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ..ProbeItem::new(uuid, ProbeItemKind::Conversation)
            });
        }
    }

    sort_newest_first(&mut items);
    if items.len() > MAX_ITEMS {
        notes.push(format!(
            "{listed} conversations in all; the {MAX_ITEMS} most recently updated are listed."
        ));
        items.truncate(MAX_ITEMS);
    }

    Ok(ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id: account
                .get("uuid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            address: account
                .get("email_address")
                .and_then(Value::as_str)
                .map(str::to_string),
            display_name: account
                .get("full_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            // Conversations, not messages: claude.ai reports no message
            // count without opening every conversation.
            message_estimate: None,
        },
        items,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An export source has no server to reach, so "test connection"
    /// has to say that rather than report a happy empty account.
    #[test]
    fn refuses_an_export_source() {
        let cfg: ClaudeConfig = toml::from_str("[export]\npath = \"~/claude-export\"\n").unwrap();
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(probe(&cfg))
            .expect_err("an export has no connection to test")
            .to_string();
        assert!(err.contains("no connection"), "{err}");
    }
}
