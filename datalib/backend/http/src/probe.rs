//! `POST /api/probe` and `GET /api/probe/{id}`: the probe of a source
//! type (`docs/dev/step_protocol.md` §"Probe") as a job the wizard
//! polls, so a list that pages for a minute can say how far it has got.
//! "Check connection" asks for the account alone; a picker's "Load" asks
//! for one list.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use datalib_probe::issue::Failure;
use datalib_probe::{ProbeList, ProbeProgress};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::connect::{err, latchkey_gateway, tail, validated_type};
use crate::AppState;

/// A probe that has not answered by now is not going to.
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    /// The group's `type`: the provider word (`slack`, `email`, …).
    #[serde(rename = "type")]
    pub source_type: String,
    /// The provider's **download** params, exactly as they would be
    /// written under `[steps.params]`. Download-shaped even when the
    /// wizard is filling in a render step: a render step's own params
    /// hold no credentials, and the labels its filter can name are the
    /// ones the account has.
    #[serde(default)]
    pub params: Value,
    /// The list to load as well; absent for "Check connection".
    #[serde(default)]
    pub list: Option<ProbeList>,
}

/// How one probe is going. The UI switches on these words, and the
/// poll endpoint reaps a probe as soon as it is no longer `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeState {
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeStatus {
    pub id: String,
    pub status: ProbeState,
    /// How far a list has got, once it has said.
    pub progress: Option<ProbeProgress>,
    /// The `ProbeReport`, once `Ok`.
    pub report: Option<Value>,
    /// What went wrong, once `Failed`: its kind, and the step's error
    /// chain for the details.
    pub failure: Option<Failure>,
}

type Slot = Arc<Mutex<ProbeStatus>>;

/// The probes in flight, by id. A global for the reason `connect.rs`
/// keeps its login attempts in one: a job outlives the request that
/// started it, and `AppState` is cloned per request.
fn jobs() -> &'static Mutex<HashMap<String, Slot>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn start_probe(
    State(s): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> Result<Json<ProbeStatus>, (StatusCode, Json<Value>)> {
    let source_type = validated_type(&req.source_type)?;
    let mut cmd = probe_command(&s.root, &source_type).await?;
    if let Some(list) = req.list {
        cmd.arg("--list").arg(list.as_str());
    }
    let params = serde_json::to_string(&req.params).unwrap_or_else(|_| "{}".to_string());
    // In the environment, as the runner hands a step its params: the
    // wizard's typed credentials are in here, and argv is readable by
    // every user on the machine.
    cmd.env(datalib_env::DATALIB_DAG_PARAMS, &params)
        .current_dir(s.root.as_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let named = datalib_dag::subprocess::env_vars_named_in(&params);
    datalib_env::inherit_only(cmd.as_std_mut(), datalib_env::Child::Step, &named);
    let child = cmd
        .spawn()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}")))?;

    let id = uuid::Uuid::new_v4().to_string();
    let status = ProbeStatus {
        id: id.clone(),
        status: ProbeState::Running,
        progress: None,
        report: None,
        failure: None,
    };
    let slot = Arc::new(Mutex::new(status.clone()));
    jobs()
        .lock()
        .expect("probe jobs mutex")
        .insert(id, slot.clone());
    tokio::spawn(async move {
        run(child, &slot, &source_type).await;
    });
    Ok(Json(status))
}

/// The probe of `source_type`, from whoever offers the type: the config's
/// packages first, as "Add source" draws a package's form before a
/// program's, then the programs that describe their sources.
async fn probe_command(
    root: &std::path::Path,
    source_type: &str,
) -> Result<Command, (StatusCode, Json<Value>)> {
    let text =
        std::fs::read_to_string(datalib_dag::config::root_config_path(root)).unwrap_or_default();
    let packages: Vec<(String, datalib_packages::Manifest)> =
        datalib_dag::config::check_text(&text)
            .cfg
            .packages
            .iter()
            .filter_map(|p| Some((p.dir_rel(), datalib_packages::installed(root, &p.sha256)?)))
            .collect();
    let programs = crate::programs::described(root).await;
    let offers = packages
        .iter()
        .map(|(dir, m)| Offer {
            by: format!("the package {:?}", m.name),
            prefix: format!("{dir}/"),
            sources: &m.sources,
        })
        .chain(programs.iter().filter_map(|p| {
            Some(Offer {
                by: format!("the program {:?}", p.command),
                prefix: format!("{} ", p.command),
                sources: &p.description.as_ref()?.sources,
            })
        }));
    let command = probe_of(offers, source_type)?;
    let argv = shlex::split(&command)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("the probe command {command:?} has unbalanced quoting"),
            )
        })?;
    let program = crate::programs::resolve(root, &argv[0]).ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("the probe's program {:?} is not there", argv[0]),
        )
    })?;
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..]);
    Ok(cmd)
}

/// Sources one package or program offers, and what its commands follow:
/// a package's directory, or a program's own word.
struct Offer<'a> {
    by: String,
    prefix: String,
    sources: &'a [datalib_packages::manifest::SourceTemplate],
}

/// The probe command of the first offer of `source_type`. Refused when
/// nothing offers the type, and when the first offer has no probe, rather
/// than asking a later one: that would ask a service the steps of the
/// source being added never reach.
fn probe_of<'a>(
    offers: impl IntoIterator<Item = Offer<'a>>,
    source_type: &str,
) -> Result<String, (StatusCode, Json<Value>)> {
    let found = offers.into_iter().find_map(|o| {
        let source = o.sources.iter().find(|s| s.source_type == source_type)?;
        Some((o, source))
    });
    let Some((offer, source)) = found else {
        return Err(err(
            StatusCode::NOT_FOUND,
            &format!(
                "nothing in this library's config offers {source_type:?} sources: no package, and \
                 no `[[programs]]` entry that lists `describe`"
            ),
        ));
    };
    match source.probe.as_deref() {
        Some(probe) => Ok(format!("{}{}", offer.prefix, probe.trim_start())),
        None => Err(err(
            StatusCode::NOT_FOUND,
            &format!(
                "{} offers {source_type:?} sources and has no probe for them",
                offer.by
            ),
        )),
    }
}

/// Wait for the step, keeping the slot's progress current from its
/// stderr, and record how it ended.
async fn run(mut child: tokio::process::Child, slot: &Slot, source_type: &str) {
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let read_stdout = async {
        let mut out = String::new();
        let _ = BufReader::new(stdout).read_to_string(&mut out).await;
        out
    };
    let read_stderr = async {
        let mut rest = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match ProbeProgress::parse_line(&line) {
                Some(p) => slot.lock().expect("probe slot mutex").progress = Some(p),
                None => {
                    rest.push_str(&line);
                    rest.push('\n');
                }
            }
        }
        rest
    };
    let finished = tokio::time::timeout(PROBE_TIMEOUT, async {
        tokio::join!(read_stdout, read_stderr, child.wait())
    })
    .await;
    let (stdout, stderr, exit) = match finished {
        Ok(done) => done,
        Err(_) => {
            let _ = child.kill().await;
            return fail(
                slot,
                Failure::from_text(
                    "the probe did not answer within two minutes".to_string(),
                    latchkey_gateway().is_some(),
                ),
            );
        }
    };
    let succeeded = matches!(exit, Ok(status) if status.success());
    if !succeeded {
        // The step's error chain is the useful message ("Gmail
        // users.getProfile: HTTP 401 …"), so pass it through rather than
        // replacing it with our own.
        let failure = failure_from(&stdout, &stderr, latchkey_gateway().is_some());
        tracing::error!(
            source_type = %source_type,
            issue = failure.issue.as_str(),
            "probe failed: {}",
            failure.detail
        );
        return fail(slot, failure);
    }
    match serde_json::from_str::<Value>(&stdout) {
        Ok(report) => {
            let mut slot = slot.lock().expect("probe slot mutex");
            slot.status = ProbeState::Ok;
            slot.report = Some(report);
        }
        Err(e) => fail(
            slot,
            Failure {
                issue: datalib_probe::issue::IssueKind::Unknown,
                detail: format!("the probe printed something that isn't JSON: {e}"),
            },
        ),
    }
}

fn fail(slot: &Slot, failure: Failure) {
    let mut slot = slot.lock().expect("probe slot mutex");
    slot.status = ProbeState::Failed;
    slot.failure = Some(failure);
}

/// What a failed probe said went wrong: the `{"failure": …}` the step
/// prints, or — from a step that crashed before it could — its error
/// lines read the same way. Never the raw stderr tail: that is where
/// the step's log records go, and one would become the headline.
fn failure_from(stdout: &str, stderr: &str, gateway: bool) -> Failure {
    let printed = stdout.lines().rev().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()?
            .get("failure")
            .and_then(|f| serde_json::from_value::<Failure>(f.clone()).ok())
    });
    printed.unwrap_or_else(|| {
        let chain: Vec<&str> = stderr
            .lines()
            .filter_map(|l| l.strip_prefix("error: "))
            .collect();
        let detail = if chain.is_empty() {
            // No chain at all: what the step printed that is not a log
            // record, which for a crash is the panic.
            let rest: Vec<&str> = stderr.lines().filter(|l| !l.starts_with('{')).collect();
            tail(&rest.join("\n"))
        } else {
            chain.join("\n")
        };
        Failure::from_text(detail, gateway)
    })
}

pub async fn probe_status(
    Path(id): Path<String>,
) -> Result<Json<ProbeStatus>, (StatusCode, Json<Value>)> {
    let slot = jobs().lock().expect("probe jobs mutex").get(&id).cloned();
    let Some(slot) = slot else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "no such probe — it may have already been read, or the server restarted",
        ));
    };
    let status = slot.lock().expect("probe slot mutex").clone();
    // Reap a finished probe on read: the client got the answer, and
    // nothing else will ask for it.
    if status.status != ProbeState::Running {
        jobs().lock().expect("probe jobs mutex").remove(&id);
    }
    Ok(Json(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_probe::issue::IssueKind;

    #[test]
    fn the_steps_own_failure_is_taken_as_printed() {
        let stdout = r#"{"failure":{"issue":"rejected","detail":"auth.test: ok=false"}}"#;
        let f = failure_from(stdout, "error: auth.test: ok=false\n", false);
        assert_eq!(f.issue, IssueKind::Rejected);
        assert_eq!(f.detail, "auth.test: ok=false");
    }

    /// A step that died before printing its failure: its error lines
    /// are read, and the log records around them are not the detail.
    #[test]
    fn without_one_the_error_lines_are_read_and_the_log_is_left_out() {
        let stderr = concat!(
            r#"{"level":"WARN","fields":{"message":"HTTP 429 from slack"}}"#,
            "\nerror: auth.test: HTTP 429\n"
        );
        let f = failure_from("", stderr, false);
        assert_eq!(f.issue, IssueKind::RateLimited);
        assert_eq!(f.detail, "auth.test: HTTP 429");
    }
}
