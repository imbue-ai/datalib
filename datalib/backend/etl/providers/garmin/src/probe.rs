//! Read-only account probe: "do these credentials reach Garmin Connect,
//! and as whom?" One request, the social profile the ingest reads first.
//! Garmin has no list a picker offers.

use anyhow::{bail, Result};
use serde_json::Value;

use datalib_etl_garmin_config::GarminConfig;
use datalib_probe::{ProbeAccount, ProbeAsk, ProbeReport};

use crate::ingest::api::{Fetched, GarminClient};

pub async fn probe(config: &GarminConfig, ask: ProbeAsk) -> Result<ProbeReport> {
    config.validate()?;
    if let ProbeAsk::List(list) = ask {
        bail!("a Garmin source has no `{}` list", list.as_str());
    }
    let client = GarminClient::new(config.latchkey_settings.clone());
    let profile = match client
        .get_json("/userprofile-service/socialProfile")
        .await?
    {
        Fetched::Some(v) => v,
        Fetched::Nothing => bail!("garmin socialProfile answered with nothing"),
    };
    Ok(ProbeReport {
        mode: "api".to_string(),
        account: account_from(&profile),
        items: Vec::new(),
        notes: Vec::new(),
    })
}

fn account_from(profile: &Value) -> ProbeAccount {
    let text = |key: &str| {
        profile[key]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    ProbeAccount {
        id: profile["profileId"]
            .as_i64()
            .map(|n| n.to_string())
            .or_else(|| text("displayName"))
            .unwrap_or_default(),
        address: text("userName").filter(|u| u.contains('@')),
        display_name: text("fullName").or_else(|| text("displayName")),
        message_estimate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_account_is_read_off_the_social_profile() {
        let profile = serde_json::json!({
            "profileId": 1701,
            "displayName": "picard-1701",
            "fullName": "Jean-Luc Picard",
            "userName": "picard@enterprise.test",
        });
        let account = account_from(&profile);
        assert_eq!(account.id, "1701");
        assert_eq!(account.address.as_deref(), Some("picard@enterprise.test"));
        assert_eq!(account.display_name.as_deref(), Some("Jean-Luc Picard"));
    }

    /// A profile with no full name or address still names someone.
    #[test]
    fn a_bare_profile_falls_back_to_its_display_name() {
        let account = account_from(&serde_json::json!({ "displayName": "picard-1701" }));
        assert_eq!(account.id, "picard-1701");
        assert_eq!(account.address, None);
        assert_eq!(account.display_name.as_deref(), Some("picard-1701"));
    }
}
