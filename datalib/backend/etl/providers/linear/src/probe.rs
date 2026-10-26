//! Read-only workspace probe. Asked for the account, one request: who
//! the credential signs in as, and in which workspace. Asked for a
//! list, the workspace's teams or projects, as `team` items whose path
//! is the key `teams` takes and `project` items whose path is the name
//! `projects` takes. Nothing is stored.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use datalib_etl_linear_config::LinearConfig;
use datalib_probe::{
    OnProgress, ProbeAccount, ProbeAsk, ProbeItem, ProbeItemKind, ProbeList, ProbeProgress,
    ProbeReport,
};

use crate::ingest::{Api, LinearClient, PageQuery};

pub async fn probe(
    config: &LinearConfig,
    ask: ProbeAsk,
    progress: OnProgress<'_>,
) -> Result<ProbeReport> {
    config.validate()?;
    if config.api.is_none() {
        return Err(anyhow!(
            "this linear source has no `api` table, so there is no connection to test. \
             Linear's API is the one way in; set `api` to select it."
        ));
    }
    let client = LinearClient::new(config.latchkey_settings.clone());
    let who = client.whoami().await.map_err(|e| anyhow!("{e}"))?;
    let items = match ask {
        ProbeAsk::Account => Vec::new(),
        ProbeAsk::List(ProbeList::Teams) => {
            let teams = list(
                &client,
                "teams",
                "id key name archivedAt updatedAt",
                progress,
            )
            .await?;
            team_items(&teams)
        }
        ProbeAsk::List(ProbeList::Projects) => {
            let projects = list(
                &client,
                "projects",
                "id name archivedAt completedAt canceledAt updatedAt",
                progress,
            )
            .await?;
            project_items(&projects)
        }
        ProbeAsk::List(other) => bail!("a Linear source has no `{}` list", other.as_str()),
    };
    let viewer = who.get("viewer");
    let org = field(who.get("organization"), "name");
    let name = field(viewer, "name").or(field(viewer, "displayName"));
    Ok(ProbeReport {
        mode: "api".to_string(),
        account: ProbeAccount {
            id: field(viewer, "id").unwrap_or_default().to_string(),
            address: field(viewer, "email").map(String::from),
            display_name: match (name, org) {
                (Some(n), Some(o)) => Some(format!("{n} ({o})")),
                (n, o) => n.or(o).map(String::from),
            },
            message_estimate: None,
        },
        items,
        notes: Vec::new(),
    })
}

async fn list(
    client: &LinearClient,
    root: &str,
    fields: &str,
    progress: OnProgress<'_>,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = client
            .page(&PageQuery {
                root,
                fields,
                filter_type: "",
                filter: None,
                extra_args: "",
                first: 250,
                after,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        out.extend(page.nodes);
        progress(ProbeProgress {
            done: out.len() as u64,
            total: None,
        });
        if !page.has_next || page.end_cursor.is_none() {
            return Ok(out);
        }
        after = page.end_cursor;
    }
}

fn field<'a>(v: Option<&'a Value>, k: &str) -> Option<&'a str> {
    v.and_then(|v| v.get(k)).and_then(Value::as_str)
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn archived(v: &Value) -> bool {
    !v.get("archivedAt").is_none_or(Value::is_null)
}

/// Teams by key, archived ones last.
pub fn team_items(teams: &[Value]) -> Vec<ProbeItem> {
    let mut items: Vec<(bool, ProbeItem)> = teams
        .iter()
        .filter_map(|t| {
            let mut item = ProbeItem::new(str_of(t, "key")?, ProbeItemKind::Team);
            item.title = str_of(t, "name").map(String::from);
            item.role = archived(t).then(|| "archived".to_string());
            item.updated_at = str_of(t, "updatedAt").map(String::from);
            Some((archived(t), item))
        })
        .collect();
    items.sort_by(|a, b| (a.0, &a.1.path).cmp(&(b.0, &b.1.path)));
    items.into_iter().map(|(_, i)| i).collect()
}

/// Projects by name: the open ones first, most recently updated first.
pub fn project_items(projects: &[Value]) -> Vec<ProbeItem> {
    let mut items: Vec<(bool, ProbeItem)> = projects
        .iter()
        .filter_map(|p| {
            let done = archived(p)
                || !p.get("completedAt").is_none_or(Value::is_null)
                || !p.get("canceledAt").is_none_or(Value::is_null);
            let mut item = ProbeItem::new(str_of(p, "name")?, ProbeItemKind::Project);
            item.role = if archived(p) {
                Some("archived".to_string())
            } else if done {
                Some("closed".to_string())
            } else {
                None
            };
            item.updated_at = str_of(p, "updatedAt").map(String::from);
            Some((done, item))
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.updated_at.cmp(&a.1.updated_at)));
    items.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn teams_are_offered_by_key_with_archived_ones_last() {
        let items = team_items(&[
            json!({"key": "SCI", "name": "Science", "archivedAt": "2026-01-01T00:00:00.000Z"}),
            json!({"key": "ENG", "name": "Engineering", "archivedAt": null}),
        ]);
        let paths: Vec<&str> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["ENG", "SCI"]);
        assert_eq!(items[0].title.as_deref(), Some("Engineering"));
        assert_eq!(items[1].role.as_deref(), Some("archived"));
    }

    #[test]
    fn open_projects_come_first_newest_first() {
        let items = project_items(&[
            json!({"name": "Old", "updatedAt": "2026-01-01T00:00:00.000Z"}),
            json!({"name": "Done", "completedAt": "2026-02-01T00:00:00.000Z",
                   "updatedAt": "2026-09-01T00:00:00.000Z"}),
            json!({"name": "New", "updatedAt": "2026-08-01T00:00:00.000Z"}),
        ]);
        let paths: Vec<&str> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["New", "Old", "Done"]);
        assert_eq!(items[2].role.as_deref(), Some("closed"));
    }
}
