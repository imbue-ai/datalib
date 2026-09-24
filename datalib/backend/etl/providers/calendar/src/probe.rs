//! "Test connection" for a calendar source: do these credentials reach
//! the account, and which calendars can `calendars` name? The listing a
//! download starts with and nothing more — no event is fetched.

use anyhow::{anyhow, Result};
use datalib_etl_calendar_config::{CalendarConfig, CalendarMethod};
use datalib_probe::{ProbeAccount, ProbeItem, ProbeItemKind, ProbeReport};
use serde_json::Value;

use crate::ingest::schema_raw::CalendarRow;
use crate::ingest::{caldav, google, FetchSummary};

pub async fn probe(config: &CalendarConfig) -> Result<ProbeReport> {
    config.validate()?;
    let lk = &config.latchkey_settings;
    let mut summary = FetchSummary::default();
    match config.method()? {
        CalendarMethod::Google { .. } => {
            let list = google::list_calendars(lk, &mut summary).await?;
            let login = google::primary_id(&list);
            let rows: Vec<CalendarRow> = list.iter().filter_map(google::calendar_row).collect();
            let roles: Vec<Option<String>> =
                rows.iter().map(|r| google_role(&list, &r.id)).collect();
            Ok(report("google", login, &rows, &roles))
        }
        CalendarMethod::Caldav { server_url, .. } => {
            let reached = caldav::reach(server_url, lk, &mut summary).await?;
            let rows: Vec<CalendarRow> = reached.calendars.into_iter().map(|c| c.row).collect();
            let login = reached.found.login.or(Some(reached.account_id));
            let method = if config.fastmail.is_some() {
                "fastmail"
            } else {
                "caldav"
            };
            Ok(report(method, login, &rows, &vec![None; rows.len()]))
        }
        CalendarMethod::Ics(_) => Err(anyhow!(
            "an `ics` source reads files on disk, so there is no connection to test"
        )),
    }
}

/// Google's own words for how the account holds a calendar: its own
/// (`primary`), one it may edit, or one it only reads.
fn google_role(list: &[Value], id: &str) -> Option<String> {
    let c = list
        .iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(id))?;
    if c.get("primary").and_then(Value::as_bool) == Some(true) {
        return Some("primary".into());
    }
    match c.get("accessRole").and_then(Value::as_str)? {
        "reader" | "freeBusyReader" => Some("read-only".into()),
        _ => None,
    }
}

/// One item per calendar, named the way `calendars` matches: by its
/// name where it has one, else its id, sorted by name.
fn report(
    method: &str,
    login: Option<String>,
    rows: &[CalendarRow],
    roles: &[Option<String>],
) -> ProbeReport {
    let mut items: Vec<ProbeItem> = rows
        .iter()
        .zip(roles)
        .map(|(r, role)| {
            let name = r.display_name.clone();
            ProbeItem {
                title: name.as_ref().filter(|n| **n != r.id).map(|_| r.id.clone()),
                role: role.clone(),
                ..ProbeItem::new(
                    name.unwrap_or_else(|| r.id.clone()),
                    ProbeItemKind::Calendar,
                )
            }
        })
        .collect();
    items.sort_by_key(|i| i.path.to_lowercase());
    ProbeReport {
        mode: method.to_string(),
        account: ProbeAccount {
            id: login.clone().unwrap_or_default(),
            address: login.filter(|l| l.contains('@')),
            display_name: None,
            message_estimate: None,
        },
        items,
        notes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_named_the_way_the_filter_matches() {
        let rows = vec![
            CalendarRow {
                id: "2c1f-bridge".into(),
                display_name: Some("Bridge".into()),
                ..Default::default()
            },
            CalendarRow {
                id: "away-team".into(),
                display_name: None,
                ..Default::default()
            },
        ];
        let r = report(
            "fastmail",
            Some("picard@enterprise.test".into()),
            &rows,
            &[None, None],
        );
        let paths: Vec<&str> = r.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["away-team", "Bridge"]);
        assert_eq!(r.items[1].title.as_deref(), Some("2c1f-bridge"));
        assert_eq!(r.items[0].title, None, "a bare id needs no second name");
        assert_eq!(r.account.address.as_deref(), Some("picard@enterprise.test"));
    }
}
