//! The locks from before search had an index of its own. `qmd_embed`, the
//! one every `embed` holds, is `embedder`. `qmd_keyword` is gone: only
//! the index step (`unified_grid_and_search_index`) held it once it alone
//! wrote the grid index's file, and a step never runs beside itself. A step's `locks` that named `qmd_embed`
//! name `embedder`; one that named `qmd_keyword` drops it, and a step left
//! holding nothing loses the key and holds its default lock again. A
//! `[[locks]]` entry that resized `qmd_embed` resizes `embedder`, and one
//! that declared `qmd_keyword` goes, with its comments.
//!
//! A text edit, like `todoist_since.rs`: every other comment and key the
//! file holds survives.

use std::collections::BTreeMap;
use std::ops::Range;

use anyhow::{Context as _, Result};
use datalib_dag::config_array::edit_string_array;
use datalib_dag::config_order::entry_ranges;
use serde::Deserialize;
use toml::Spanned;

const OLD_EMBED: &str = "qmd_embed";
const NEW_EMBED: &str = "embedder";
const GONE: &str = "qmd_keyword";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    steps: Vec<StepView>,
    #[serde(default)]
    locks: Vec<LockView>,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    locks: Option<Spanned<toml::Value>>,
}

#[derive(Deserialize)]
struct LockView {
    name: Spanned<String>,
}

/// The name a lock has now, or `None` for one that is gone.
fn renamed(name: &str) -> Option<&str> {
    match name {
        OLD_EMBED => Some(NEW_EMBED),
        GONE => None,
        other => Some(other),
    }
}

fn is_old(name: &str) -> bool {
    name == OLD_EMBED || name == GONE
}

/// What a step's `locks` value becomes: `Some(text)` to replace it with,
/// `None` to take the key out. Untouched values are not returned.
fn new_value(written: &str, value: &toml::Value) -> Result<Option<Option<String>>> {
    match value {
        toml::Value::Array(names) => {
            let names: Vec<&str> = names.iter().filter_map(toml::Value::as_str).collect();
            if !names.iter().any(|n| is_old(n)) {
                return Ok(None);
            }
            let mut after: Vec<String> = Vec::new();
            for name in names.iter().filter_map(|n| renamed(n)) {
                if !after.iter().any(|a| a == name) {
                    after.push(name.to_string());
                }
            }
            if after.is_empty() {
                return Ok(Some(None));
            }
            // Renamed where it stands: the array edit appends a string it
            // did not already hold.
            let renamed_in_place =
                written.replace(&format!("\"{OLD_EMBED}\""), &format!("\"{NEW_EMBED}\""));
            let text = edit_string_array(&renamed_in_place, &after).map_err(anyhow::Error::msg)?;
            Ok(Some(Some(text)))
        }
        toml::Value::Table(held) => {
            if !held.keys().any(|n| is_old(n)) {
                return Ok(None);
            }
            let mut after: BTreeMap<&str, &toml::Value> = BTreeMap::new();
            for (name, hold) in held {
                if let Some(name) = renamed(name) {
                    after.entry(name).or_insert(hold);
                }
            }
            if after.is_empty() {
                return Ok(Some(None));
            }
            let pairs: Vec<String> = after
                .iter()
                .map(|(name, hold)| format!("{name} = {hold}"))
                .collect();
            Ok(Some(Some(format!("{{ {} }}", pairs.join(", ")))))
        }
        _ => Ok(None),
    }
}

/// The whole line or lines a key's value sits on, so taking the key out
/// leaves no blank line where it was.
fn key_lines(text: &str, value: &Range<usize>) -> Range<usize> {
    let start = text[..value.start].rfind('\n').map_or(0, |i| i + 1);
    let end = text[value.end..]
        .find('\n')
        .map_or(text.len(), |i| value.end + i + 1);
    start..end
}

/// Whether `entry` is a `[[locks]]` entry declaring the lock that is gone.
fn declares_the_gone_lock(entry: &str) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(entry) else {
        return false;
    };
    table
        .get("locks")
        .and_then(toml::Value::as_array)
        .and_then(|a| a.first())
        .and_then(|l| l.get("name"))
        .and_then(toml::Value::as_str)
        == Some(GONE)
}

fn edits(text: &str) -> Result<Vec<(Range<usize>, String)>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for locks in view.steps.iter().filter_map(|s| s.locks.as_ref()) {
        let span = locks.span();
        match new_value(&text[span.clone()], locks.get_ref())? {
            None => {}
            Some(Some(with)) => edits.push((span, with)),
            Some(None) => edits.push((key_lines(text, &span), String::new())),
        }
    }
    for lock in &view.locks {
        if lock.name.get_ref() == OLD_EMBED {
            edits.push((lock.name.span(), format!("\"{NEW_EMBED}\"")));
        }
    }
    for range in entry_ranges(text) {
        if declares_the_gone_lock(&text[range.clone()]) {
            edits.push((range, String::new()));
        }
    }
    Ok(edits)
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!edits(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut edits = edits(text)?;
    // Editing the last span first leaves every earlier one where it was.
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (span, with) in edits {
        out.replace_range(span, &with);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
id = "picard"
type = "email"

[[groups]]
id = "riker"
type = "slack"

[[steps]]
group = "picard"
function = "render_markdown"
command = "datalib-step"

[[steps]]
group = "picard"
function = "embed"
command = "datalib-step"
inputs = ["unified_grid_and_search_index/unified_grid_and_search_index"]
locks = ["qmd_embed", "gpu"]  # one model, one GPU

[[steps]]
group = "riker"
function = "render_markdown"
command = "datalib-step"

[[steps]]
group = "riker"
function = "embed"
command = "datalib-step"
inputs = ["unified_grid_and_search_index/unified_grid_and_search_index"]
locks = { qmd_embed = "exclusive" }

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "unified_grid_and_search_index"
command = "datalib-step"
inputs = ["*/render_markdown"]
locks = ["qmd_keyword"]

# the warp core takes one embed at a time
[[locks]]
name = "qmd_embed"
slots = 1

# never resize this one
[[locks]]
name = "qmd_keyword"
slots = 1

[[locks]]
name = "gpu"
"#;

    fn held(text: &str, id: &str) -> Vec<String> {
        let check = crate::check_as_current(text);
        let spec = check.graph.steps.iter().find(|s| s.id == id).unwrap();
        datalib_dag::supervisor::locks::held_by(spec)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// `qmd_embed` becomes `embedder` in a list, a table and a `[[locks]]`
    /// entry; `qmd_keyword` leaves a step's list, the step falls back to its
    /// default, and its `[[locks]]` entry goes with its comment. The result
    /// loads whole, and a second pass finds nothing to do.
    #[test]
    fn the_embed_lock_is_renamed_and_the_keyword_lock_goes() {
        assert!(is_retired(BEFORE).unwrap());

        let after = rewrite(BEFORE).unwrap();
        assert!(!after.contains("qmd_"), "{after}");
        assert!(
            after.contains("locks = [\"embedder\", \"gpu\"]  # one model, one GPU"),
            "{after}"
        );
        assert!(
            after.contains("locks = { embedder = \"exclusive\" }"),
            "{after}"
        );
        assert!(
            after.contains(
                "# the warp core takes one embed at a time\n[[locks]]\nname = \"embedder\""
            ),
            "{after}"
        );
        assert!(!after.contains("never resize this one"), "{after}");
        assert!(
            !after.contains("\n\n\n"),
            "no blank lines pile up:\n{after}"
        );

        let check = crate::check_as_current(&after);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        assert_eq!(held(&after, "picard/embed"), ["embedder", "gpu"]);
        assert_eq!(held(&after, "riker/embed"), ["embedder"]);
        assert_eq!(
            held(
                &after,
                "unified_grid_and_search_index/unified_grid_and_search_index"
            ),
            ["index"]
        );
        assert!(!is_retired(&after).unwrap());
        assert_eq!(rewrite(&after).unwrap(), after);
    }

    /// Both were built in, so a step named them without declaring them.
    /// The runner of the day refused such a step; the rewrite gives it back.
    #[test]
    fn a_step_naming_an_old_lock_it_never_declared_loads_again() {
        let before = r#"[[groups]]
id = "picard"
type = "email"

[[steps]]
group = "picard"
function = "render_markdown"
command = "datalib-step"

[[steps]]
group = "picard"
function = "embed"
command = "datalib-step"
inputs = ["picard/render_markdown"]
locks = ["qmd_embed"]

[[steps]]
id = "custom/writer"
command = "x"
inputs = ["picard/render_markdown"]
locks = ["qmd_keyword"]
"#;
        assert!(is_retired(before).unwrap());
        let after = rewrite(before).unwrap();
        let check = crate::check_as_current(&after);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        assert_eq!(held(&after, "picard/embed"), ["embedder"]);
        assert_eq!(held(&after, "custom/writer"), ["index"]);
    }

    /// A config that names neither lock is left exactly as written.
    #[test]
    fn a_config_without_the_old_names_is_untouched() {
        let text = "[[locks]]\nname = \"gpu\"\n\n[[steps]]\nid = \"x/y\"\ncommand = \"x\"\nlocks = [\"gpu\"]\n";
        assert!(!is_retired(text).unwrap());
        assert_eq!(rewrite(text).unwrap(), text);
    }
}
