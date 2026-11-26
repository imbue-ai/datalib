//! A WhatsApp or Signal ingest whose params name no variable for its
//! backup secret. `datalib-step` falls back to a default name, but the
//! runner hands a step only the variables its params name
//! (`docs/dev/step_protocol.md` § "What your command receives"), so the
//! default is written into its `[steps.params.backup]`: the step keeps
//! decrypting whether the app or a terminal's `datalib-pipeline` runs it.
//!
//! A text edit: every comment and key the file holds survives. A step
//! whose `backup` table is not written as a `[steps.params.backup]`
//! header (an inline table) is refused, naming the step, so a person
//! adds the line by hand.

use anyhow::{bail, Context as _, Result};

use crate::retired::is_datalib_step;

/// The variable each source type's ingest reads its backup secret from
/// when its params name none, and the params key that names it.
const BACKUP_SECRETS: &[(&str, &str, &str)] = &[
    ("whatsapp", "key_env_var", "WHATSAPP_BACKUP_DECRYPTION_KEY"),
    ("signal", "aep_env_var", "SIGNAL_BACKUP_PASSPHRASE"),
];

const INGEST: &str = "ingest";

/// One ingest that relies on the default name.
struct Unnamed {
    id: String,
    key: &'static str,
    var: &'static str,
}

fn unnamed(text: &str) -> Result<Vec<Unnamed>> {
    let doc: toml::Value = toml::from_str(text).context("parse the config")?;
    let steps = doc
        .get("steps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for step in &steps {
        let (Some(id), Some(command)) = (
            step.get("id").and_then(|v| v.as_str()),
            step.get("command").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let argv: Vec<&str> = command.split_whitespace().collect();
        let Some(&(_, key, var)) = (match argv.as_slice() {
            [program, "run", ingest, source_type, ..]
                if is_datalib_step(program) && *ingest == INGEST =>
            {
                BACKUP_SECRETS.iter().find(|(t, _, _)| t == source_type)
            }
            _ => None,
        }) else {
            continue;
        };
        let backup = step.get("params").and_then(|p| p.get("backup"));
        if backup.is_some_and(|b| b.get(key).is_some()) {
            continue;
        }
        out.push(Unnamed {
            id: id.to_string(),
            key,
            var,
        });
    }
    Ok(out)
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!unnamed(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut out = text.to_string();
    let mut by_hand = Vec::new();
    for Unnamed { id, key, var } in unnamed(text)? {
        match backup_header_end(&out, &id) {
            Some(at) => out.insert_str(at, &format!("\n{key} = {}", quote(var))),
            None => by_hand.push(format!(
                "{id} (add `{key} = {}` to its params' `backup` table)",
                quote(var)
            )),
        }
    }
    if !by_hand.is_empty() {
        bail!(
            "these steps name no variable for their backup secret, and their `backup` \
             table is not a `[steps.params.backup]` header this rewrite can add to: {}",
            by_hand.join(", ")
        );
    }
    Ok(out)
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// Just past the newline that ends the line `at` is on.
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i + 1)
}

/// The end of the `[steps.params.backup]` header line that belongs to
/// the step with `id`, when it is written as one.
fn backup_header_end(text: &str, id: &str) -> Option<usize> {
    let id_line = format!("id = {}", quote(id));
    let start = text
        .match_indices(&id_line)
        .map(|(i, _)| i)
        .find(|&i| text[line_start(text, i)..i].trim().is_empty())?;
    let mut at = line_end(text, start);
    while at < text.len() {
        let end = line_end(text, at);
        let line = text[at..end].trim();
        if line.starts_with("[[") {
            return None;
        }
        if line == "[steps.params.backup]" {
            return Some(at + text[at..end].trim_end_matches(['\n', '\r']).len());
        }
        at = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
id = "phone"
type = "whatsapp"

[[groups]]
id = "sig"
type = "signal"

[[groups]]
id = "work-phone"
type = "whatsapp"

# the phone in sickbay
[[steps]]
id = "phone/ingest"
command = "datalib-step run ingest whatsapp"

[steps.params.backup]
path = "/backups/phone"  # pulled off by adb

[[steps]]
id = "sig/ingest"
command = "datalib-step run ingest signal"

[steps.params.backup]
path = "/backups/sig"

[[steps]]
id = "work-phone/ingest"
command = "datalib-step run ingest whatsapp"

[steps.params.backup]
path = "/backups/work"
key_env_var = "WORK_PHONE_KEY"
"#;

    fn named(text: &str, id: &str, key: &str) -> Option<String> {
        let doc: toml::Value = toml::from_str(text).unwrap();
        doc["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"].as_str() == Some(id))
            .and_then(|s| s["params"]["backup"].get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// A WhatsApp or Signal ingest that relied on `datalib-step`'s default
    /// name gets it written, since the runner hands a step only what its
    /// params name and the app no longer hands these two by name; one
    /// that names its own keeps it, and the comments stay.
    #[test]
    fn an_ingest_relying_on_the_default_names_it() {
        assert!(is_retired(BEFORE).unwrap());
        let out = rewrite(BEFORE).unwrap();
        assert_eq!(
            named(&out, "phone/ingest", "key_env_var").as_deref(),
            Some("WHATSAPP_BACKUP_DECRYPTION_KEY"),
            "{out}"
        );
        assert_eq!(
            named(&out, "sig/ingest", "aep_env_var").as_deref(),
            Some("SIGNAL_BACKUP_PASSPHRASE"),
            "{out}"
        );
        assert_eq!(
            named(&out, "work-phone/ingest", "key_env_var").as_deref(),
            Some("WORK_PHONE_KEY"),
            "{out}"
        );
        assert!(out.contains("# the phone in sickbay\n"), "{out}");
        assert!(
            out.contains("path = \"/backups/phone\"  # pulled off by adb\n"),
            "{out}"
        );
        assert!(!is_retired(&out).unwrap(), "{out}");
    }

    /// An inline `backup` table has no header line to add to; the
    /// rewrite says which step to fix rather than leave it unable to
    /// decrypt.
    #[test]
    fn an_inline_backup_table_is_refused_by_name() {
        let text = r#"[[steps]]
id = "phone/ingest"
command = "datalib-step run ingest whatsapp"
params = { backup = { path = "/backups/phone" } }
"#;
        let err = rewrite(text).unwrap_err().to_string();
        assert!(err.contains("phone/ingest"), "{err}");
        assert!(err.contains("key_env_var"), "{err}");
    }

    #[test]
    fn another_programs_step_is_left_alone() {
        let text = r#"[[steps]]
id = "phone/ingest"
command = "./bin/my-whatsapp run ingest whatsapp"

[steps.params.backup]
path = "/backups/phone"
"#;
        assert!(!is_retired(text).unwrap());
    }
}
