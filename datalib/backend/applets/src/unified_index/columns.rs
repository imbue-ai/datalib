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
        ColumnSpec::new("created_at", "Created", ColumnType::Datetime).describe(
            "When the thing came into being, as the source wrote it: a message's own \
             stamp; for a document, the earliest moment in it.",
        ),
        // Off by default in the unified grid, where most rows are
        // messages with nothing here; a Browse of one source names it,
        // and there — one row per thread — it is the column that says
        // which are still alive.
        ColumnSpec::new("modified_at", "Modified", ColumnType::Datetime)
            .describe(
                "When it last changed, as the source wrote it: the last message of a \
                 thread, a PR's updated_at, a page's last_edited_time. Empty on a row \
                 not known to have changed since it was created.",
            )
            .hidden(),
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
        // Set only on a diff group's rows; a real source's rows carry
        // null in both, and the grid colours a row off the first.
        ColumnSpec::new("diff_status", "Change", ColumnType::Text)
            .describe(
                "How this row differs between the two commits its diff group compares: \
                 added, removed, modified or unchanged. Empty on every real source's rows.",
            )
            .hidden(),
        ColumnSpec::new("diff_changed_columns", "Changed columns", ColumnType::Text)
            .describe("For a modified row, the columns whose value differs.")
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

    /// The specs name `SearchRow` fields as strings, and the viewer
    /// draws whatever key that string finds — a spec for a field that
    /// was renamed away draws an empty column and nothing complains.
    /// So: every spec names a key the row serializes, holding a value
    /// its type can draw; and every key without a spec is one this
    /// list says is not a column, so a new field has to be placed.
    #[test]
    fn every_column_names_a_wire_field_of_the_type_it_declares() {
        let mut row = SearchRow {
            uuid: "u".into(),
            conversation_uuid: "c".into(),
            markdown_uuid: Some("m".into()),
            message_index: Some(0),
            snippet: "s".into(),
            sender: "who".into(),
            created_at: Some("2026-06-02T13:00:00-07:00".into()),
            modified_at: Some("2026-06-03T09:30:00-07:00".into()),
            is_document: true,
            conversation_name: "n".into(),
            project: "p".into(),
            account: "a".into(),
            org_uuid: "o".into(),
            org_name: "Org".into(),
            entire_chat: "/chat/c".into(),
            source: "Slack".into(),
            provider: "slack".into(),
            provider_ref: None,
            source_ref: None,
            source_id: "slack".into(),
            kind: "k".into(),
            author: "who".into(),
            channel: "#c".into(),
            slack_link: "slack://x".into(),
            source_url: "https://x".into(),
            notion_page_uuid: "n".into(),
            upstream_id: "1".into(),
            upstream_entity_kind: "message".into(),
            byte_size: Some(1),
            item_count: Some(1),
            diff_status: Some("modified".into()),
            diff_changed_columns: Some("text".into()),
            score: Some(0.5),
        };
        Sources::read(Path::new("/nonexistent")).resolve(&mut row);
        let wire = serde_json::to_value(&row).unwrap();
        let wire = wire.as_object().unwrap();

        // Row identity, joins and facets the grid reads without a
        // column of their own.
        const NOT_A_COLUMN: &[&str] = &[
            "uuid",
            "conversation_uuid",
            "markdown_uuid",
            "message_index",
            "sender",
            "entire_chat",
            "source",
            "provider",
            "source_id",
            "org_uuid",
            "slack_link",
            "source_url",
            "notion_page_uuid",
            "upstream_id",
            "upstream_entity_kind",
            "is_document",
        ];
        let specs = columns();
        for spec in &specs {
            let value = wire
                .get(&spec.field)
                .unwrap_or_else(|| panic!("column `{}` names no SearchRow field", spec.field));
            let fits = match spec.r#type {
                ColumnType::Text => value.is_string(),
                ColumnType::Count | ColumnType::Bytes => value.is_i64(),
                ColumnType::Number => value.is_number(),
                ColumnType::Datetime | ColumnType::Timestamp => value
                    .as_str()
                    .is_some_and(|s| datalib_time::validate_iso_offset(s).is_ok()),
                ColumnType::Identity => value.get("id").is_some() && value.get("label").is_some(),
                other => panic!("column `{}`: no rule for {other:?} here yet", spec.field),
            };
            assert!(
                fits,
                "column `{}` is {:?} but the row holds {value}",
                spec.field, spec.r#type
            );
        }
        for key in wire.keys() {
            assert!(
                specs.iter().any(|s| s.field == *key) || NOT_A_COLUMN.contains(&key.as_str()),
                "SearchRow.{key} has no column and is not listed as not-a-column"
            );
        }
        for key in NOT_A_COLUMN {
            assert!(
                wire.contains_key(*key),
                "NOT_A_COLUMN names `{key}`, which is gone"
            );
            assert!(
                !specs.iter().any(|s| s.field == *key),
                "`{key}` is both a column and not one"
            );
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
