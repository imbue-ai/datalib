//! How a `datalib-step` step is written in a config: its `command`, which
//! carries its function and its group's type, and the `reads` and `locks`
//! it needs. The runner knows none of this; the example stanzas,
//! `examples --append` and `check-params` read and write it here.

use clap::Parser;

use crate::function::Function;
use crate::source::RunArgs;

/// The program word a config names datalib-step by.
pub const PROGRAM: &str = "datalib-step";

/// The verbs datalib-step takes, as its `[[programs]]` entry lists them.
pub const VERBS: &[&str] = &["reset", "prepare", "compact", "collect", "check"];

/// The locks datalib-step's steps hold beyond the runner's own, with
/// their slots. `index` bounds how many index steps run at once;
/// `embedder` keeps one embedding model loaded at a time, and two embeds
/// off the search index's vectors at once.
pub const LOCKS: &[(&str, usize)] = &[("index", 2), ("embedder", 1)];

/// The group the time-series index is filed under, and the tree it writes.
pub const TIME_SERIES_INDEX_GROUP: &str = "unified_time_series_index";

/// What a step's command asks datalib-step to do, when the command runs
/// `datalib-step run`; `None` for any other command. `Some(Err)` is a
/// datalib-step step whose words do not parse.
pub fn run_args(command: &str) -> Option<Result<RunArgs, String>> {
    let argv = shlex::split(command)?;
    let (program, rest) = argv.split_first()?;
    let is_ours = program == PROGRAM || program.ends_with(&format!("/{PROGRAM}"));
    if !is_ours || rest.first().map(String::as_str) != Some("run") {
        return None;
    }
    #[derive(Parser)]
    #[command(no_binary_name = true)]
    struct Words {
        #[command(flatten)]
        run: RunArgs,
    }
    Some(
        Words::try_parse_from(&rest[1..])
            .map(|w| w.run)
            .map_err(|e| e.to_string().trim().to_string()),
    )
}

/// `datalib-step run <function> [<type>] [--source-type <type>]`.
pub fn command(function: Function, group_type: Option<&str>, source_type: Option<&str>) -> String {
    let mut out = format!("{PROGRAM} run {function}");
    if let Some(t) = group_type {
        out.push_str(&format!(" {t}"));
    }
    if let Some(s) = source_type {
        out.push_str(&format!(" --source-type {s}"));
    }
    out
}

/// The `reads` a step of `function` under a group of `group_type` writes;
/// `None` for an ingest, which reads nothing. Written even while a fan-in's
/// `inputs` are empty, so the step reads as it should once a source is
/// added.
pub fn reads(function: Function, group_type: Option<&str>) -> Option<&'static str> {
    match function {
        Function::Ingest => None,
        Function::Embed | Function::EmbeddingMap => Some("files"),
        Function::RenderMarkdown if group_type == Some("perseus") => Some("files"),
        _ => Some("pinned"),
    }
}

/// The one lock a step of `function` under a group of `group_type` names,
/// or `None` for the runner's default.
pub fn lock(function: Function, group_type: Option<&str>) -> Option<&'static str> {
    match function {
        Function::Embed => Some("embedder"),
        Function::Ingest => None,
        _ if group_type.is_none() => Some("index"),
        _ => None,
    }
}

/// A whole `[[steps]]` entry for a step of `group`, as the examples and
/// `examples --append` write it.
pub fn entry(group: &str, function: Function, group_type: Option<&str>, inputs: &[&str]) -> String {
    let mut out = format!(
        "[[steps]]\nid = \"{group}/{function}\"\ncommand = \"{}\"\n",
        command(function, group_type, None)
    );
    if function != Function::Ingest {
        let quoted: Vec<String> = inputs.iter().map(|i| format!("\"{i}\"")).collect();
        out.push_str(&format!("inputs = [{}]\n", quoted.join(", ")));
    }
    if let Some(r) = reads(function, group_type) {
        out.push_str(&format!("reads = \"{r}\"\n"));
    }
    if let Some(l) = lock(function, group_type) {
        out.push_str(&format!("locks = [\"{l}\"]\n"));
    }
    out
}

/// The `[[programs]]` entry for datalib-step.
pub fn programs_entry() -> String {
    let verbs: Vec<String> = VERBS.iter().map(|v| format!("\"{v}\"")).collect();
    format!(
        "[[programs]]\ncommand = \"{PROGRAM}\"\nverbs = [{}]\n",
        verbs.join(", ")
    )
}

/// The `[[locks]]` entry declaring `name`, one of [`LOCKS`].
pub fn locks_entry(name: &str) -> Option<String> {
    let (_, slots) = LOCKS.iter().find(|(n, _)| *n == name)?;
    Some(format!("[[locks]]\nname = \"{name}\"\nslots = {slots}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_round_trips_through_its_words() {
        for (f, t, s) in [
            (Function::Ingest, Some("slack"), None),
            (Function::UnifiedGridAndSearchIndex, None, None),
            (Function::RenderMarkdown, Some("diff"), Some("slack")),
        ] {
            let parsed = run_args(&command(f, t, s)).unwrap().unwrap();
            assert_eq!(parsed.function, f.as_str());
            assert_eq!(parsed.group_type.as_deref(), t);
            assert_eq!(parsed.source_type.as_deref(), s);
        }
        assert!(run_args("/opt/datalib/datalib-step run ingest slack").is_some());
        assert!(run_args("datalib-step").is_none());
        assert!(run_args("./bin/fetch run ingest").is_none());
        assert!(run_args("datalib-step run").unwrap().is_err());
        assert!(run_args("datalib-step run a b c").unwrap().is_err());
    }

    /// The datalib-step `[[programs]]` entry names exactly the runner's
    /// verbs, so a typo here is not a verb the runner never sends.
    #[test]
    fn the_verbs_are_the_runners() {
        for v in VERBS {
            assert!(datalib_dag::step::Verb::parse(v).is_some(), "{v}");
        }
    }
}
