//! What the search grid's columns are, and the two identities the
//! applet resolves before a row goes out: the provider as the
//! configured source's own mark (Gmail rather than Mail, when the
//! config says which) and the source as the group's name rather than
//! its id. Both read `config.toml` — the names live there and nowhere
//! in the index, which is what keeps renaming a source free of a
//! re-index.

use std::collections::HashMap;
use std::path::Path;

use datalib_columns::{source_catalog, ColumnSpec, ColumnType, Identity};
use datalib_unified_index::db::datalib_source_id;
use datalib_unified_index::search::SearchRow;

pub fn columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("score", "Score", ColumnType::Number).describe(
            "How well the row matched a free-text search. Not comparable across searches.",
        ),
        ColumnSpec::new("provider_ref", "Provider", ColumnType::Identity).describe(
            "Which service this came from. A property of the source's type — two Slack \
             workspaces share it; the Source column is what separates them.",
        ),
        ColumnSpec::new("source_ref", "Source", ColumnType::Identity).describe(
            "The configured source this row came from — its id is its directory under the \
             data root; the cell shows the name config.toml gives it. Datalib's own rows, \
             like a source's storage report, say Datalib rather than the source they describe.",
        ),
        ColumnSpec::new("kind", "Type", ColumnType::Text),
        ColumnSpec::new("conversation_name", "Conversation", ColumnType::Text).hidden(),
        ColumnSpec::new("project", "Project", ColumnType::Text)
            .describe(
                "What the source calls a grouping above the conversation: a Claude project, \
                 a GitHub repo, a GitLab project path.",
            )
            .hidden(),
        ColumnSpec::new("channel", "Channel", ColumnType::Text),
        ColumnSpec::new("when", "Time", ColumnType::Datetime),
        ColumnSpec::new("snippet", "Contents", ColumnType::Text),
        ColumnSpec::new("author", "Author", ColumnType::Text),
        ColumnSpec::new("account", "Account", ColumnType::Text).hidden(),
        ColumnSpec::new("org_name", "Org", ColumnType::Text).hidden(),
        ColumnSpec::new("byte_size", "Size", ColumnType::Bytes).hidden(),
        ColumnSpec::new("item_count", "Items", ColumnType::Count)
            .describe(
                "What is being counted depends on the row's Type: rows for a Table, files \
                 for a Source Size, pages for a PDF.",
            )
            .hidden(),
    ]
}

/// What the config says about each group: its name, its type, and its
/// ingest step's params (which narrow the type to a variant).
#[derive(Default)]
pub struct Sources {
    groups: HashMap<String, Group>,
}

struct Group {
    name: Option<String>,
    r#type: Option<String>,
    ingest_params: serde_json::Value,
}

impl Sources {
    /// Read leniently: a config the loader would reject still names its
    /// groups, and a name is all this needs. A file that is not TOML
    /// at all, or is absent, resolves nothing.
    pub fn read(root: &Path) -> Sources {
        let text = std::fs::read_to_string(root.join("config.toml")).unwrap_or_default();
        let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
            return Sources::default();
        };
        let tables = |key: &str| -> Vec<&toml::Value> {
            doc.get(key)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter(|v| v.is_table()).collect())
                .unwrap_or_default()
        };
        let s = |v: &toml::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let mut groups: HashMap<String, Group> = tables("groups")
            .into_iter()
            .filter_map(|v| {
                let id = s(v, "id")?;
                let name = s(v, "name")
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty());
                Some((
                    id,
                    Group {
                        name,
                        r#type: s(v, "type"),
                        ingest_params: serde_json::Value::Null,
                    },
                ))
            })
            .collect();
        for v in tables("steps") {
            if s(v, "function").as_deref() != Some("ingest") {
                continue;
            }
            let Some(g) = s(v, "group").and_then(|g| groups.get_mut(&g)) else {
                continue;
            };
            g.ingest_params = v
                .get("params")
                .and_then(|p| serde_json::to_value(p).ok())
                .unwrap_or(serde_json::Value::Null);
        }
        Sources { groups }
    }

    pub fn resolve(&self, row: &mut SearchRow) {
        let group = self.groups.get(&row.source_id);
        // The configured source's own mark first — Gmail and Fastmail are
        // both provider "Mail", and only the config knows which this is.
        // Failing that, the provider tag names its own mark.
        let catalog = group
            .and_then(|g| g.r#type.as_deref().map(|t| (t, &g.ingest_params)))
            .map(|(t, params)| source_catalog::source_type(t, params));
        row.provider_ref = Some(Identity {
            id: row.provider.clone(),
            label: catalog
                .as_ref()
                .map(|c| c.label.clone())
                .unwrap_or_else(|| row.source.clone()),
            icon: catalog
                .as_ref()
                .and_then(|c| c.icon.clone())
                .or_else(|| Some(row.provider.clone()).filter(|p| !p.is_empty())),
            detail: None,
        });
        // Datalib's own rows — each source's storage report — are filed
        // under datalib rather than under the source they measure.
        let label = if row.source_id == datalib_source_id() {
            "Datalib".to_string()
        } else {
            group
                .and_then(|g| g.name.clone())
                .unwrap_or_else(|| row.source_id.clone())
        };
        row.source_ref = Some(Identity {
            id: row.source_id.clone(),
            label,
            icon: None,
            detail: Some(if row.source_id == datalib_source_id() {
                "Datalib's own row, not a source's data".to_string()
            } else {
                format!("Stored in {}/", row.source_id)
            }),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(provider: &str, source: &str, source_id: &str) -> SearchRow {
        SearchRow {
            provider: provider.into(),
            source: source.into(),
            source_id: source_id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_configured_source_narrows_the_provider_and_names_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
[[groups]]
id = "work-mail"
name = "Work mail"
type = "email"

[[steps]]
group = "work-mail"
function = "ingest"
[steps.params.gmail]
account = "x"
"#,
        )
        .unwrap();
        let sources = Sources::read(tmp.path());
        let mut r = row("email", "Mail", "work-mail");
        sources.resolve(&mut r);
        let p = r.provider_ref.unwrap();
        assert_eq!(
            (p.label.as_str(), p.icon.as_deref()),
            ("Gmail", Some("gmail"))
        );
        let s = r.source_ref.unwrap();
        assert_eq!(
            (s.id.as_str(), s.label.as_str()),
            ("work-mail", "Work mail")
        );
    }

    #[test]
    fn an_unconfigured_source_falls_back_to_the_provider_and_the_id() {
        let sources = Sources::read(Path::new("/nonexistent"));
        let mut r = row("slack", "Slack", "old-slack");
        sources.resolve(&mut r);
        let p = r.provider_ref.unwrap();
        assert_eq!(
            (p.label.as_str(), p.icon.as_deref()),
            ("Slack", Some("slack"))
        );
        assert_eq!(r.source_ref.unwrap().label, "old-slack");
        let mut d = row("datalib", "Datalib", "datalib");
        sources.resolve(&mut d);
        assert_eq!(d.source_ref.unwrap().label, "Datalib");
    }
}
