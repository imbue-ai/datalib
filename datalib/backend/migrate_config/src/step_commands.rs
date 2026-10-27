//! The shape from before every step named its program: a step of
//! datalib's own wrote no `command`, and the loader read that as
//! `datalib-step`. The rewrite writes `command = "datalib-step"` on the
//! line after each such step's `function`.
//!
//! A text edit, like `qmd_steps.rs`: every comment and key the file holds
//! survives. A step written as an inline table is left as it is; the
//! loader still reads it.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use toml::Spanned;

const LINE: &str = "command = \"datalib-step\"";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    steps: Vec<StepView>,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    function: Option<Spanned<String>>,
    #[serde(default)]
    command: Option<String>,
}

/// Where each command-less grouped step's `function` line ends.
fn insertions(text: &str) -> Result<Vec<usize>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(view
        .steps
        .iter()
        .filter(|s| s.command.is_none())
        .filter_map(|s| s.function.as_ref())
        .filter_map(|f| {
            let line_start = text[..f.span().start].rfind('\n').map_or(0, |i| i + 1);
            let on_its_own_line = text[line_start..f.span().start]
                .trim_start()
                .starts_with("function");
            on_its_own_line.then(|| {
                text[f.span().end..]
                    .find('\n')
                    .map_or(text.len(), |i| f.span().end + i)
            })
        })
        .collect())
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!insertions(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut at = insertions(text)?;
    // Inserting at the last place first leaves every earlier one where it
    // was.
    at.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = text.to_string();
    for i in at {
        out.insert_str(i, &format!("\n{LINE}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
id = "picard"
type = "slack"

# the captain's log
[[steps]]
group = "picard"
function = "ingest"  # every channel
[steps.params.api]
workspace = "enterprise"

[[steps]]
group = "picard"
function = "render_markdown"
inputs = ["picard/ingest"]

[[steps]]
group = "picard"
function = "keyword_index"
command = "my-indexer --fast"
inputs = ["picard/render_markdown"]
"#;

    #[test]
    fn each_command_less_step_names_datalib_step_and_nothing_else_moves() {
        let out = rewrite(BEFORE).unwrap();
        assert_eq!(
            out,
            BEFORE
                .replace(
                    "function = \"ingest\"  # every channel\n",
                    "function = \"ingest\"  # every channel\ncommand = \"datalib-step\"\n",
                )
                .replace(
                    "function = \"render_markdown\"\n",
                    "function = \"render_markdown\"\ncommand = \"datalib-step\"\n",
                )
        );
        assert!(!is_retired(&out).unwrap());
    }

    #[test]
    fn an_inline_step_is_left_for_the_loader() {
        let inline = "steps = [{ group = \"g\", function = \"ingest\" }]\n";
        assert!(!is_retired(inline).unwrap());
    }
}
