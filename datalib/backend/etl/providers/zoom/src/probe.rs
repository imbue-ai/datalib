//! Read-only connection check: which Zoom user the sign-in and
//! `api.user` together name, the same way a sync works it out. Nothing
//! is stored, and there is no list to pick from.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl_zoom_config::ZoomConfig;
use datalib_probe::{ProbeAccount, ProbeAsk, ProbeReport};

use crate::ingest::api;

pub async fn probe(config: &ZoomConfig, ask: ProbeAsk) -> Result<ProbeReport> {
    config.validate()?;
    let Some(zoom) = &config.api else {
        return Err(anyhow!(
            "this zoom source has no `api` table, so there is no connection to test. \
             Zoom's API is the one way in; set `api` to select it."
        ));
    };
    if let ProbeAsk::List(list) = ask {
        bail!("a Zoom source has no `{}` list", list.as_str());
    }
    let user = api::resolve_user(zoom.user.as_deref(), &config.latchkey_settings).await?;
    Ok(report(&user))
}

fn report(user: &Value) -> ProbeReport {
    let s = |k: &str| {
        user.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let name = match (s("first_name"), s("last_name")) {
        (Some(f), Some(l)) => Some(format!("{f} {l}")),
        (f, l) => f.or(l).map(String::from),
    }
    .or_else(|| s("display_name").map(String::from));
    ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id: s("id").unwrap_or_default().to_string(),
            address: s("email").map(String::from),
            display_name: name,
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
    fn the_report_names_the_user_by_name_and_email() {
        let r = report(
            &json!({"id": "U1", "first_name": "Jean-Luc", "last_name": "Picard",
                               "email": "picard@enterprise.example"}),
        );
        assert_eq!(r.account.id, "U1");
        assert_eq!(
            r.account.address.as_deref(),
            Some("picard@enterprise.example")
        );
        assert_eq!(r.account.display_name.as_deref(), Some("Jean-Luc Picard"));
    }
}
