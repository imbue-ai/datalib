//! A config that names a lock by a name from when qmd did the search.
//! `qmd_embed`, the one-slot lock every `embed` holds, is `embedder` now;
//! the rewrite renames it in a `[[locks]]` entry's `name` and in every
//! step's `locks`. `qmd_keyword` is no longer a lock every config has, so
//! a step that still holds it would be refused for holding a lock nobody
//! declares; the rewrite declares it, one slot as it was, so that step
//! keeps loading and keeps what it held. Everything else stays as written.

use std::ops::Range;

use anyhow::{Context as _, Result};
use datalib_dag::supervisor::locks::{EMBEDDER, RETIRED_EMBEDDER};
use serde::Deserialize;
use toml::Spanned;

const RETIRED_KEYWORD: &str = "qmd_keyword";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    locks: Vec<LockView>,
    #[serde(default)]
    steps: Vec<StepView>,
}

#[derive(Deserialize)]
struct LockView {
    #[serde(default)]
    name: Option<Spanned<String>>,
}

#[derive(Deserialize)]
struct StepView {
    /// `["a", "b"]` or `{ a = "exclusive" }`: either way, the names are
    /// words in this span and nothing else in it is.
    #[serde(default)]
    locks: Option<Spanned<toml::Value>>,
}

fn held(value: &toml::Value) -> Vec<&str> {
    match value {
        toml::Value::Array(names) => names.iter().filter_map(toml::Value::as_str).collect(),
        toml::Value::Table(holds) => holds.keys().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

fn declared(view: &View) -> impl Iterator<Item = &Spanned<String>> {
    view.locks.iter().filter_map(|l| l.name.as_ref())
}

fn holds(view: &View, lock: &str) -> bool {
    view.steps
        .iter()
        .filter_map(|s| s.locks.as_ref())
        .any(|l| held(l.get_ref()).contains(&lock))
}

/// A step holds `qmd_keyword` and no `[[locks]]` entry declares it.
fn keyword_undeclared(view: &View) -> bool {
    holds(view, RETIRED_KEYWORD) && !declared(view).any(|n| n.get_ref() == RETIRED_KEYWORD)
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(declared(&view).any(|n| n.get_ref() == RETIRED_EMBEDDER)
        || holds(&view, RETIRED_EMBEDDER)
        || keyword_undeclared(&view))
}

/// `text` with every `old` that stands as a whole lock name, not as part
/// of a longer one, written `new`.
fn rename_word(text: &str, old: &str, new: &str) -> String {
    let part_of_a_name = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(old) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + old.len()..].chars().next();
        out.push_str(&rest[..at]);
        if before.is_some_and(part_of_a_name) || after.is_some_and(part_of_a_name) {
            out.push_str(old);
        } else {
            out.push_str(new);
        }
        rest = &rest[at + old.len()..];
    }
    out.push_str(rest);
    out
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for name in declared(&view).filter(|n| n.get_ref() == RETIRED_EMBEDDER) {
        edits.push((name.span(), toml::Value::from(EMBEDDER).to_string()));
    }
    for locks in view.steps.iter().filter_map(|s| s.locks.as_ref()) {
        if held(locks.get_ref()).contains(&RETIRED_EMBEDDER) {
            let span = locks.span();
            let renamed = rename_word(&text[span.clone()], RETIRED_EMBEDDER, EMBEDDER);
            edits.push((span, renamed));
        }
    }
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (span, with) in edits {
        out.replace_range(span, &with);
    }
    if keyword_undeclared(&view) {
        out = format!(
            "{}\n\n# Every config had this lock while qmd kept the keyword index; a step\n\
             # here still holds it.\n[[locks]]\nname = \"{RETIRED_KEYWORD}\"\nslots = 1\n",
            out.trim_end()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
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
locks = ["qmd_embed", "qmd_embedding_budget"]  # one model at a time

[[steps]]
id = "tools/embed-photos"
command = "embed-photos"
inputs = ["picard/render_markdown"]
locks = { qmd_embed = "exclusive" }

[[steps]]
id = "tools/keywords"
command = "keywords"
inputs = ["picard/render_markdown"]
locks = ["qmd_keyword"]

# two embeds when the GPU is free
[[locks]]
name = "qmd_embed"
slots = 2

[[locks]]
name = "qmd_embedding_budget"
"#;

    fn holds_of(check: &datalib_dag::config::ConfigCheck, id: &str) -> Vec<String> {
        let spec = check.graph.steps.iter().find(|s| s.id == id).unwrap();
        datalib_dag::supervisor::locks::held_by(spec)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Every `qmd_embed` is `embedder`, in a declaration and in both
    /// shapes of a step's `locks`, and a lock whose name only starts with
    /// it is left alone; a step holding `qmd_keyword` gets it declared.
    /// Everything loads, the resize stands, and a second pass finds
    /// nothing to do.
    #[test]
    fn the_embed_lock_is_renamed_and_a_held_keyword_lock_declared() {
        let before = datalib_dag::config::check_text(BEFORE);
        assert_eq!(
            before.dropped(),
            1,
            "the step holding the undeclared `qmd_keyword` is refused: {}",
            before.render("c.toml".as_ref())
        );
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert!(!after.contains("\"qmd_embed\""), "{after}");
        assert!(
            after.contains("locks = [\"embedder\", \"qmd_embedding_budget\"]  # one model"),
            "{after}"
        );
        assert!(
            after.contains("locks = { embedder = \"exclusive\" }"),
            "{after}"
        );
        assert!(
            after.contains(
                "# two embeds when the GPU is free\n[[locks]]\nname = \"embedder\"\nslots = 2"
            ),
            "{after}"
        );
        let check = datalib_dag::config::check_text(&after);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        assert_eq!(
            holds_of(&check, "picard/embed"),
            ["embedder", "qmd_embedding_budget"]
        );
        assert_eq!(holds_of(&check, "tools/embed-photos"), ["embedder"]);
        assert_eq!(holds_of(&check, "tools/keywords"), ["qmd_keyword"]);
        let slots = |name: &str| {
            check
                .graph
                .locks
                .iter()
                .find(|l| l.name == name)
                .map(|l| l.slots)
        };
        assert_eq!(slots("embedder"), Some(2));
        assert_eq!(slots("qmd_keyword"), Some(1));
        assert!(!is_retired(&after).unwrap());
        assert_eq!(rewrite(&after).unwrap(), after);
    }

    /// A config that names neither lock has nothing to do.
    #[test]
    fn a_config_without_the_old_names_is_left_alone() {
        let plain = "[[groups]]\nid = \"picard\"\ntype = \"email\"\n";
        assert!(!is_retired(plain).unwrap());
        assert_eq!(rewrite(plain).unwrap(), plain);
    }
}
