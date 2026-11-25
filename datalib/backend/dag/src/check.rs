//! `--check`: before anything runs, ask each step whose program takes the
//! verb whether it would run its params as written
//! (`docs/dev/step_protocol.md` § Check). `datalib-pipeline check` and the
//! app's config save ask; the loop never does.

use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use futures::StreamExt as _;

use crate::config::ConfigCheck;
use crate::diagnostics::{Diagnostic, EntryRef, Severity};
use crate::scheduler::Runner;
use crate::step::Verb;

/// How long one step may take to answer. A check reads its params and
/// opens nothing, so one that has not answered by then is stuck.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How many steps are asked at once.
const AT_ONCE: usize = 8;

/// What a step said when asked.
#[derive(Debug, PartialEq)]
enum Answer {
    /// It would run its params as written, warning about these.
    Accepted(Vec<String>),
    /// It would refuse them, saying why.
    Refused(String),
    /// It did not answer within [`TIMEOUT`], or could not be started.
    Unanswered(String),
}

/// What every step of `checked` whose program takes `--check` says of its
/// params, as diagnostics against `checked.text`: a refusal is
/// [`Severity::Rejected`] at the step's line, or at the params key it
/// names; a warning, and a step that did not answer or could not be
/// started, are
/// [`Severity::Warning`]. `binary_dir` is the host's, as for a run.
pub async fn check_params(
    checked: &ConfigCheck,
    data_root: &Path,
    binary_dir: Option<&Path>,
) -> Vec<Diagnostic> {
    let asked: Vec<String> = checked
        .graph
        .steps
        .iter()
        .filter(|s| s.verbs.contains(&Verb::Check))
        .map(|s| s.id.to_string())
        .collect();
    if asked.is_empty() {
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
    let mut asks = Vec::new();
    for step in &asked {
        asks.push(ask(&runner, &checked.graph, step));
    }
    let mut answers: Vec<(String, Answer)> = futures::stream::iter(asks)
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    let mut found: Vec<Diagnostic> = answers
        .into_iter()
        .flat_map(|(step, answer)| diagnostics(&checked.text, &step, answer))
        .collect();
    // In file order, as the loader's are: the order someone fixing them
    // reads.
    found.sort_by_key(|d| d.span.map(|(start, _)| start));
    found
}

async fn ask(runner: &Runner, graph: &crate::graph::Graph, step: &str) -> (String, Answer) {
    let step = step.to_string();
    let asked = runner.invoke_verb(graph, &step, Verb::Check);
    let answer = match tokio::time::timeout(TIMEOUT, asked).await {
        Ok(Ok(Ok(warnings))) => Answer::Accepted(warnings),
        Ok(Ok(Err(not_started))) => Answer::Unanswered(not_started),
        Ok(Err(why)) => Answer::Refused(why),
        Err(_) => Answer::Unanswered(format!("--check took over {TIMEOUT:?}")),
    };
    (step, answer)
}

fn diagnostics(text: &str, step: &str, answer: Answer) -> Vec<Diagnostic> {
    let header = crate::config::step_header_span(text, step);
    let place = |d: Diagnostic, span: Option<Range<usize>>| {
        let d = d.at_entry(EntryRef::step_id(step));
        match span {
            Some(span) => d.at_span(text, span),
            None => d,
        }
    };
    match answer {
        Answer::Accepted(warnings) => warnings
            .into_iter()
            .map(|w| place(Diagnostic::new(Severity::Warning, w), header.clone()))
            .collect(),
        Answer::Refused(why) => {
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
        Answer::Unanswered(why) => vec![place(
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

    /// A program that answers `--check` from a shell script in the data
    /// root, so the asking, the reading and the placing are tested without
    /// `datalib-step`.
    const TEXT: &str = "[[programs]]\ncommand = \"sh\"\nverbs = [\"check\"]\n\n\
                        [[steps]]\nid = \"weather/ingest\"\ncommand = \"sh step.sh\"\n\
                        [steps.params]\nstaton = \"KSFO\"\n\n\
                        [[steps]]\nid = \"weather/unasked\"\ncommand = \"true\"\n";

    async fn ask_with(text: &str, script: &str) -> Vec<Diagnostic> {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("step.sh"), script).unwrap();
        let checked = crate::config::check_text(text);
        assert!(checked.is_clean(), "{:?}", checked.diagnostics);
        check_params(&checked, root.path(), None).await
    }

    #[tokio::test]
    async fn a_refusal_comes_back_at_the_key_it_names() {
        let found = ask_with(
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
        let found = ask_with(
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

    /// A program that is not there cannot answer, and a save is not refused
    /// for it: the step will fail when it runs, saying so.
    #[tokio::test]
    async fn a_program_that_cannot_start_is_a_warning() {
        let text = TEXT
            .replace("command = \"sh step.sh\"", "command = \"sh-is-not-here\"")
            .replace("command = \"sh\"", "command = \"sh-is-not-here\"");
        let found = ask_with(&text, "").await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Warning, "{found:?}");
        assert!(found[0].message.contains("not checked"), "{found:?}");
    }

    #[tokio::test]
    async fn a_program_that_does_not_take_the_verb_is_not_asked() {
        let text = "[[steps]]\nid = \"a\"\ncommand = \"sh step.sh\"\n";
        assert!(ask_with(text, "exit 1\n").await.is_empty());
    }
}
