//! A config's datalib-step steps' params, held to what a sync would
//! accept before `PUT /api/config` writes it. This server links no
//! provider, so it asks `datalib-step check-params`, and [`diagnostics`]
//! reads the answer. Only `datalib-step`'s steps are checked: see
//! `docs/dev/plans/decoupling_followups.md` item 1.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_dag::{Diagnostic, EntryRef, Severity};

/// `check-params` plans and runs nothing; one that has not answered by
/// now is stuck, and the save goes on without it.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The diagnostics for every built-in step whose params a sync would
/// refuse. None when the config has no built-in ingest or render step,
/// or when no `datalib-step` can be found or run, which is logged: the
/// save still goes through, and the step fails when it runs.
pub async fn refused(text: &str) -> Vec<Diagnostic> {
    if !has_builtin_params(text) {
        return Vec::new();
    }
    let Some(step_bin) = crate::binaries::resolve_step_bin() else {
        tracing::warn!("no datalib-step found; the config's step params were not checked");
        return Vec::new();
    };
    match refused_by(&step_bin, text).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("the config's step params were not checked: {e:#}");
            Vec::new()
        }
    }
}

fn has_builtin_params(text: &str) -> bool {
    datalib_dag::config::check_text(text)
        .cfg
        .steps
        .iter()
        .any(|s| {
            matches!(
                crate::step_program::datalib_step_function(&s.command).as_deref(),
                Some("ingest" | "render_markdown")
            )
        })
}

pub async fn refused_by(step_bin: &Path, text: &str) -> Result<Vec<Diagnostic>> {
    let dir = tempfile::tempdir().context("a directory for the config being checked")?;
    let path = dir.path().join("config.toml");
    tokio::fs::write(&path, text)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    let mut cmd = tokio::process::Command::new(step_bin);
    cmd.arg("check-params").arg(&path).kill_on_drop(true);
    datalib_env::inherit_only(cmd.as_std_mut(), datalib_env::Child::Step, &[]);
    let run = cmd.output();
    let out = tokio::time::timeout(TIMEOUT, run)
        .await
        .with_context(|| {
            format!(
                "`{} check-params` took over {TIMEOUT:?}",
                step_bin.display()
            )
        })?
        .with_context(|| format!("run {}", step_bin.display()))?;
    anyhow::ensure!(
        out.status.success(),
        "`{} check-params` failed: {}",
        step_bin.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(diagnostics(text, &String::from_utf8_lossy(&out.stdout)))
}

/// What `datalib-step check-params` printed for this config, as
/// diagnostics against its text: one `Rejected` per step whose params a
/// sync would refuse, at the line of the key the message names when it
/// names one.
fn diagnostics(text: &str, check_params_stdout: &str) -> Vec<Diagnostic> {
    check_params_stdout
        .lines()
        .filter_map(|line| {
            let problem: serde_json::Value = serde_json::from_str(line).ok()?;
            let step = problem.get("step")?.as_str()?;
            let message = problem.get("message")?.as_str()?;
            let key = message
                .split_once("unknown field `")
                .and_then(|(_, rest)| rest.split_once('`'))
                .map(|(key, _)| key);
            let mut d = Diagnostic::new(
                Severity::Rejected,
                format!("a sync would refuse its params: {message}"),
            )
            .with_help("`datalib-step help <type> <method>` lists every option a source takes");
            d.entry = Some(EntryRef::step_id(step));
            if let Some(header) = datalib_dag::config::step_header_span(text, step) {
                let span = match key {
                    Some(key) => params_key_span(text, header, key),
                    None => header,
                };
                d.set_span(text, span);
            }
            Some(d)
        })
        .collect()
}

/// Where `key = …` sits in a step's body, its `[steps.params…]`
/// sub-tables included. The step's header when it is not found.
fn params_key_span(
    text: &str,
    header: std::ops::Range<usize>,
    key: &str,
) -> std::ops::Range<usize> {
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

    const TEXT: &str = "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\n\
                        id = \"slack/ingest\"\ncommand = \"datalib-step run ingest slack\"\n\n\
                        [steps.params.api]\nchanels = [\"bridge\"]\n";

    // A stand-in for `datalib-step`: answers `check-params` with what the
    // real one prints for TEXT, so this tests the asking and the reading,
    // and `step_tests::check_params` the real answer.
    fn fake_step(dir: &Path, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("datalib-step");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn a_refused_param_comes_back_at_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let step = fake_step(
            dir.path(),
            r#"[ "$1" = check-params ] && [ -f "$2" ] || exit 9
echo '{"step":"slack/ingest","message":"unknown field `chanels`"}'"#,
        );
        let found = refused_by(&step, TEXT).await.unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, Some(10), "{found:?}");
    }

    #[tokio::test]
    async fn a_step_program_that_fails_is_an_error_not_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        let step = fake_step(dir.path(), "echo 'not a config' >&2; exit 1");
        let err = refused_by(&step, TEXT).await.unwrap_err();
        assert!(format!("{err:#}").contains("not a config"), "{err:#}");
    }

    #[test]
    fn a_config_with_only_custom_commands_is_not_asked_about() {
        assert!(!has_builtin_params(
            "[[steps]]\nid = \"mine/thing\"\ncommand = \"true\"\n"
        ));
        assert!(has_builtin_params(TEXT));
    }
}
