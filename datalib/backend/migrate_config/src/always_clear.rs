//! `common.always_clear_before_ingest`, the switch that emptied a source's
//! store before every ingest. Every source now deletes what its input lost
//! on its own, so the key is gone, and this takes it out of a config that
//! still carries it.
//!
//! An edit of the document as written (`toml_edit`), so every other line,
//! comment and layout survives. The comment lines directly above the key
//! go with it — they explained the switch — and so does a `common` table
//! it leaves empty.

use anyhow::{bail, Context as _, Result};
use toml_edit::{DocumentMut, Item, TableLike};

pub const KEY: &str = "always_clear_before_ingest";

pub fn is_retired(text: &str) -> Result<bool> {
    let doc: DocumentMut = text.parse().context("parse the config")?;
    let retired = steps(&doc).any(|step| {
        step.get("params")
            .and_then(Item::as_table_like)
            .and_then(|p| p.get("common"))
            .and_then(Item::as_table_like)
            .is_some_and(|c| c.contains_key(KEY))
    });
    Ok(retired)
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("parse the config")?;
    if let Some(steps) = doc.get_mut("steps").and_then(Item::as_array_of_tables_mut) {
        for step in steps.iter_mut() {
            let Some(params) = step.get_mut("params").and_then(Item::as_table_like_mut) else {
                continue;
            };
            let Some(common) = params.get_mut("common").and_then(Item::as_table_like_mut) else {
                continue;
            };
            common.remove(KEY);
            if common.is_empty() {
                params.remove("common");
            }
        }
    }
    let out = doc.to_string();
    if is_retired(&out)? {
        bail!(
            "`{KEY}` is written in a way this rewrite does not reach (a `steps` array \
             of inline tables?); delete it by hand from each step's `common`"
        );
    }
    Ok(out)
}

fn steps(doc: &DocumentMut) -> impl Iterator<Item = &dyn TableLike> {
    doc.get("steps")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|a| a.iter())
        .map(|t| t as &dyn TableLike)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIZARD: &str = r#"[[groups]]
id = "sms"
type = "sms_backup_restore"

[[steps]]
group = "sms"
function = "ingest"
[steps.params.backup]
path = "~/backups/SMSBackupRestore"
[steps.params.common]
# The export directory is the whole archive, so a message no longer in it
# was deleted on the phone.
always_clear_before_ingest = true

[[steps]]
group = "sms"
function = "render_markdown"
inputs = ["sms/ingest"]
"#;

    #[test]
    fn the_key_its_comment_and_its_emptied_table_go() {
        assert!(is_retired(WIZARD).unwrap());
        let out = rewrite(WIZARD).unwrap();
        assert_eq!(
            out,
            r#"[[groups]]
id = "sms"
type = "sms_backup_restore"

[[steps]]
group = "sms"
function = "ingest"
[steps.params.backup]
path = "~/backups/SMSBackupRestore"

[[steps]]
group = "sms"
function = "render_markdown"
inputs = ["sms/ingest"]
"#
        );
        assert!(!is_retired(&out).unwrap());
    }

    #[test]
    fn a_common_table_with_other_keys_keeps_them() {
        let text = r#"[[steps]]
group = "g"
function = "ingest"
[steps.params.common]
blob_size_limit_bytes = "5 MB"
always_clear_before_ingest = false
"#;
        assert_eq!(
            rewrite(text).unwrap(),
            r#"[[steps]]
group = "g"
function = "ingest"
[steps.params.common]
blob_size_limit_bytes = "5 MB"
"#
        );
    }

    /// The layouts a person writes by hand, not only the wizard's.
    #[test]
    fn inline_and_dotted_keys_go_too() {
        for text in [
            "[[steps]]\ngroup = \"g\"\nfunction = \"ingest\"\n\
             params = { common = { always_clear_before_ingest = true } }\n",
            "[[steps]]\ngroup = \"g\"\nfunction = \"ingest\"\n[steps.params]\n\
             common.always_clear_before_ingest = true\n",
        ] {
            assert!(is_retired(text).unwrap(), "{text}");
            let out = rewrite(text).unwrap();
            assert!(!out.contains(KEY), "{out}");
            assert!(out.contains("function = \"ingest\""), "{out}");
        }
    }

    /// A key of that name anywhere but a step's `common` is not ours.
    #[test]
    fn the_same_name_elsewhere_is_left_alone() {
        let text = r#"[[steps]]
id = "custom"
command = "tool"
[steps.params.other]
always_clear_before_ingest = true
"#;
        assert!(!is_retired(text).unwrap());
        assert_eq!(rewrite(text).unwrap(), text);
    }
}
