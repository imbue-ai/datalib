//! `datalib-migrate-config` — rewrite a `config.toml` from a shape nothing
//! writes any more into the one the wizard writes.
//!
//! The runner refuses a retired shape, naming this tool, and the editor
//! cannot change it; the rewrite has to live somewhere, and this is that
//! somewhere. One rewrite lives here at a time — "any earlier shape to
//! this one" — and when the shape moves again the next rewrite replaces
//! it. Nothing pre-TOML is convertible any more: a root that still has a
//! `config.yaml` is set up again from the app.
//!
//! What the rewrite covers today is the header of `convert.rs`: the
//! `datalib-step download <type>` command lines, the `_api` / `_backup`
//! type words, and the `sync` / `common.input_path` / `common.raw_path`
//! params.

pub mod convert;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub use convert::Retired as LegacyFormat;

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
    if let Some(shape) = convert::retired_shape(text)? {
        return Ok(shape);
    }
    bail!("this config is already in the current shape — there is nothing to migrate")
}

pub fn convert(text: &str) -> Result<String> {
    detect(text)?;
    let out = convert::rewrite(text)?;
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

    /// The shape slice 1 of the groups plan wrote: grouped, but with the
    /// function and the provider still on the command line and the old
    /// function names.
    const GROUPED_WITH_COMMANDS: &str = r#"
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack_api"

[[steps]]
group = "slack"
function = "raw"
command = "datalib-step download slack_api"

[[steps]]
group = "slack"
function = "rendered_md"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid"
command = "datalib-step grid_index"
inputs = ["slack/rendered_md"]

[[steps]]
group = "unified_index"
function = "qmd"
command = "datalib-step qmd_index"
inputs = ["slack/rendered_md"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
"#;

    /// The shape slice 2 wrote: command-less, but with the type spelled
    /// for the method and the method as a `sync` table or a
    /// `common.input_path`.
    const GROUPED_PRE_METHOD_TABLES: &str = r#"
[[groups]]
id = "slack"
type = "slack_api"

[[steps]]
group = "slack"
function = "ingest"
[steps.params.sync]
channels = ["chat-qi"]

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

[[groups]]
id = "claude-export"
type = "claude_export"

[[steps]]
group = "claude-export"
function = "ingest"
[steps.params.common]
input_path = "~/claude-export"
raw_path = "/big/disk/claude-export/ingest"
blob_size_limit_bytes = 5000000

[[steps]]
group = "claude-export"
function = "render_markdown"
inputs = ["claude-export/ingest"]
[steps.params.common]
raw_path = "/big/disk/claude-export/ingest"

[[groups]]
id = "mail"
type = "email"

[[steps]]
group = "mail"
function = "ingest"
[steps.params]
only_extract_labels = ["Inbox"]
[steps.params.common]
input_path = "~/Takeout/mail.mbox"
[steps.params.mbox]
display_name = "Me"

[[groups]]
id = "signal"
type = "signal_backup"

[[steps]]
group = "signal"
function = "ingest"
[steps.params.sync]
snapshot_dir = "~/backups/Signal"
aep_env_var = "AEP"

[[groups]]
id = "takeout"
type = "google_takeout"

[[steps]]
group = "takeout"
function = "ingest"
[steps.params.common]
input_path = "~/Takeout"
[steps.params.sync]
google_chat = true

[[groups]]
id = "contacts"
type = "carddav"

[[steps]]
group = "contacts"
function = "ingest"
[steps.params.sync]
server_url = "https://contacts.icloud.com/"

[[groups]]
id = "pdfs"
type = "pdf"

[[steps]]
group = "pdfs"
function = "ingest"
[steps.params]
ignore = ["drafts/**"]
[steps.params.common]
input_path = "~/Documents"

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["slack/render_markdown"]
"#;

    #[test]
    fn detects_each_retired_shape() {
        assert_eq!(detect(UNGROUPED).unwrap(), LegacyFormat::StepSubcommands);
        assert_eq!(
            detect(GROUPED_WITH_COMMANDS).unwrap(),
            LegacyFormat::StepSubcommands
        );
        assert_eq!(
            detect(GROUPED_PRE_METHOD_TABLES).unwrap(),
            LegacyFormat::TypesAndMethodTables
        );
    }

    /// The type and method-table rewrite, per provider shape: a `sync`
    /// table renamed, a `common.input_path` moved into a method table's
    /// `path` (merging with a table that was already there), the feed
    /// toggles folded into `export`, and `common` dropped once empty.
    #[test]
    fn renames_types_and_moves_methods_into_their_tables() {
        let out = convert(GROUPED_PRE_METHOD_TABLES).unwrap();
        // `carddav` is still a word in the output — as contacts' method
        // table, not as a type.
        for old in ["slack_api", "claude_export", "signal_backup", "carddav"] {
            assert!(
                !out.contains(&format!("type = \"{old}\"")),
                "{old} survived: {out}"
            );
        }
        assert!(!out.contains("input_path"), "{out}");
        assert!(!out.contains("raw_path"), "{out}");
        assert!(!out.contains("[steps.params.sync]"), "{out}");
        assert!(out.contains("type = \"slack\""), "{out}");
        assert!(
            out.contains("[steps.params.api]\nchannels = [\"chat-qi\"]"),
            "{out}"
        );
        assert!(out.contains("type = \"claude\""), "{out}");
        assert!(
            out.contains("[steps.params.export]\npath = \"~/claude-export\""),
            "{out}"
        );
        // The other `common` key stays where it was.
        assert!(
            out.contains("[steps.params.common]\nblob_size_limit_bytes = 5000000"),
            "{out}"
        );
        assert!(
            out.contains(
                "[steps.params.mbox]\ndisplay_name = \"Me\"\npath = \"~/Takeout/mail.mbox\""
            ),
            "{out}"
        );
        assert!(out.contains("only_extract_labels = [\"Inbox\"]"), "{out}");
        assert!(out.contains("type = \"signal\""), "{out}");
        assert!(
            out.contains(
                "[steps.params.backup]\naep_env_var = \"AEP\"\npath = \"~/backups/Signal\""
            ),
            "{out}"
        );
        assert!(
            out.contains("[steps.params.export]\ngoogle_chat = true\npath = \"~/Takeout\""),
            "{out}"
        );
        assert!(out.contains("type = \"contacts\""), "{out}");
        assert!(out.contains("[steps.params.carddav]\nserver_url"), "{out}");
        assert!(out.contains("ignore = [\"drafts/**\"]"), "{out}");
        assert!(
            out.contains("[steps.params.fswalk]\npath = \"~/Documents\""),
            "{out}"
        );
        let (cfg, diags) = datalib_dag::config::parse_graded(&out);
        assert!(diags.is_empty(), "{diags:?}\n{out}");
        assert_eq!(cfg.groups.len(), 8);
        // Running it again finds nothing to do.
        let err = detect(&out).unwrap_err().to_string();
        assert!(err.contains("already"), "{err}");
    }

    /// A perseus ingest step's staged tree has no home in the new shape,
    /// and the migrator says where it goes rather than dropping it.
    #[test]
    fn a_perseus_input_path_on_the_ingest_step_is_refused_with_directions() {
        let err = convert(
            "[[groups]]\nid = \"p\"\ntype = \"perseus\"\n\n[[steps]]\ngroup = \"p\"\n\
             function = \"ingest\"\n[steps.params.common]\ninput_path = \"/tei\"\n",
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("render"), "{err}");
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
        assert!(out.contains("type = \"slack\""), "{out}");
        // The name moves from the download step to the group.
        assert_eq!(out.matches("name = \"Work Slack\"").count(), 1, "{out}");
        assert!(
            out.contains("group = \"slack\"\nfunction = \"ingest\""),
            "{out}"
        );
        assert!(
            out.contains("group = \"slack\"\nfunction = \"render_markdown\""),
            "{out}"
        );
        assert!(
            out.contains("group = \"unified_index\"\nfunction = \"grid_index\""),
            "{out}"
        );
        assert!(!out.contains("id = \"slack/raw\""), "{out}");
        assert!(
            !out.contains("datalib-step"),
            "the provider word leaves: {out}"
        );
        // …and the method table with it, under its current name.
        assert!(
            out.contains("[steps.params.api]\nchannels = [\"chat-qi\"]"),
            "{out}"
        );
        assert!(!out.contains("sync"), "{out}");
        // Everything that named an old id follows it to the new one.
        assert!(out.contains("inputs = [\"slack/ingest\"]"), "{out}");
        assert!(out.contains("tree = \"slack/render_markdown\""), "{out}");
        assert!(!out.contains("rendered_md"), "{out}");
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

    /// The grouped-with-commands shape rewrites the same way: the groups
    /// are kept as declared, the functions are renamed, and the commands
    /// go.
    #[test]
    fn a_grouped_config_with_subcommands_loses_them_and_renames_its_functions() {
        let out = convert(GROUPED_WITH_COMMANDS).unwrap();
        assert!(!out.contains("datalib-step"), "{out}");
        assert!(
            !out.contains("\"raw\"") && !out.contains("rendered_md"),
            "{out}"
        );
        assert!(out.contains("function = \"qmd_index\""), "{out}");
        assert!(
            out.contains("inputs = [\"slack/render_markdown\"]"),
            "{out}"
        );
        let (cfg, diags) = datalib_dag::config::parse_graded(&out);
        assert!(diags.is_empty(), "{diags:?}\n{out}");
        assert_eq!(cfg.groups.len(), 2);
        assert_eq!(cfg.groups[0].name.as_deref(), Some("Work Slack"));
        assert_eq!(cfg.groups[0].r#type.as_deref(), Some("slack"));
        assert!(cfg.steps.iter().all(|s| s.command.is_none()), "{out}");
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
        assert!(err.contains("slack"), "{err}");
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
