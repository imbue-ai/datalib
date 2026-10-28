//! Read-only account probe: one request, `GET /v1/users/me`, which says
//! who the token acts as and in which workspace. It also proves the
//! stored credential carries `Notion-Version`, since Notion refuses every
//! request without it. Notion offers no list to pick from. Nothing is
//! stored.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl_notion_config::NotionConfig;
use datalib_probe::{ProbeAccount, ProbeAsk, ProbeReport};

use crate::ingest::official::NotionOfficialClient;

pub async fn probe(config: &NotionConfig, ask: ProbeAsk) -> Result<ProbeReport> {
    config.validate()?;
    if config.api.is_none() {
        return Err(anyhow!(
            "this notion source has no `api` table, so there is no connection to test. \
             Notion's API is the one way in; set `api` to select it."
        ));
    }
    if let ProbeAsk::List(list) = ask {
        bail!("a Notion source has no `{}` list", list.as_str());
    }
    let client = NotionOfficialClient::with_latchkey(config.latchkey_settings.clone());
    let me = client
        .get_user("me")
        .await
        .map_err(|e| anyhow!("{}", explain(&e.to_string())))?;
    Ok(report(&me))
}

/// A refusal over the version header is the one mistake a person can make
/// pasting the token by hand, and Notion's own words for it do not say
/// what to do.
fn explain(error: &str) -> String {
    if error.contains("Notion-Version") {
        format!(
            "{error}\nThe stored notion credential must carry exactly one `Notion-Version` \
             header beside the token; paste the token again in the Add source form, which \
             stores both."
        )
    } else {
        error.to_string()
    }
}

/// The account as `/users/me` names it. A personal access token answers
/// as a bot owned by a person, and that person is the account; an
/// internal integration is owned by the workspace and sees only the
/// pages shared with it, which is worth saying.
pub fn report(me: &Value) -> ProbeReport {
    let at = |path: &str| me.pointer(path).and_then(Value::as_str);
    let owner_is_person = at("/bot/owner/type") == Some("user");
    let workspace = at("/bot/workspace_name");
    let (id, name) = if owner_is_person {
        (at("/bot/owner/user/id"), at("/bot/owner/user/name"))
    } else {
        (at("/id"), at("/name"))
    };
    let mut notes = Vec::new();
    if at("/bot/owner/type") == Some("workspace") {
        notes.push(
            "This token is an internal integration: it copies only the pages shared with it \
             (⋯ → Connections in Notion). A personal access token sees everything you can."
                .to_string(),
        );
    }
    ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id: id.unwrap_or_default().to_string(),
            address: owner_is_person
                .then(|| at("/bot/owner/user/person/email"))
                .flatten()
                .map(String::from),
            display_name: match (name, workspace) {
                (Some(n), Some(w)) => Some(format!("{n} ({w})")),
                (n, w) => n.or(w).map(String::from),
            },
            message_estimate: None,
        },
        items: Vec::new(),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_personal_token_is_named_by_its_owner_and_workspace() {
        let r = report(&json!({
            "object": "user", "id": "bot-1", "name": "datalib", "type": "bot",
            "bot": {
                "owner": {"type": "user", "user": {
                    "object": "user", "id": "user-1", "name": "Jean-Luc Picard", "type": "person",
                    "person": {"email": "picard@enterprise.test"}}},
                "workspace_name": "Enterprise", "workspace_id": "ws-1"}
        }));
        assert_eq!(r.account.id, "user-1");
        assert_eq!(r.account.address.as_deref(), Some("picard@enterprise.test"));
        assert_eq!(
            r.account.display_name.as_deref(),
            Some("Jean-Luc Picard (Enterprise)")
        );
        assert!(r.notes.is_empty());
    }

    #[test]
    fn an_internal_integration_says_it_sees_only_shared_pages() {
        let r = report(&json!({
            "object": "user", "id": "bot-1", "name": "Ops Mirror", "type": "bot",
            "bot": {"owner": {"type": "workspace", "workspace": true},
                    "workspace_name": "Enterprise"}
        }));
        assert_eq!(r.account.id, "bot-1");
        assert_eq!(r.account.address, None);
        assert_eq!(
            r.account.display_name.as_deref(),
            Some("Ops Mirror (Enterprise)")
        );
        assert_eq!(r.notes.len(), 1);
    }

    #[test]
    fn a_version_refusal_says_to_paste_again() {
        let e = explain(
            r#"GET /users/me: HTTP 400 body="{\"code\":\"missing_version\",\"message\":\"Notion-Version header failed validation\"}""#,
        );
        assert!(e.contains("paste the token again"), "{e}");
        assert_eq!(
            explain("GET /users/me -> HTTP 401"),
            "GET /users/me -> HTTP 401"
        );
    }
}
