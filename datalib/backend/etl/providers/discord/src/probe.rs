//! Read-only account probe: who the credential signs in as, and the
//! lists a picker offers (the servers it is in, their channels, its
//! direct messages). No message is read and nothing is stored.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl_discord_config::DiscordConfig;
use datalib_etl_web::http::{HttpRequest, LatchkeySettings};
use datalib_probe::{
    OnProgress, ProbeAccount, ProbeAsk, ProbeItem, ProbeItemKind, ProbeList, ProbeProgress,
    ProbeReport,
};

use crate::ingest::{api, schema_raw, select};

pub async fn probe(
    config: &DiscordConfig,
    ask: ProbeAsk,
    progress: OnProgress<'_>,
) -> Result<ProbeReport> {
    // Not the whole of `validate`: a picker is loaded to fill in what
    // validation asks for.
    config
        .latchkey_settings
        .validate()
        .map_err(anyhow::Error::msg)?;
    let Some(sync) = &config.api else {
        return Err(anyhow!(
            "this discord source has no `api` table, so there is no connection to test. \
             Discord's API is the one way in; set `api` to select it."
        ));
    };
    let latchkey = &config.latchkey_settings;
    let me = send(api::me_request(), latchkey).await?;
    let items = match ask {
        ProbeAsk::Account => Vec::new(),
        ProbeAsk::List(ProbeList::Servers) => {
            server_items(&list(send(api::guilds_request(), latchkey).await?))
        }
        ProbeAsk::List(ProbeList::Channels) => {
            let all = list(send(api::guilds_request(), latchkey).await?);
            let (servers, _) = select::servers(&all, sync.servers.as_deref(), sync.all_servers);
            let mut items = Vec::new();
            for (done, server) in servers.iter().enumerate() {
                progress(ProbeProgress {
                    done: done as u64,
                    total: Some(servers.len() as u64),
                });
                let channels =
                    list(send(api::channels_request(text(server, "id")), latchkey).await?);
                items.extend(channel_items(text(server, "name"), &channels));
            }
            items
        }
        ProbeAsk::List(ProbeList::Conversations) => {
            dm_items(&list(send(api::dms_request(), latchkey).await?))
        }
        ProbeAsk::List(other) => bail!("a Discord source has no `{}` list", other.as_str()),
    };
    Ok(ProbeReport {
        mode: "api".to_string(),
        account: account(&me),
        items,
        notes: Vec::new(),
    })
}

async fn send(req: HttpRequest, latchkey: &LatchkeySettings) -> Result<Value> {
    api::send(req, latchkey).await.map_err(|e| anyhow!("{e}"))
}

fn list(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn label(user: &Value) -> String {
    crate::user_label(
        user.get("global_name").and_then(Value::as_str),
        user.get("username").and_then(Value::as_str),
        text(user, "id"),
    )
}

pub fn account(me: &Value) -> ProbeAccount {
    ProbeAccount {
        id: text(me, "id").to_string(),
        address: me.get("email").and_then(Value::as_str).map(String::from),
        display_name: Some(label(me)),
        message_estimate: None,
    }
}

/// The servers, by name: the ids go in the config, since a name changes.
fn server_items(servers: &[Value]) -> Vec<ProbeItem> {
    let mut items: Vec<ProbeItem> = servers
        .iter()
        .map(|g| ProbeItem {
            title: Some(text(g, "name").to_string()),
            ..ProbeItem::new(text(g, "id"), ProbeItemKind::Server)
        })
        .collect();
    items.sort_by_key(|i| i.title.clone().map(|t| t.to_lowercase()));
    items
}

/// One server's channels and forums, tagged with the server, in the order
/// Discord shows them.
fn channel_items(server: &str, channels: &[Value]) -> Vec<ProbeItem> {
    let mut taken = select::channels(channels, None);
    taken.sort_by_key(|c| c.get("position").and_then(Value::as_i64));
    taken
        .into_iter()
        .map(|c| ProbeItem {
            title: Some(format!("#{}", text(c, "name"))),
            role: Some(server.to_string()),
            ..ProbeItem::new(text(c, "id"), ProbeItemKind::Channel)
        })
        .collect()
}

/// The direct messages, each titled after the people in it.
fn dm_items(channels: &[Value]) -> Vec<ProbeItem> {
    channels
        .iter()
        .map(|c| {
            let people: Vec<String> = c
                .get("recipients")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(label)
                .collect();
            let named = text(c, "name");
            let title = match named.is_empty() {
                true => people.join(", "),
                false => named.to_string(),
            };
            let group = schema_raw::channel_type(c) == schema_raw::GROUP_DM;
            ProbeItem {
                title: Some(title),
                role: group.then(|| "group".to_string()),
                members: group.then_some(people.len() as u64),
                ..ProbeItem::new(text(c, "id"), ProbeItemKind::Conversation)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_account_is_named_by_its_email_and_display_name() {
        let got = account(&json!({"id": "1701", "username": "jlpicard",
            "global_name": "Jean-Luc Picard", "email": "picard@enterprise.org"}));
        assert_eq!(got.id, "1701");
        assert_eq!(got.address.as_deref(), Some("picard@enterprise.org"));
        assert_eq!(got.display_name.as_deref(), Some("Jean-Luc Picard"));
    }

    #[test]
    fn a_picker_writes_ids_and_shows_names() {
        let servers = server_items(&[
            json!({"id": "2", "name": "ten Forward"}),
            json!({"id": "1", "name": "Starfleet"}),
        ]);
        assert_eq!(servers[0].path, "1");
        assert_eq!(servers[1].title.as_deref(), Some("ten Forward"));
        let channels = channel_items(
            "Starfleet",
            &[
                json!({"id": "11", "type": 0, "name": "log", "position": 2}),
                json!({"id": "12", "type": 15, "name": "ideas", "position": 1}),
                json!({"id": "10", "type": 4, "name": "Bridge", "position": 0}),
            ],
        );
        let shown: Vec<_> = channels
            .iter()
            .map(|c| c.title.as_deref().unwrap())
            .collect();
        assert_eq!(shown, ["#ideas", "#log"]);
        let dms = dm_items(&[
            json!({"id": "900", "type": 1, "recipients": [{"id": "2", "username": "wriker"}]}),
            json!({"id": "901", "type": 3, "name": "Poker night", "recipients": [{"id": "2"}, {"id": "3"}]}),
        ]);
        assert_eq!(dms[0].title.as_deref(), Some("wriker"));
        assert_eq!(dms[1].title.as_deref(), Some("Poker night"));
        assert_eq!(
            (dms[1].role.as_deref(), dms[1].members),
            (Some("group"), Some(2))
        );
    }
}
