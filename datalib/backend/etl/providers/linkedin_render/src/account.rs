//! Whose export this is — the `account` column on every LinkedIn row.
//! An export names its owner in `Email Addresses.csv` (one row flagged
//! `Primary`) and `Profile.csv`; nothing in `Connections.csv`,
//! `messages.csv` or `Shares.csv` does.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use datalib_etl_linkedin::ingest::{db_path_for, RawDb};

pub fn load_account(raw_dir: &Path) -> Result<Option<String>> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(None);
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open_reader(&db_path).await?;
            let Some(pin) = datalib_etl::pin::head(db.pool()).await? else {
                db.close().await;
                return Ok(None);
            };
            datalib_etl::pin::install_views(db.pool(), &pin).await?;
            // Either file can be absent from an export; a missing table
            // is "unknown", not a failed render.
            let emails = db
                .load_payloads(datalib_etl::pin::Reads::At(&pin), "email_addresses")
                .await
                .unwrap_or_default();
            let profile = db
                .load_payloads(datalib_etl::pin::Reads::At(&pin), "profile")
                .await
                .unwrap_or_default();
            db.close().await;
            Ok(account_label(&emails, &profile))
        })
    })
}

/// The primary address (else the first listed), else the profile's
/// name. `None` when the export carries neither file.
pub fn account_label(email_addresses: &[Value], profile: &[Value]) -> Option<String> {
    let field = |p: &Value, k: &str| -> Option<String> {
        p.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let is_primary = |p: &Value| field(p, "Primary").is_some_and(|v| v.eq_ignore_ascii_case("yes"));
    let email = email_addresses
        .iter()
        .find(|p| is_primary(p))
        .or_else(|| email_addresses.first())
        .and_then(|p| field(p, "Email Address"));
    let name = profile.first().and_then(|p| {
        let parts: Vec<String> = ["First Name", "Last Name"]
            .iter()
            .filter_map(|k| field(p, k))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    });
    datalib_etl_chat_common::account_label("", email.as_deref(), name.as_deref())
}

#[cfg(test)]
mod tests {
    use super::account_label;
    use serde_json::json;

    #[test]
    fn primary_address_wins_over_listing_order() {
        let emails = [
            json!({"Email Address": "old@x.test", "Primary": "No"}),
            json!({"Email Address": "me@x.test", "Primary": "Yes"}),
        ];
        assert_eq!(account_label(&emails, &[]).as_deref(), Some("me@x.test"));
    }

    #[test]
    fn falls_back_to_the_first_address_then_the_profile_name() {
        let emails = [json!({"Email Address": "only@x.test", "Primary": "No"})];
        assert_eq!(account_label(&emails, &[]).as_deref(), Some("only@x.test"));
        let profile = [json!({"First Name": "Jean-Luc", "Last Name": "Picard"})];
        assert_eq!(
            account_label(&[], &profile).as_deref(),
            Some("Jean-Luc Picard")
        );
        assert_eq!(account_label(&[], &[]), None);
    }
}
