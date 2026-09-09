//! `datalib-migrate-config` — rewrite a `config.toml` from a shape nothing
//! writes any more into the one the wizard writes.
//!
//! The runner still loads the retired shape, with a warning naming this
//! tool, but the editor cannot change it; the rewrite has to live
//! somewhere, and this is that somewhere. One rewrite
//! lives here at a time; when the shape moves again, the next rewrite
//! replaces it. Nothing pre-TOML is convertible any more: a root that still
//! has a `config.yaml` is set up again from the app.
//!
//! The rewrite today: `[[steps]]` written with verbatim ids and
//! `datalib-step download|render <type>` commands — the shape before
//! `[[groups]]` — into groups, with each step declared as `group` +
//! `function`.

pub mod convert;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The shapes this tool can rewrite. One today; the enum stays so the
/// next one has a place to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyFormat {
    /// Steps carrying a verbatim `id` and a `datalib-step` command, from
    /// before `[[groups]]` existed.
    UngroupedSteps,
}

/// Which shape this text is in. Not TOML at all, and already current, are
/// both errors rather than silent no-ops: running the tool on the wrong
/// file is the likeliest mistake, and the message should say so.
pub fn detect(text: &str) -> Result<LegacyFormat> {
    if text.trim().is_empty() || !datalib_dag::config::is_toml(text) {
        bail!(
            "this is not a TOML config. Pre-TOML `config.yaml` roots are no longer \
             convertible — set the root up again from the app, then delete the old file."
        );
    }
    let (cfg, _) = datalib_dag::config::parse_graded(text);
    if convert::needs_grouping(&cfg) {
        return Ok(LegacyFormat::UngroupedSteps);
    }
    bail!("this config is already in the current shape — there is nothing to migrate")
}

pub fn convert(text: &str) -> Result<String> {
    let out = match detect(text)? {
        LegacyFormat::UngroupedSteps => convert::group_steps(text),
    }?;
    // The conversion is value-level, so anything the loader would refuse in
    // the result surfaces here rather than on the next run. Report what the
    // runner rejected and let the message speak.
    verify(&out).context("the converted config does not load")?;
    Ok(out)
}

fn verify(toml_text: &str) -> Result<()> {
    let cfg = datalib_dag::config::parse(toml_text)?;
    let specs = datalib_dag::config::to_specs(&cfg)?;
    datalib_dag::Graph::build(specs)?;
    Ok(())
}

pub fn resolve_input(arg: &Path) -> PathBuf {
    if arg.is_dir() {
        arg.join(datalib_dag::config::CONFIG_FILE_NAME)
    } else {
        arg.to_path_buf()
    }
}

pub fn default_output(input: &Path) -> PathBuf {
    input.with_file_name(datalib_dag::config::CONFIG_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNGROUPED: &str = r#"data_root = "/tmp/dl"

[[steps]]
id = "slack/raw"
name = "Work Slack"
command = "datalib-step download slack_api"
[steps.params.sync]
channels = ["chat-qi"]

[[steps]]
id = "slack/rendered_md"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]

[[steps]]
id = "unified_index/grid"
command = "datalib-step grid_index"
inputs = ["slack/rendered_md"]

[[steps]]
id = "exports/csv"
command = "my-exporter --flag"
inputs = ["slack/rendered_md"]

[[applets]]
id = "unified_index"
command = "datalib-applet unified_index"

[[applets]]
id = "slack_view"
command = "datalib-applet slack"
[applets.params]
tree = "slack/rendered_md"
"#;

    #[test]
    fn detects_the_ungrouped_shape() {
        assert_eq!(detect(UNGROUPED).unwrap(), LegacyFormat::UngroupedSteps);
    }

    /// Running the tool twice is the likeliest mistake, so an
    /// already-converted config says exactly that.
    #[test]
    fn an_already_migrated_config_says_so() {
        let err = detect(&convert(UNGROUPED).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("already"), "{err}");
        // A config with only custom steps has nothing to regroup either.
        let err = detect("[[steps]]\nid = \"x/out\"\ncommand = \"c\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already"), "{err}");
    }

    /// The pre-TOML era is gone from here: a YAML config is refused with
    /// a message that says what to do instead.
    #[test]
    fn yaml_and_empty_input_are_refused() {
        for text in ["", "sources:\n  - name: slack\n", "steps:\n  - id: x\n"] {
            let err = detect(text).unwrap_err().to_string();
            assert!(err.contains("not a TOML config"), "{text:?} -> {err}");
        }
    }

    /// The whole conversion, checked on the text the runner would read:
    /// `convert` verifies its own output loads, so reaching the asserts
    /// proves that much already.
    #[test]
    fn groups_the_built_in_steps_and_leaves_custom_ones_alone() {
        let out = convert(UNGROUPED).unwrap();
        assert!(out.contains("data_root = \"/tmp/dl\""), "{out}");
        assert!(out.contains("[[groups]]"), "{out}");
        assert!(out.contains("id = \"slack\""), "{out}");
        assert!(out.contains("type = \"slack_api\""), "{out}");
        // The name moves from the download step to the group.
        assert_eq!(out.matches("name = \"Work Slack\"").count(), 1, "{out}");
        assert!(
            out.contains("group = \"slack\"\nfunction = \"raw\""),
            "{out}"
        );
        assert!(
            out.contains("group = \"slack\"\nfunction = \"rendered_md\""),
            "{out}"
        );
        assert!(
            out.contains("group = \"unified_index\"\nfunction = \"grid\""),
            "{out}"
        );
        assert!(!out.contains("id = \"slack/raw\""), "{out}");
        assert!(out.contains("channels = [\"chat-qi\"]"), "{out}");
        // A custom step keeps its verbatim id, and the group it is not in.
        assert!(out.contains("id = \"exports/csv\""), "{out}");
        assert!(out.contains("command = \"my-exporter --flag\""), "{out}");
        // Applets are filed under the group their tree or id names.
        let (cfg, diags) = datalib_dag::config::parse_graded(&out);
        assert!(diags.is_empty(), "{diags:?}\n{out}");
        let by_id = |id: &str| cfg.applets.iter().find(|a| a.id == id).unwrap().clone();
        assert_eq!(
            by_id("unified_index").group.as_deref(),
            Some("unified_index")
        );
        assert_eq!(by_id("slack_view").group.as_deref(), Some("slack"));
        assert_eq!(cfg.groups.len(), 2);
        assert_eq!(cfg.steps.len(), 4);
    }

    /// The old wizard named only the render step when the name box was
    /// left blank — `<fetch id> (render markdown)` — so the group's name
    /// must come from the download step, and the render step's only once
    /// that suffix is gone. An unnamed source stays unnamed.
    #[test]
    fn the_groups_name_comes_from_the_download_step_not_the_render_one() {
        let out = convert(
            "[[steps]]\nid = \"slack/rendered_md\"\nname = \"Work Slack (render markdown)\"\n\
             command = \"datalib-step render slack_api\"\ninputs = [\"slack/raw\"]\n\n\
             [[steps]]\nid = \"slack/raw\"\ncommand = \"datalib-step download slack_api\"\n",
        )
        .unwrap();
        assert!(out.contains("name = \"Work Slack\""), "{out}");
        assert!(!out.contains("(render markdown)"), "{out}");

        let out = convert(
            "[[steps]]\nid = \"slack/raw\"\ncommand = \"datalib-step download slack_api\"\n\n\
             [[steps]]\nid = \"slack/rendered_md\"\nname = \"slack/raw (render markdown)\"\n\
             command = \"datalib-step render slack_api\"\ninputs = [\"slack/raw\"]\n",
        )
        .unwrap();
        assert!(!out.contains("name ="), "{out}");
    }

    /// Two steps of one source that disagree about its type cannot share a
    /// group, and guessing which is right would be worse than stopping.
    #[test]
    fn a_source_whose_steps_disagree_on_type_is_refused() {
        let err = convert(
            "[[steps]]\nid = \"a/raw\"\ncommand = \"datalib-step download slack_api\"\n\n\
             [[steps]]\nid = \"a/rendered_md\"\ncommand = \"datalib-step render email\"\n\
             inputs = [\"a/raw\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("slack_api"), "{err}");
        assert!(err.contains("email"), "{err}");
    }

    #[test]
    fn a_directory_resolves_to_the_config_inside_it() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(resolve_input(td.path()), td.path().join("config.toml"));
        // A file path is taken as-is, whatever it's called.
        let f = td.path().join("old.toml");
        std::fs::write(&f, "steps = []\n").unwrap();
        assert_eq!(resolve_input(&f), f);
        assert_eq!(default_output(&f), td.path().join("config.toml"));
    }
}
