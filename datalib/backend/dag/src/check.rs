//! `--check`: before anything runs, run the command of each step whose
//! program takes the verb with `--check` appended, and read from what it
//! reports whether it would run its params as written
//! (`docs/dev/step_protocol.md` § Check). `datalib-pipeline check` and the
//! app's config save do this; the loop never does.

use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;

use crate::config::ConfigCheck;
use crate::diagnostics::{Diagnostic, EntryRef, Severity};
use crate::scheduler::Runner;
use crate::step::Verb;

/// How long one step's `--check` process may take to exit. A check reads
/// its params and opens nothing, so one still running by then is stuck.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How many `--check` processes run at once.
const AT_ONCE: usize = 8;

/// What a step's `--check` process reported.
#[derive(Debug, PartialEq)]
enum CheckResult {
    /// It would run its params as written, warning about these.
    Accepted(Vec<String>),
    /// It would refuse them, saying why.
    Refused(String),
    /// It did not exit within [`TIMEOUT`], or could not be started.
    NoResult(String),
}

/// What every step of `checked` whose program takes `--check` says of its
/// params, as diagnostics against `checked.text`: a refusal is
/// [`Severity::Rejected`] at the step's line, or at the params key it
/// names; a warning, and a step whose process did not exit in time or
/// could not be started, are
/// [`Severity::Warning`]. `binary_dir` is the host's, as for a run.
pub async fn check_params(
    checked: &ConfigCheck,
    data_root: &Path,
    binary_dir: Option<&Path>,
) -> Vec<Diagnostic> {
    let checked_steps: Vec<String> = checked
        .graph
        .steps
        .iter()
        .filter(|s| s.verbs.contains(&Verb::Check))
        .map(|s| s.id.to_string())
        .collect();
    if checked_steps.is_empty() {
        return Vec::new();
    }
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_string();
    let env = match crate::supervisor::host::step_env(&checked.cfg, binary_dir, &[], &now, "check")
    {
        Ok(env) => env,
        Err(e) => {
            return vec![Diagnostic::new(
                Severity::Warning,
                format!("no step's params were checked: {e:#}"),
            )]
        }
    };
    let runner = Runner::new(data_root).child_env(env.vars);
    // Collected first: a stream over a closure that borrows is not one an
    // axum handler's future can hold.
    let mut checks = Vec::new();
    for step in &checked_steps {
        checks.push(run_check(&runner, &checked.graph, step));
    }
    let mut results: Vec<(String, CheckResult)> = futures::stream::iter(checks)
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    results.sort_by(|a, b| a.0.cmp(&b.0));
    let mut found: Vec<Diagnostic> = results
        .into_iter()
        .flat_map(|(step, result)| diagnostics(&checked.text, &step, result))
        .collect();
    // In file order, as the loader's are: the order someone fixing them
    // reads.
    found.sort_by_key(|d| d.span.map(|(start, _)| start));
    found
}

async fn run_check(
    runner: &Runner,
    graph: &crate::graph::Graph,
    step: &str,
) -> (String, CheckResult) {
    let step = step.to_string();
    let check = runner.invoke_verb(graph, &step, Verb::Check);
    let result = match tokio::time::timeout(TIMEOUT, check).await {
        Ok(Ok(Ok(warnings))) => CheckResult::Accepted(warnings),
        Ok(Ok(Err(not_started))) => CheckResult::NoResult(not_started),
        Ok(Err(why)) => CheckResult::Refused(why),
        Err(_) => CheckResult::NoResult(format!("--check took over {TIMEOUT:?}")),
    };
    (step, result)
}

fn diagnostics(text: &str, step: &str, result: CheckResult) -> Vec<Diagnostic> {
    let header = crate::config::step_header_span(text, step);
    let place = |d: Diagnostic, span: Option<Range<usize>>| {
        let d = d.at_entry(EntryRef::step_id(step));
        match span {
            Some(span) => d.at_span(text, span),
            None => d,
        }
    };
    match result {
        CheckResult::Accepted(warnings) => warnings
            .into_iter()
            .map(|w| place(Diagnostic::new(Severity::Warning, w), header.clone()))
            .collect(),
        CheckResult::Refused(why) => {
            let span = header.map(|h| match unknown_key(&why) {
                Some(key) => params_key_span(text, h, key),
                None => h,
            });
            let d = Diagnostic::new(
                Severity::Rejected,
                format!("its program would refuse its params: {why}"),
            );
            vec![place(d, span)]
        }
        CheckResult::NoResult(why) => vec![place(
            Diagnostic::new(
                Severity::Warning,
                format!("its params were not checked: {why}"),
            ),
            header,
        )],
    }
}

/// The key serde names in an "unknown field `chanels`" refusal.
fn unknown_key(why: &str) -> Option<&str> {
    why.split_once("unknown field `")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(key, _)| key)
}

/// Where `key = …` sits in a step's body, its `[steps.params…]`
/// sub-tables included. The step's header when it is not found.
fn params_key_span(text: &str, header: Range<usize>, key: &str) -> Range<usize> {
    let mut at = header.end;
    for line in text[header.end..].split_inclusive('\n') {
        let line_start = at;
        at += line.len();
        if line.trim_start().starts_with("[[") {
            break;
        }
        let mut from = 0;
        while let Some(i) = line[from..].find(key) {
            let start = from + i;
            let end = start + key.len();
            let word_before = line[..start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            if !word_before && line[end..].trim_start().starts_with('=') {
                return line_start + start..line_start + end;
            }
            from = end;
        }
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that handles `--check` with a shell script in the data
    /// root, so running it, reading what it reports and placing the
    /// diagnostics are tested without `datalib-step`.
    const TEXT: &str = "[[programs]]\ncommand = \"sh\"\nverbs = [\"check\"]\n\n\
                        [[steps]]\nid = \"weather/ingest\"\ncommand = \"sh step.sh\"\n\
                        [steps.params]\nstaton = \"KSFO\"\n\n\
                        [[steps]]\nid = \"weather/unasked\"\ncommand = \"true\"\n";

    async fn check_with(text: &str, script: &str) -> Vec<Diagnostic> {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("step.sh"), script).unwrap();
        let checked = crate::config::check_text(text);
        assert!(checked.is_clean(), "{:?}", checked.diagnostics);
        check_params(&checked, root.path(), None).await
    }

    #[tokio::test]
    async fn a_refusal_comes_back_at_the_key_it_names() {
        let found = check_with(
            TEXT,
            "[ \"$1\" = --check ] || exit 9\necho 'unknown field `staton`' >&2\nexit 1\n",
        )
        .await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Rejected);
        assert_eq!(found[0].id(), Some("weather/ingest"));
        assert_eq!(found[0].line, Some(9), "{found:?}");
        assert!(found[0].message.contains("staton"), "{found:?}");
    }

    #[tokio::test]
    async fn warnings_in_the_outcome_come_back_as_warnings_and_params_reach_it() {
        let found = check_with(
            TEXT,
            "case \"$DATALIB_DAG_PARAMS\" in *KSFO*) ;; *) exit 9;; esac\n\
             echo '{\"event\":\"outcome\",\"warnings\":[\"staton does nothing\"]}'\n",
        )
        .await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Warning);
        assert_eq!(found[0].message, "staton does nothing");
        assert_eq!(found[0].line, Some(5), "at the step's header");
    }

    /// A program that is not there cannot be started, and a save is not
    /// refused for it: the step will fail when it runs, saying so.
    #[tokio::test]
    async fn a_program_that_cannot_start_is_a_warning() {
        let text = TEXT
            .replace("command = \"sh step.sh\"", "command = \"sh-is-not-here\"")
            .replace("command = \"sh\"", "command = \"sh-is-not-here\"");
        let found = check_with(&text, "").await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Warning, "{found:?}");
        assert!(found[0].message.contains("not checked"), "{found:?}");
    }

    #[tokio::test]
    async fn a_program_that_does_not_take_the_verb_is_not_run_with_it() {
        let text = "[[steps]]\nid = \"a\"\ncommand = \"sh step.sh\"\n";
        assert!(check_with(text, "exit 1\n").await.is_empty());
    }
}
