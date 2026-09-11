//! Read-only workspace probe: "do these credentials reach Slack, and
//! which channels and people can this account name?" The same three
//! listing calls a download starts with (`auth.test`, `users.list`,
//! `conversations.list`) and nothing else — no history is fetched.
//! Channels come back as `channel` items whose path is the bare name
//! `channels` takes; the people behind the account's DMs come back as
//! `person` items whose path is the user id `dm_users` takes.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use serde_json::Value;

use datalib_etl::http::LatchkeySettings;
use datalib_etl_slack_config::SlackConfig;
use datalib_source_common::probe::{ProbeAccount, ProbeItem, ProbeItemKind, ProbeReport};

use crate::ingest::api::call_slack;
use crate::ingest::shapes::{M_AUTH_TEST, M_CHANNELS, M_USERS};
use crate::ingest::{conversation_types, next_cursor};

pub async fn probe(config: &SlackConfig) -> Result<ProbeReport> {
    config.validate()?;
    if config.api.is_none() {
        return Err(anyhow!(
            "this slack source has no `api` table, so there is no connection to test. \
             The live Slack API is the one way in; set `api` to select it."
        ));
    }
    let latchkey = &config.latchkey_settings;

    let me = call(M_AUTH_TEST, BTreeMap::new(), latchkey).await?;
    let self_user_id = me.get("user_id").and_then(Value::as_str);

    let users = list_pages(M_USERS, BTreeMap::new(), "members", latchkey).await?;
    let directory: BTreeMap<&str, &Value> = users
        .iter()
        .filter_map(|u| Some((u.get("id")?.as_str()?, u)))
        .collect();

    // Every kind at once, DMs included: the probe is not the place to
    // honour `dms` — the picker for `dm_users` only appears once it is
    // on, and by then the answer has to already be here.
    let mut params = BTreeMap::new();
    params.insert("exclude_archived".to_string(), "true".to_string());
    params.insert("types".to_string(), conversation_types(true).to_string());
    let conversations = list_pages(M_CHANNELS, params, "channels", latchkey).await?;

    let mut items = channel_items(&conversations);
    items.extend(person_items(&conversations, &directory, self_user_id));

    Ok(ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id: self_user_id.unwrap_or_default().to_string(),
            address: None,
            display_name: display_name(&me),
            message_estimate: None,
        },
        items,
        notes: Vec::new(),
    })
}

async fn call(
    method: &str,
    params: BTreeMap<String, String>,
    latchkey: &LatchkeySettings,
) -> Result<Value> {
    Ok(call_slack(method, &params, latchkey)
        .await
        .map_err(|e| anyhow!("{e}"))?
        .response)
}

/// Every page of a cursor-paginated list method, concatenated. The
/// page size matches the downloader's so a playback fixture serves
/// both.
async fn list_pages(
    method: &str,
    mut params: BTreeMap<String, String>,
    field: &str,
    latchkey: &LatchkeySettings,
) -> Result<Vec<Value>> {
    params.insert("limit".to_string(), "200".to_string());
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut p = params.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call(method, p, latchkey).await?;
        if let Some(arr) = resp.get(field).and_then(Value::as_array) {
            out.extend(arr.iter().cloned());
        }
        cursor = next_cursor(&resp);
        if cursor.is_none() {
            return Ok(out);
        }
    }
}

/// "picard in USS Enterprise" — `auth.test` reports the handle and
/// the workspace, and a Slack account has no address to show instead.
fn display_name(auth: &Value) -> Option<String> {
    let user = auth.get("user").and_then(Value::as_str)?;
    match auth.get("team").and_then(Value::as_str) {
        Some(team) => Some(format!("{user} in {team}")),
        None => Some(user.to_string()),
    }
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Channels the account can see, member or not — `channels` names
/// any of them. Ones the account is in sort first, since those are
/// what an unfiltered run would mirror.
fn channel_items(conversations: &[Value]) -> Vec<ProbeItem> {
    let mut items: Vec<(bool, ProbeItem)> = conversations
        .iter()
        .filter(|c| !flag(c, "is_im") && !flag(c, "is_mpim"))
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?;
            let is_member = flag(c, "is_member");
            let mut tags = Vec::new();
            if flag(c, "is_private") {
                tags.push("private");
            }
            if !is_member {
                tags.push("not a member");
            }
            let item = ProbeItem {
                role: (!tags.is_empty()).then(|| tags.join(", ")),
                members: c.get("num_members").and_then(Value::as_u64),
                ..ProbeItem::new(name, ProbeItemKind::Channel)
            };
            Some((is_member, item))
        })
        .collect();
    items.sort_by(|(a_member, a), (b_member, b)| {
        b_member.cmp(a_member).then_with(|| a.path.cmp(&b.path))
    });
    items.into_iter().map(|(_, item)| item).collect()
}

/// Everyone on the other end of a DM: the counterpart of each 1:1, and
/// every member of each group DM but the account itself. One item per
/// person however many conversations they appear in, named from the
/// user directory where it knows them.
fn person_items(
    conversations: &[Value],
    directory: &BTreeMap<&str, &Value>,
    self_user_id: Option<&str>,
) -> Vec<ProbeItem> {
    let mut people: BTreeMap<String, ProbeItem> = BTreeMap::new();
    for c in conversations {
        let ids: Vec<&str> = if flag(c, "is_im") {
            c.get("user").and_then(Value::as_str).into_iter().collect()
        } else if flag(c, "is_mpim") {
            c.get("members")
                .and_then(Value::as_array)
                .map(|m| m.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default()
        } else {
            continue;
        };
        for id in ids {
            if Some(id) == self_user_id || people.contains_key(id) {
                continue;
            }
            let user = directory.get(id).copied();
            people.insert(
                id.to_string(),
                ProbeItem {
                    title: user.and_then(real_name).map(str::to_string),
                    role: user
                        .and_then(|u| u.get("name"))
                        .and_then(Value::as_str)
                        .map(|handle| format!("@{handle}")),
                    ..ProbeItem::new(id, ProbeItemKind::Person)
                },
            );
        }
    }
    let mut items: Vec<ProbeItem> = people.into_values().collect();
    items.sort_by(|a, b| {
        a.title
            .is_none()
            .cmp(&b.title.is_none())
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.path.cmp(&b.path))
    });
    items
}

/// The same first choice [`crate::user_label`] makes, read off the
/// wire shape rather than a stored row.
fn real_name(user: &Value) -> Option<&str> {
    user.get("real_name")
        .and_then(Value::as_str)
        .or_else(|| user.get("profile")?.get("real_name")?.as_str())
        .filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conversations() -> Vec<Value> {
        vec![
            json!({"id": "C2", "name": "zulu", "is_member": true, "num_members": 4}),
            json!({"id": "C3", "name": "alpha", "is_member": false}),
            json!({"id": "C1", "name": "bridge", "is_member": true, "is_private": true}),
            json!({"id": "D1", "is_im": true, "user": "U_RIKER"}),
            json!({"id": "D2", "is_im": true, "user": "U_STRANGER"}),
            json!({"id": "G1", "is_mpim": true, "members": ["U_ME", "U_RIKER", "U_WORF"]}),
        ]
    }

    fn directory() -> Vec<Value> {
        vec![
            json!({"id": "U_ME", "name": "picard", "real_name": "Jean-Luc Picard"}),
            json!({"id": "U_RIKER", "name": "riker", "profile": {"real_name": "William Riker"}}),
            json!({"id": "U_WORF", "name": "worf"}),
        ]
    }

    #[test]
    fn channels_are_named_tagged_and_members_first() {
        let items = channel_items(&conversations());
        let rows: Vec<(&str, Option<&str>)> = items
            .iter()
            .map(|i| (i.path.as_str(), i.role.as_deref()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("bridge", Some("private")),
                ("zulu", None),
                ("alpha", Some("not a member")),
            ]
        );
        assert!(items.iter().all(|i| i.kind == ProbeItemKind::Channel));
        assert_eq!(items[1].members, Some(4));
    }

    /// One row per person, the account itself left out of its own
    /// group DM, and someone the directory doesn't know still listed
    /// under their id so the DM isn't silently unpickable.
    #[test]
    fn people_are_the_far_end_of_every_dm() {
        let users = directory();
        let dir: BTreeMap<&str, &Value> = users
            .iter()
            .map(|u| (u["id"].as_str().unwrap(), u))
            .collect();
        let items = person_items(&conversations(), &dir, Some("U_ME"));
        let rows: Vec<(&str, Option<&str>, Option<&str>)> = items
            .iter()
            .map(|i| (i.path.as_str(), i.title.as_deref(), i.role.as_deref()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("U_RIKER", Some("William Riker"), Some("@riker")),
                ("U_STRANGER", None, None),
                ("U_WORF", None, Some("@worf")),
            ]
        );
        assert!(items.iter().all(|i| i.kind == ProbeItemKind::Person));
    }

    #[test]
    fn account_reads_as_handle_in_workspace() {
        assert_eq!(
            display_name(&json!({"user": "picard", "team": "Enterprise"})).as_deref(),
            Some("picard in Enterprise")
        );
        assert_eq!(display_name(&json!({"team": "Enterprise"})), None);
    }

    /// A config with no `api` table selects no method at all, so
    /// "test connection" has to say that rather than dial Slack with
    /// nothing selected.
    #[test]
    fn refuses_a_source_with_no_api_table() {
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(probe(&SlackConfig::default()))
            .expect_err("nothing to test")
            .to_string();
        assert!(err.contains("no connection"), "{err}");
    }
}
