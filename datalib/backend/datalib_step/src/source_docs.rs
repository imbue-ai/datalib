//! What each source's config crate says about itself: its example
//! stanzas and the JSON Schema of its params. `datalib-step help` prints
//! them, and the tests below hold every source to them.

use datalib_source_common::{ConfigExample, ExampleKind};
use serde_json::Value;

use crate::source_type::SourceType;

pub struct SourceDocs {
    pub examples: &'static [ConfigExample],
    /// The ingest step's params, as JSON Schema.
    pub schema: Value,
}

impl SourceDocs {
    pub fn example(&self, kind: ExampleKind, method: &str) -> Option<&'static ConfigExample> {
        self.examples
            .iter()
            .find(|e| e.kind == kind && e.method == method)
    }
}

pub fn source_docs(t: SourceType) -> SourceDocs {
    use SourceType as T;
    match t {
        T::Airvisual => docs::<datalib_etl_airvisual_config::AirvisualConfig>(
            datalib_etl_airvisual_config::EXAMPLES,
        ),
        T::AppleMessages => docs::<datalib_etl_apple_messages_config::AppleMessagesConfig>(
            datalib_etl_apple_messages_config::EXAMPLES,
        ),
        T::ApplePhotos => docs::<datalib_etl_apple_photos_config::ApplePhotosConfig>(
            datalib_etl_apple_photos_config::EXAMPLES,
        ),
        T::Beeper => {
            docs::<datalib_etl_beeper_config::BeeperConfig>(datalib_etl_beeper_config::EXAMPLES)
        }
        T::Calendar => docs::<datalib_etl_calendar_config::CalendarConfig>(
            datalib_etl_calendar_config::EXAMPLES,
        ),
        T::Chatgpt => {
            docs::<datalib_etl_chatgpt_config::ChatgptConfig>(datalib_etl_chatgpt_config::EXAMPLES)
        }
        T::Claude => {
            docs::<datalib_etl_claude_config::ClaudeConfig>(datalib_etl_claude_config::EXAMPLES)
        }
        T::ClaudeCode => docs::<datalib_etl_claude_code_config::ClaudeCodeConfig>(
            datalib_etl_claude_code_config::EXAMPLES,
        ),
        T::Codex => {
            docs::<datalib_etl_codex_config::CodexConfig>(datalib_etl_codex_config::EXAMPLES)
        }
        T::Contacts => docs::<datalib_etl_contacts_config::ContactsConfig>(
            datalib_etl_contacts_config::EXAMPLES,
        ),
        T::Email => {
            docs::<datalib_etl_email_config::EmailConfig>(datalib_etl_email_config::EXAMPLES)
        }
        T::Facebook => docs::<datalib_etl_facebook_config::FacebookConfig>(
            datalib_etl_facebook_config::EXAMPLES,
        ),
        T::Fsindex => {
            docs::<datalib_etl_fsindex_config::FsindexConfig>(datalib_etl_fsindex_config::EXAMPLES)
        }
        T::Garmin => {
            docs::<datalib_etl_garmin_config::GarminConfig>(datalib_etl_garmin_config::EXAMPLES)
        }
        T::Github => {
            docs::<datalib_etl_github_config::GithubConfig>(datalib_etl_github_config::EXAMPLES)
        }
        T::Gitlab => {
            docs::<datalib_etl_gitlab_config::GitlabConfig>(datalib_etl_gitlab_config::EXAMPLES)
        }
        T::GoogleDrive => docs::<datalib_etl_google_drive_config::GoogleDriveConfig>(
            datalib_etl_google_drive_config::EXAMPLES,
        ),
        T::GoogleTakeout => docs::<datalib_etl_google_takeout_config::GoogleTakeoutConfig>(
            datalib_etl_google_takeout_config::EXAMPLES,
        ),
        T::Lightroom => docs::<datalib_etl_lightroom_config::LightroomConfig>(
            datalib_etl_lightroom_config::EXAMPLES,
        ),
        T::Linear => {
            docs::<datalib_etl_linear_config::LinearConfig>(datalib_etl_linear_config::EXAMPLES)
        }
        T::Linkedin => docs::<datalib_etl_linkedin_config::LinkedinConfig>(
            datalib_etl_linkedin_config::EXAMPLES,
        ),
        T::Media => {
            docs::<datalib_etl_media_config::MediaConfig>(datalib_etl_media_config::EXAMPLES)
        }
        T::Notion => {
            docs::<datalib_etl_notion_config::NotionConfig>(datalib_etl_notion_config::EXAMPLES)
        }
        T::Pdf => docs::<datalib_etl_pdf_config::PdfConfig>(datalib_etl_pdf_config::EXAMPLES),
        T::Perseus => {
            docs::<datalib_etl_perseus_config::PerseusConfig>(datalib_etl_perseus_config::EXAMPLES)
        }
        T::Signal => {
            docs::<datalib_etl_signal_config::SignalConfig>(datalib_etl_signal_config::EXAMPLES)
        }
        T::Slack => {
            docs::<datalib_etl_slack_config::SlackConfig>(datalib_etl_slack_config::EXAMPLES)
        }
        T::SmsBackupRestore => {
            docs::<datalib_etl_sms_backup_restore_config::SmsBackupRestoreConfig>(
                datalib_etl_sms_backup_restore_config::EXAMPLES,
            )
        }
        T::Whatsapp => docs::<datalib_etl_whatsapp_config::WhatsappConfig>(
            datalib_etl_whatsapp_config::EXAMPLES,
        ),
        T::Yolink => {
            docs::<datalib_etl_yolink_config::YolinkConfig>(datalib_etl_yolink_config::EXAMPLES)
        }
        T::Zoom => docs::<datalib_etl_zoom_config::ZoomConfig>(datalib_etl_zoom_config::EXAMPLES),
    }
}

fn docs<C: schemars::JsonSchema>(examples: &'static [ConfigExample]) -> SourceDocs {
    SourceDocs {
        examples,
        schema: schemars::schema_for!(C).to_value(),
    }
}

/// One option the params take, as `help <type> <method>` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// Dotted, from the params' root: `api.channels`, `common.event_tape.enabled`.
    /// A table in a list of tables is written `devices[].name`.
    pub path: String,
    pub kind: String,
    pub default: Option<Value>,
    pub description: Option<String>,
}

pub fn fields(schema: &Value) -> Vec<Field> {
    let mut out = Vec::new();
    walk(schema, schema, "", &mut out);
    out
}

fn walk(root: &Value, node: &Value, prefix: &str, out: &mut Vec<Field>) {
    let node = as_table(root, node).unwrap_or(node);
    let Some(props) = node.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, prop) in props {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(table) = as_table(root, prop) {
            walk(root, table, &path, out);
            continue;
        }
        let items = resolve(root, prop).get("items");
        if let Some(table) = items.and_then(|i| as_table(root, i)) {
            walk(root, table, &format!("{path}[]"), out);
            continue;
        }
        let resolved = resolve(root, prop);
        out.push(Field {
            path,
            kind: kind_of(root, prop),
            default: prop.get("default").filter(|d| !d.is_null()).cloned(),
            description: prop
                .get("description")
                .or_else(|| resolved.get("description"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
}

fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(r) => r
            .strip_prefix("#/")
            .map(|p| format!("/{p}"))
            .and_then(|p| root.pointer(&p))
            .unwrap_or(node),
        None => node,
    }
}

// A struct, or an `Option` of one: something whose fields are options of
// their own rather than one value. Either a list or a table (google_drive's
// `exports`) is one value that takes two shapes, and stays one field.
fn as_table<'a>(root: &'a Value, node: &'a Value) -> Option<&'a Value> {
    let node = resolve(root, node);
    if node.get("properties").is_some() {
        return Some(node);
    }
    let variants = node
        .get("anyOf")
        .or_else(|| node.get("oneOf"))?
        .as_array()?;
    let (tables, others): (Vec<&Value>, Vec<&Value>) = variants
        .iter()
        .map(|v| resolve(root, v))
        .partition(|v| v.get("properties").is_some());
    let only_null_besides = others
        .iter()
        .all(|v| v.get("type").and_then(Value::as_str) == Some("null"));
    match tables.as_slice() {
        [table] if only_null_besides => Some(table),
        _ => None,
    }
}

// The values a node allows when it is a closed set: `enum`, `const`, or
// variants that are each one of those (an enum whose variants carry docs).
fn fixed_values(root: &Value, node: &Value) -> Option<Vec<String>> {
    let node = resolve(root, node);
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        return Some(values.iter().map(Value::to_string).collect());
    }
    if let Some(value) = node.get("const") {
        return Some(vec![value.to_string()]);
    }
    let variants = node
        .get("anyOf")
        .or_else(|| node.get("oneOf"))?
        .as_array()?;
    let mut all = Vec::new();
    for v in variants {
        all.extend(fixed_values(root, v)?);
    }
    Some(all)
}

fn kind_of(root: &Value, node: &Value) -> String {
    let node = resolve(root, node);
    if let Some(values) = fixed_values(root, node) {
        return format!("one of {}", values.join(", "));
    }
    if let Some(variants) = node
        .get("anyOf")
        .or_else(|| node.get("oneOf"))
        .and_then(Value::as_array)
    {
        let kinds: Vec<String> = variants
            .iter()
            .map(|v| kind_of(root, v))
            .filter(|k| k != "null")
            .collect();
        return kinds.join(" or ");
    }
    let types: Vec<&str> = match node.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if types == ["null"] {
        return "null".to_string();
    }
    let named: Vec<String> = types
        .into_iter()
        .filter(|t| *t != "null")
        .map(|t| match t {
            "array" => match node.get("items") {
                Some(items) => format!("list of {}", kind_of(root, items)),
                None => "list".to_string(),
            },
            "object" => "table".to_string(),
            other => other.to_string(),
        })
        .collect();
    if named.is_empty() {
        "any".to_string()
    } else {
        named.join(" or ")
    }
}

/// An example stanza with its group renamed: the `id` of its `[[groups]]`
/// entry, the `group` of each step, and every step id naming the group,
/// in an input or in a comment's backticks. The `type` stays, which is
/// why this is not a plain replace.
#[cfg(test)]
pub fn with_group_id(toml: &str, from: &str, to: &str) -> String {
    let quoted = format!("\"{from}\"");
    let step_prefix = format!("\"{from}/");
    toml.lines()
        .map(|line| {
            let key = line.trim_start();
            let line = if key.starts_with("id =") || key.starts_with("group =") {
                line.replace(&quoted, &format!("\"{to}\""))
            } else {
                line.to_string()
            };
            line.replace(&step_prefix, &format!("\"{to}/"))
                .replace(&format!("`{from}/"), &format!("`{to}/"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use strum::VariantArray;

    use super::*;
    use crate::methods::{held, ingest_methods};

    /// Every dotted path a params tree writes, in the spelling [`Field::path`]
    /// uses, so a schema's fields can be checked off against an example.
    fn written_paths(params: &Value) -> Vec<String> {
        let mut out = Vec::new();
        collect_paths(params, "", &mut out);
        out
    }

    fn collect_paths(value: &Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (k, v) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    collect_paths(v, &path, out);
                }
            }
            Value::Array(items) if items.iter().any(Value::is_object) => {
                for item in items {
                    collect_paths(item, &format!("{prefix}[]"), out);
                }
            }
            _ => out.push(prefix.to_string()),
        }
    }

    // Every source's params carry these; the field reference lists them
    // once, and a stanza sets them only where they do something.
    const SHARED: &[&str] = &["common", "latchkey_settings"];

    struct Loaded {
        group_id: String,
        group_type: Option<String>,
        ingest_params: Value,
        functions: Vec<String>,
    }

    // Loads and plans an example the way `config_examples_test` does every
    // checked-in config, and hands back what the checks below look at.
    fn load(name: &str, toml: &str, dir: &Path) -> Loaded {
        let path = dir.join(format!("{}.toml", name.replace('/', "_")));
        std::fs::write(
            &path,
            format!("data_root = \"{}\"\n\n{toml}", dir.join("root").display()),
        )
        .unwrap();
        crate::config_examples_test::validate_config(name, &path);
        let (cfg, _) = datalib_dag::config::load(&path).unwrap();
        assert_eq!(
            cfg.groups.len(),
            1,
            "{name}: one [[groups]] entry per example"
        );
        let group = &cfg.groups[0];
        let ingest = cfg
            .steps
            .iter()
            .find(|s| s.function.as_deref() == Some("ingest"))
            .unwrap_or_else(|| panic!("{name}: no ingest step"));
        assert_eq!(ingest.group.as_deref(), Some(group.id.as_str()), "{name}");
        Loaded {
            group_id: group.id.clone(),
            group_type: group.r#type.clone(),
            ingest_params: ingest
                .params
                .as_ref()
                .map(|p| serde_json::to_value(p).unwrap())
                .unwrap_or(Value::Null),
            functions: cfg
                .steps
                .iter()
                .filter_map(|s| s.function.clone())
                .collect(),
        }
    }

    /// Every source shows a minimal and a complete stanza for each of its
    /// ingest methods; each loads and plans as a sync would, holds the
    /// method it is named for, and still loads under another group id. A
    /// complete stanza may also hold a method its own pairs with (Gmail's
    /// Takeout); its config's `validate`, run by the load, says which do.
    /// A method's complete stanza writes every option of its own table,
    /// and a source's complete stanzas together write every other option
    /// but the shared `common` and `latchkey_settings` ones, which a
    /// stanza sets only where they do something. A field added to a
    /// config struct without an example fails here by name.
    #[test]
    fn every_source_has_examples_that_load_and_cover_its_options() {
        let dir = tempfile::tempdir().unwrap();
        let mut missing = Vec::new();
        for &t in SourceType::VARIANTS {
            let docs = source_docs(t);
            let methods = ingest_methods(t);
            let mut written: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
            for m in methods {
                for &kind in ExampleKind::VARIANTS {
                    let Some(example) = docs.example(kind, m.path) else {
                        missing.push(format!(
                            "{t}: no {} example for `{}`",
                            kind.as_str(),
                            m.path
                        ));
                        continue;
                    };
                    let name = format!("{t}/{}_{}", kind.as_str(), m.path);
                    let loaded = load(&name, example.toml, dir.path());
                    assert_eq!(
                        loaded.group_id,
                        t.as_str(),
                        "{name}: the group's id is its type"
                    );
                    assert_eq!(loaded.group_type.as_deref(), Some(t.as_str()), "{name}");
                    let holds: Vec<&str> = held(&loaded.ingest_params, methods)
                        .iter()
                        .map(|h| h.path)
                        .collect();
                    match kind {
                        ExampleKind::Minimal => {
                            assert_eq!(holds, [m.path], "{name}: holds the method it is named for")
                        }
                        ExampleKind::Complete => assert!(
                            holds.contains(&m.path),
                            "{name}: holds the method it is named for, not only {holds:?}"
                        ),
                    }
                    for needed in ["render_markdown", "render_time_series"] {
                        let wanted = needed == "render_markdown" || t.renders_time_series();
                        if wanted && !loaded.functions.iter().any(|f| f == needed) {
                            missing.push(format!("{name}: declares no `{needed}` step"));
                        }
                    }
                    if kind == ExampleKind::Complete {
                        written
                            .entry(m.path)
                            .or_default()
                            .extend(written_paths(&loaded.ingest_params));
                    }
                    let renamed = load(
                        &format!("{name}-renamed"),
                        &with_group_id(example.toml, t.as_str(), "renamed-source"),
                        dir.path(),
                    );
                    assert_eq!(renamed.group_id, "renamed-source", "{name}");
                    let renamed_text = with_group_id(example.toml, t.as_str(), "renamed-source");
                    for stale in [format!("\"{t}/"), format!("`{t}/")] {
                        assert!(
                            !renamed_text.contains(&stale),
                            "{name}: renamed, it still names a step {stale}…"
                        );
                    }
                }
            }
            for e in docs.examples {
                assert!(
                    methods.iter().any(|m| m.path == e.method),
                    "{t}: an example names `{}`, which is not one of its methods",
                    e.method
                );
            }
            if docs.examples.is_empty() {
                continue;
            }
            for field in fields(&docs.schema) {
                let top = field.path.split(['.', '[']).next().unwrap_or("");
                if SHARED.contains(&top) {
                    continue;
                }
                let writes = |paths: &BTreeSet<String>| {
                    paths.iter().any(|w| {
                        w == &field.path
                            || w.starts_with(&format!("{}.", field.path))
                            || w.starts_with(&format!("{}[]", field.path))
                    })
                };
                let (covered, by) = match written.get(top) {
                    Some(own) if methods.iter().any(|m| m.path == top) => {
                        (writes(own), format!("complete_{top}"))
                    }
                    _ => (
                        written.values().any(writes),
                        "any complete example".to_string(),
                    ),
                };
                if !covered {
                    missing.push(format!("{t}: {by} does not write `{}`", field.path));
                }
            }
        }
        assert!(missing.is_empty(), "{}", missing.join("\n"));
    }

    #[test]
    fn a_group_id_is_renamed_and_the_type_kept() {
        let toml = "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\n\
                    group = \"slack\"\nfunction = \"render_markdown\"\ninputs = [\"slack/ingest\"]\n";
        assert_eq!(
            with_group_id(toml, "slack", "work"),
            "[[groups]]\nid = \"work\"\ntype = \"slack\"\n\n[[steps]]\n\
             group = \"work\"\nfunction = \"render_markdown\"\ninputs = [\"work/ingest\"]\n"
        );
    }

    #[test]
    fn fields_walk_tables_options_and_lists_of_tables() {
        let schema = source_docs(SourceType::Yolink).schema;
        let paths: Vec<String> = fields(&schema).into_iter().map(|f| f.path).collect();
        assert!(
            paths.contains(&"common.event_tape.enabled".to_string()),
            "{paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.starts_with("api.devices[].")),
            "{paths:?}"
        );
        let slack = fields(&source_docs(SourceType::Slack).schema);
        let channels = slack.iter().find(|f| f.path == "api.channels").unwrap();
        assert_eq!(channels.kind, "list of string");
        assert!(channels
            .description
            .as_deref()
            .unwrap()
            .contains("Channel names"));
        let limit = slack
            .iter()
            .find(|f| f.path == "common.blob_size_limit_bytes")
            .unwrap();
        assert_eq!(limit.kind, "integer or string", "{limit:?}");
        let drive = fields(&source_docs(SourceType::GoogleDrive).schema);
        let exports = drive.iter().find(|f| f.path == "api.exports").unwrap();
        assert_eq!(
            exports.kind, r#"list of one of "pdf", "odf", "ooxml" or table"#,
            "{exports:?}"
        );
    }
}
