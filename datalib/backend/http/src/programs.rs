//! `GET /api/programs`: what each program a config's `[[programs]]` entry
//! lists with the `describe` verb offers "Add source", as it answers
//! `<program> --describe` (`docs/dev/step_protocol.md` §"Describe"). The
//! wizard draws these sources beside the packages' and writes their steps
//! as `<program> <command>`; the probe route runs their probes the same
//! way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::Json;
use datalib_dag::step::Verb;
use datalib_packages::Description;
use serde::Serialize;
use tokio::process::Command;

use crate::AppState;

/// A program that has not described itself by now is not going to.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct ProgramView {
    /// The `[[programs]]` entry's `command`: the word a step's command
    /// starts with to run it.
    pub command: String,
    pub description: Option<Description>,
    /// Why it has no description: not found, or what it said.
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProgramsResponse {
    pub programs: Vec<ProgramView>,
}

pub async fn list(State(s): State<AppState>) -> Json<ProgramsResponse> {
    Json(ProgramsResponse {
        programs: described(&s.root).await,
    })
}

/// Every describing program of `root`'s config, in the order the config
/// lists them.
pub async fn described(root: &Path) -> Vec<ProgramView> {
    let text =
        std::fs::read_to_string(datalib_dag::config::root_config_path(root)).unwrap_or_default();
    let cfg = datalib_dag::config::check_text(&text).cfg;
    let mut out = Vec::new();
    for p in &cfg.programs {
        if !p.verbs.iter().any(|v| v == Verb::Describe.as_str()) {
            continue;
        }
        let command = p.command.trim().to_string();
        let (description, error) = match describe(root, &command).await {
            Ok(d) => (Some(d), None),
            Err(e) => {
                tracing::warn!(program = %command, "describe: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        };
        out.push(ProgramView {
            command,
            description,
            error,
        });
    }
    out
}

/// What each binary said, by its path and when it was last changed: a
/// program asked again is asked only once it has changed.
static DESCRIBED: LazyLock<Mutex<HashMap<(PathBuf, SystemTime), Description>>> =
    LazyLock::new(Default::default);

async fn describe(root: &Path, program: &str) -> anyhow::Result<Description> {
    let path = resolve(root, program)
        .ok_or_else(|| anyhow::anyhow!("no program {program:?} found on the steps' PATH"))?;
    let changed = std::fs::metadata(&path)?.modified()?;
    let key = (path.clone(), changed);
    if let Some(d) = DESCRIBED.lock().expect("describe cache").get(&key) {
        return Ok(d.clone());
    }
    let mut cmd = Command::new(&path);
    cmd.arg(Verb::Describe.flag())
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    datalib_env::inherit_only(cmd.as_std_mut(), datalib_env::Child::Step, &[]);
    let out = tokio::time::timeout(DESCRIBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!("`{program} --describe` did not answer within 30 seconds")
        })??;
    if !out.status.success() {
        anyhow::bail!(
            "`{program} --describe` failed ({}): {}",
            out.status,
            crate::connect::tail(&String::from_utf8_lossy(&out.stderr))
        );
    }
    let d = Description::parse(&String::from_utf8_lossy(&out.stdout))?;
    DESCRIBED
        .lock()
        .expect("describe cache")
        .insert(key, d.clone());
    Ok(d)
}

/// Where `program`, a command's first word, is: a path with a `/` as it
/// is written, relative to the data root; a bare word on the PATH a step
/// gets, the step-binary directory and the library's `system/bin` first.
pub fn resolve(root: &Path, program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = Path::new(program);
        let p = if p.is_relative() {
            root.join(p)
        } else {
            p.to_path_buf()
        };
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH");
    crate::binaries::resolve_binary_dir()
        .into_iter()
        .chain([root.join(datalib_runtime::layout::BIN_DIR_REL)])
        .chain(path.iter().flat_map(std::env::split_paths))
        .map(|dir| dir.join(program))
        .find(|cand| cand.is_file())
}
