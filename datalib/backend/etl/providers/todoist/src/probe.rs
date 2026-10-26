//! Read-only account probe: one request, who the credential signs in
//! as. Todoist has nothing to pick from, so no list is offered. Nothing
//! is stored.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl_todoist_config::TodoistConfig;
use datalib_probe::{ProbeAccount, ProbeAsk, ProbeReport};

use crate::ingest::api;

pub async fn probe(config: &TodoistConfig, ask: ProbeAsk) -> Result<ProbeReport> {
    config.validate()?;
    if config.api.is_none() {
        return Err(anyhow!(
            "this todoist source has no `api` table, so there is no connection to test. \
             Todoist's API is the one way in; set `api` to select it."
        ));
    }
    if let ProbeAsk::List(list) = ask {
        bail!("a Todoist source has no `{}` list", list.as_str());
    }
    let user = api::send(api::user_request(), &config.latchkey_settings)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(report(&user))
}

/// The account as `/user` names it.
pub fn report(user: &Value) -> ProbeReport {
    let s = |k: &str| user.get(k).and_then(Value::as_str).map(String::from);
    let id = match user.get("id") {
        Some(Value::String(id)) => id.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id,
            address: s("email"),
            display_name: s("full_name"),
            message_estimate: None,
        },
        items: Vec::new(),
        notes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_account_is_named_by_its_email_and_full_name() {
        let r = report(&json!({"id": "1701", "email": "picard@enterprise.test",
                               "full_name": "Jean-Luc Picard", "token": "never shown"}));
        assert_eq!(r.account.id, "1701");
        assert_eq!(r.account.address.as_deref(), Some("picard@enterprise.test"));
        assert_eq!(r.account.display_name.as_deref(), Some("Jean-Luc Picard"));
        assert!(r.items.is_empty());
    }
}
