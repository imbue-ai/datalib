//! Read-only account probe: one request, `GET /user`, which says who the
//! credential signs in as. GitHub offers no list to pick from. Nothing is
//! stored.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl_github_config::GithubConfig;
use datalib_probe::{ProbeAccount, ProbeAsk, ProbeReport};

pub async fn probe(config: &GithubConfig, ask: ProbeAsk) -> Result<ProbeReport> {
    config.validate()?;
    if config.api.is_none() {
        return Err(anyhow!(
            "this github source has no `api` table, so there is no connection to test. \
             GitHub's API is the one way in; set `api` to select it."
        ));
    }
    if let ProbeAsk::List(list) = ask {
        bail!("a GitHub source has no `{}` list", list.as_str());
    }
    let me = crate::ingest::signed_in_user(&config.latchkey_settings)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(report(&me))
}

/// The account as `/user` names it. A GitHub account goes by its login,
/// which is also the name latchkey stores the credential under, so the
/// login is always shown. `/user`'s `email` is whichever address the
/// person chose to make public, usually none, so it does not name them.
pub fn report(me: &Value) -> ProbeReport {
    let at = |key: &str| {
        me.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let login = at("login");
    ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id: login.unwrap_or_default().to_string(),
            address: None,
            display_name: match (at("name"), login) {
                (Some(name), Some(login)) => Some(format!("{name} ({login})")),
                (name, login) => name.or(login).map(String::from),
            },
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
    fn an_account_is_named_by_its_name_and_login() {
        let r = report(&json!({"id": 17010001, "login": "jlpicard",
                               "name": "Jean-Luc Picard",
                               "email": "picard@enterprise.test"}));
        assert_eq!(r.account.id, "jlpicard");
        assert_eq!(r.account.address, None);
        assert_eq!(
            r.account.display_name.as_deref(),
            Some("Jean-Luc Picard (jlpicard)")
        );
        assert!(r.items.is_empty());
    }

    /// GitHub answers `"name": null` for an account that never set one.
    #[test]
    fn an_account_with_no_name_goes_by_its_login() {
        let r = report(&json!({"login": "jlpicard", "name": null, "email": null}));
        assert_eq!(r.account.display_name.as_deref(), Some("jlpicard"));
    }

    #[tokio::test]
    async fn a_list_is_refused_before_any_request() {
        let config = GithubConfig {
            api: Some(Default::default()),
            ..Default::default()
        };
        let err = probe(&config, ProbeAsk::List(datalib_probe::ProbeList::Channels))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `channels` list"), "{err}");
    }

    #[tokio::test]
    async fn a_source_with_no_api_table_has_no_connection_to_test() {
        let err = probe(&GithubConfig::default(), ProbeAsk::Account)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `api` table"), "{err}");
    }
}
