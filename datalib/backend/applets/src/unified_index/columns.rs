//! What the search grid's columns are, and the source identity the
//! applet resolves before a row goes out: the group's name rather than
//! its id, led by the configured source's own mark (Gmail rather than
//! Mail, when the config says which) — the Manage screen's Name cell.
//! It reads `config.toml`: the names live there and nowhere in the
//! index, which is what keeps renaming a source free of a re-index.

use std::collections::HashMap;
use std::path::Path;

use datalib_columns::{
    source_catalog, ColumnSearch, ColumnSpec, ColumnType, DocumentLink, Entity, FreeTextMatch,
    Identity, KeyValues, RowsSpec, SearchKeySpec, ValueSuggestion,
};
use datalib_handle::{Handle, HandleKind};
use datalib_query::table::{Column, FreeText, SearchTable};
use datalib_schema::grid_rows::{GridRow, GridRowColumn};
use datalib_unified_index::db::datalib_source_id;
use datalib_unified_index::grid_columns::GridColumn;
use datalib_unified_index::search::SearchRow;
use datalib_unified_index::terms_keys::TERMS_KEYS;
use datalib_unified_index::view::{self, View};
use serde::Deserialize;

/// The Author cell (docs/dev/chips.md § "In a grid"): the handle
/// as a URI for its id, so the viewer resolves it as it does a chip link
/// in a document, the name the source showed for its label, and the
/// handle kind's mark. A row with neither has no author to draw.
pub fn author_identity(author: &str, handle: Option<&str>) -> Option<Identity> {
    let handle = handle.and_then(Handle::parse);
    if author.is_empty() && handle.is_none() {
        return None;
    }
    let label = match (&handle, author.is_empty()) {
        (Some(h), true) => h.value().to_string(),
        _ => author.to_string(),
    };
    Some(Identity {
        id: handle
            .as_ref()
            .map_or_else(|| author.to_string(), Handle::to_uri),
        label,
        icon: handle.as_ref().map(|h| handle_mark(h.kind()).to_string()),
        detail: handle.as_ref().map(|h| h.describe(author)),
        entity: None,
    })
}

/// The icon token for a handle kind; `KIND_ICON` in `ui/src/cards/contacts.ts`
/// is the same table.
fn handle_mark(kind: HandleKind) -> &'static str {
    match kind {
        HandleKind::Email => "email",
        HandleKind::Tel => "sms",
        HandleKind::Slack => "slack",
        HandleKind::SignalAci => "signal",
    }
}

pub fn columns() -> Vec<ColumnSpec> {
    searchable::<GridColumn>(declared())
}

/// A search row opens its document at itself; a row with no document
/// named opens as one.
pub fn rows_spec() -> RowsSpec {
    use GridRowColumn as G;
    RowsSpec {
        row_key: G::Uuid.as_str(),
        document: DocumentLink {
            fields: &["markdown_uuid", "uuid"],
            anchor: "uuid",
        },
        free_text: free_text_of::<GridRow>(),
    }
}

pub fn free_text_of<T: SearchTable>() -> FreeTextMatch {
    match T::FREE_TEXT {
        FreeText::Qmd => FreeTextMatch::Qmd,
        FreeText::Like(_) => FreeTextMatch::Like,
    }
}

/// Every key `T`'s search reads (`datalib_unified_index::query`), with
/// what its values are.
pub fn keys_of<T: SearchTable>() -> Vec<SearchKeySpec> {
    let key = |key, values| SearchKeySpec {
        key,
        aliases: &[],
        values,
    };
    let mut keys: Vec<SearchKeySpec> = T::KEYS
        .iter()
        .map(|k| SearchKeySpec {
            key: k.key,
            aliases: k.aliases,
            values: match k.vocabulary {
                Some(words) => KeyValues::Words { words: words() },
                // Every table that files its rows under a source names
                // the column so (AGENTS.md, "A source's id is not its name").
                None if k.column.as_str() == "source_id" => KeyValues::Source,
                None => KeyValues::Text,
            },
        })
        .collect();
    if T::RANGE.is_some() {
        keys.push(key("before", KeyValues::Stamp));
        keys.push(key("after", KeyValues::Stamp));
    }
    if !T::FLAGS.is_empty() {
        let words = T::FLAGS.iter().map(|(word, _)| *word).collect();
        keys.push(key("is", KeyValues::Words { words }));
    }
    if matches!(T::FREE_TEXT, FreeText::Qmd) {
        keys.push(key("qmd", KeyValues::Text));
        keys.push(key("qmd_vsearch", KeyValues::Text));
    }
    keys
}

/// The grid's keys: its columns' and those that read the search terms.
pub fn grid_keys() -> Vec<SearchKeySpec> {
    let mut keys = keys_of::<GridRow>();
    keys.extend(TERMS_KEYS.iter().map(|k| SearchKeySpec {
        key: k.key,
        aliases: k.aliases,
        values: if k.person {
            KeyValues::Person
        } else {
            KeyValues::Text
        },
    }));
    keys
}

/// `…/values?key=…&typed=…&q=…`: one key's values holding `typed`,
/// among the rows the rest of the query `q` keeps.
#[derive(Debug, Deserialize)]
pub struct ValuesParams {
    pub key: String,
    #[serde(default)]
    pub typed: String,
    #[serde(default)]
    pub q: String,
}

/// Where a key's values come from.
pub enum ValueSource<C> {
    /// Its closed set.
    Words(Vec<&'static str>),
    /// The column's values among the rows.
    Column(C),
    /// Nothing to offer: a date, the words free text routes to qmd, a key
    /// the table does not have.
    Nothing,
}

pub fn value_source<T: SearchTable>(key: &str) -> ValueSource<T::Column> {
    if key == "is" {
        return ValueSource::Words(T::FLAGS.iter().map(|(word, _)| *word).collect());
    }
    match datalib_query::table::key::<T>(key) {
        Some(k) => match k.vocabulary {
            Some(words) => ValueSource::Words(words()),
            None => ValueSource::Column(k.column),
        },
        None => ValueSource::Nothing,
    }
}

/// The words holding `typed`, case-blind, in their own order.
pub fn words_holding(words: &[&'static str], typed: &str) -> Vec<ValueSuggestion> {
    let typed = typed.to_lowercase();
    words
        .iter()
        .filter(|w| w.to_lowercase().contains(&typed))
        .map(|w| ValueSuggestion {
            value: (*w).to_string(),
            count: None,
        })
        .collect()
}

pub fn counted(values: Vec<(String, u64)>) -> Vec<ValueSuggestion> {
    values
        .into_iter()
        .map(|(value, count)| ValueSuggestion {
            value,
            count: Some(count),
        })
        .collect()
}

/// The search bar is a grid's one filter: each column says which of its
/// keys filters its cells.
pub fn searchable<V: View>(mut columns: Vec<ColumnSpec>) -> Vec<ColumnSpec> {
    for c in &mut columns {
        c.search = view::for_column::<V>(&c.field).map(|(key, field)| ColumnSearch {
            key: key.into(),
            field: field.into(),
        });
    }
    columns
}

fn declared() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("score", "Score", ColumnType::Number).describe(
            "How well the row matched a free-text search. Not comparable across searches.",
        ),
        ColumnSpec::new("source_ref", "Source", ColumnType::Identity).describe(
            "The configured source this row came from, led by the mark of the service it \
             mirrors — its id is its directory under the data root; the cell shows the name \
             config.toml gives it. Datalib's own rows, like a source's storage report, say \
             Datalib rather than the source they describe.",
        ),
        // Second, so what a row says is on screen at any width.
        ColumnSpec::new("snippet", "Contents", ColumnType::Text),
        ColumnSpec::new("kind", "Type", ColumnType::Text),
        ColumnSpec::new("conversation_name", "Conversation", ColumnType::Text).hidden(),
        ColumnSpec::new("project", "Project", ColumnType::Text)
            .describe(
                "What the source calls a grouping above the conversation: a Claude project, \
                 a GitHub repo, a GitLab project path.",
            )
            .hidden(),
        ColumnSpec::new("channel", "Channel", ColumnType::Text),
        ColumnSpec::new("touched_at", "Touched", ColumnType::Datetime).describe(
            "When it last changed at its source: Modified where the row has one, else \
             Created. The grid's newest-first order sorts on it.",
        ),
        ColumnSpec::new("created_at", "Created", ColumnType::Datetime)
            .describe(
                "When the thing came into being, as the source wrote it: a message's own \
                 stamp; for a document, the earliest moment in it; for a calendar event, \
                 when it happens.",
            )
            .hidden(),
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
        ColumnSpec::new("author_ref", "Author", ColumnType::Identity).describe(
            "Who wrote it, as the source showed them. Where the source has an identifier \
             for them — an address, a number, a Slack user — the cell is a chip the \
             contacts app resolves, and `from:` finds it by that identifier.",
        ),
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
        let mut source = self.identity(&row.source_id);
        // A source the config no longer names still has its provider
        // tag, and the tag names its own mark.
        if source.icon.is_none() && !row.provider.is_empty() {
            source.icon = Some(row.provider.clone());
            source.detail = Some(row.source.clone());
        }
        row.source_ref = Some(source);
        row.author_ref = author_identity(&row.author, row.author_handle.as_deref());
        row.author_term = row
            .author_handle
            .clone()
            .or_else(|| Some(row.author.clone()))
            .filter(|t| !t.is_empty());
    }

    /// The source as the grid shows it: the name the config gives the
    /// group, or its id when the config does not name it, led by its
    /// type's mark with the type's label on hover. Datalib's own rows —
    /// each source's storage report — are filed under datalib rather
    /// than under the source they measure.
    pub fn identity(&self, source_id: &str) -> Identity {
        if source_id == datalib_source_id() {
            return Identity {
                id: source_id.to_string(),
                label: "Datalib".to_string(),
                icon: Some("system".to_string()),
                detail: Some("Datalib's own row, not a source's data".to_string()),
                entity: None,
            };
        }
        let group = self.groups.get(source_id);
        let r#type = group.and_then(|g| {
            g.r#type
                .as_deref()
                .map(|t| source_catalog::source_type(t, &g.ingest_params))
        });
        Identity {
            id: source_id.to_string(),
            label: group
                .and_then(|g| g.name.clone())
                .unwrap_or_else(|| source_id.to_string()),
            icon: r#type.as_ref().and_then(|t| t.icon.clone()),
            detail: r#type.map(|t| t.label),
            // A group the config declares is a chip the viewer can
            // resolve and open; an id it does not know is only a name.
            entity: group.map(|_| Entity::Group(source_id).uri()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values_of(keys: &[SearchKeySpec], key: &str) -> KeyValues {
        keys.iter()
            .find(|k| k.key == key)
            .unwrap_or_else(|| panic!("no `{key}:` in {keys:?}"))
            .values
            .clone()
    }

    /// Every key the grid's search reads is offered, each saying what its
    /// values are, so the bar can draw a source as its chip.
    #[test]
    fn the_grid_offers_every_key_it_reads() {
        let keys = grid_keys();
        assert_eq!(values_of(&keys, "source_id"), KeyValues::Source);
        assert_eq!(values_of(&keys, "channel"), KeyValues::Text);
        assert_eq!(values_of(&keys, "before"), KeyValues::Stamp);
        assert_eq!(
            values_of(&keys, "is"),
            KeyValues::Words {
                words: vec!["document"]
            }
        );
        assert_eq!(values_of(&keys, "qmd_vsearch"), KeyValues::Text);
        assert_eq!(values_of(&keys, "from"), KeyValues::Person);
        assert_eq!(values_of(&keys, "label"), KeyValues::Text);
        let from = keys.iter().find(|k| k.key == "from").unwrap();
        assert_eq!(from.aliases, ["author", "author_handle"]);
        for k in &keys {
            let value = match &k.values {
                KeyValues::Words { words } => words[0],
                _ => "x",
            };
            let q = format!("{}:{value}", k.key);
            let refused = datalib_unified_index::query::parse_query(&q).refusal();
            assert_eq!(refused, None, "`{q}` is offered but refused");
        }
    }

    #[test]
    fn a_key_takes_its_values_from_its_words_or_its_column() {
        assert!(matches!(
            value_source::<GridRow>("is"),
            ValueSource::Words(w) if w == ["document"]
        ));
        assert!(matches!(
            value_source::<GridRow>("channel"),
            ValueSource::Column(GridRowColumn::Channel)
        ));
        assert!(matches!(
            value_source::<GridRow>("before"),
            ValueSource::Nothing
        ));
        assert!(matches!(
            value_source::<GridRow>("nope"),
            ValueSource::Nothing
        ));
        let held: Vec<String> = words_holding(&["error", "warning", "info"], "R")
            .into_iter()
            .map(|v| v.value)
            .collect();
        assert_eq!(held, ["error", "warning"]);
    }

    /// The search bar is the grid's one filter: every column a person
    /// might narrow by names its key, and a row field the grid can read
    /// the term's value from.
    #[test]
    fn every_column_but_score_and_contents_says_how_to_search_it() {
        // A row as the applet sends it, with what it resolves filled in.
        let mut row = SearchRow {
            author: "Worf".into(),
            ..SearchRow::default()
        };
        Sources::read(Path::new("/nonexistent")).resolve(&mut row);
        let row = serde_json::to_value(row).unwrap();
        for c in columns() {
            match (&c.search, c.field.as_str()) {
                (None, "score" | "snippet") => {}
                (None, field) => panic!("{field} has no search key"),
                (Some(s), field) => assert!(
                    row.get(&s.field).is_some(),
                    "{field} searches by {}, which a row does not carry",
                    s.field
                ),
            }
        }
    }

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
            touched_at: Some("2026-06-03T09:30:00-07:00".into()),
            is_document: true,
            conversation_name: "n".into(),
            project: "p".into(),
            account: "a".into(),
            org_uuid: "o".into(),
            org_name: "Org".into(),
            entire_chat: "/chat/c".into(),
            source: "Slack".into(),
            provider: "slack".into(),
            source_ref: None,
            source_id: "slack".into(),
            kind: "k".into(),
            author: "who".into(),
            author_handle: None,
            author_ref: None,
            author_term: None,
            channel: "#c".into(),
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
            "author",
            "author_handle",
            "author_term",
            "entire_chat",
            "source",
            "provider",
            "source_id",
            "org_uuid",
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
    fn the_configured_source_names_the_source_and_leads_with_its_mark() {
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
        let s = r.source_ref.unwrap();
        assert_eq!(
            (
                s.id.as_str(),
                s.label.as_str(),
                s.icon.as_deref(),
                s.detail.as_deref()
            ),
            ("work-mail", "Work mail", Some("gmail"), Some("Gmail"))
        );
    }

    #[test]
    fn an_unconfigured_source_falls_back_to_the_id_and_the_provider_mark() {
        let sources = Sources::read(Path::new("/nonexistent"));
        let mut r = row("slack", "Slack", "old-slack");
        sources.resolve(&mut r);
        let s = r.source_ref.unwrap();
        assert_eq!(
            (s.label.as_str(), s.icon.as_deref(), s.detail.as_deref()),
            ("old-slack", Some("slack"), Some("Slack"))
        );
        let mut d = row("datalib", "Datalib", "datalib");
        sources.resolve(&mut d);
        let d = d.source_ref.unwrap();
        assert_eq!(
            (d.label.as_str(), d.icon.as_deref()),
            ("Datalib", Some("system"))
        );
    }
}
