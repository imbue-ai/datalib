//! A config's built-in steps' params, held to what a sync would accept
//! before `PUT /api/config` writes it. This server links no provider, so
//! it asks `datalib-step check-params`, the way `datalib-dag --check`
//! does; `datalib_dag::config::params_diagnostics` reads the answer.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_dag::Diagnostic;

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
            s.command.is_none()
                && matches!(s.function.as_deref(), Some("ingest" | "render_markdown"))
        })
}

pub async fn refused_by(step_bin: &Path, text: &str) -> Result<Vec<Diagnostic>> {
    let dir = tempfile::tempdir().context("a directory for the config being checked")?;
    let path = dir.path().join("config.toml");
    tokio::fs::write(&path, text)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    let run = tokio::process::Command::new(step_bin)
        .arg("check-params")
        .arg(&path)
        .kill_on_drop(true)
        .output();
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
    Ok(datalib_dag::config::params_diagnostics(
        text,
        &String::from_utf8_lossy(&out.stdout),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\n\
                        group = \"slack\"\nfunction = \"ingest\"\n\n[steps.params.api]\n\
                        chanels = [\"bridge\"]\n";

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
