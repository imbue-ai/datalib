//! Endpoints the Add-a-source wizard needs before a source exists:
//! which latchkey accounts are stored, starting latchkey's browser
//! login, and asking a provider what an account can actually reach.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::AppState;

/// How long to wait on `latchkey services info` before giving up. It
/// makes a validation request per stored credential, so it is a network
/// call, not a keyring read.
const SERVICES_TIMEOUT: Duration = Duration::from_secs(45);
/// How long a probe may take. Two HTTP calls against a mail API, plus
/// however long latchkey needs to refresh an expired OAuth token.
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a browser login may stay pending before we call it lost.
/// Long, because the clock is a person reading a consent screen.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

// GET /api/latchkey/{service}

/// The accounts latchkey holds for one service, and how one could be
/// added.
#[derive(Debug, Serialize)]
pub struct ServiceInfo {
    pub service: String,
    /// `browser`, `set`, … — straight from latchkey. The wizard offers
    /// its "Connect" button only when `browser` is among them.
    pub auth_options: Vec<String>,
    pub accounts: Vec<StoredAccount>,
    /// Whether latchkey knows this service at all. False means the name
    /// is free, which is the only state in which anything here may
    /// register it: latchkey refuses to re-register an existing name,
    /// and a service somebody already set up by hand is theirs.
    pub registered: bool,
    /// How to invoke latchkey on *this* machine — the bundled binary's
    /// path, or the `npx` fallback. The wizard prints commands people
    /// are meant to run, and `latchkey` alone is not on everyone's PATH.
    pub cli: String,
    /// Set when latchkey could not answer at all (not installed, no
    /// keyring access). The wizard still lets you type an account name
    /// by hand, so this is a note rather than an error.
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StoredAccount {
    /// latchkey's account key. **Empty string is a real value**: it is
    /// how latchkey spells "the one unnamed account for this service",
    /// and it is addressed by omitting `--account` rather than by
    /// passing `""`. The wizard shows it as "(default)" and writes no
    /// `latchkey_settings.account`.
    pub account: String,
    pub credential_type: Option<String>,
    /// `valid`, `invalid`, `missing`, `unknown`.
    pub credential_status: Option<String>,
}

pub async fn get_service(
    State(_s): State<AppState>,
    Path(service): Path<String>,
) -> Result<Json<ServiceInfo>, (StatusCode, Json<Value>)> {
    let service = validated_service(&service)?;
    match latchkey_json(&["services", "info", &service], SERVICES_TIMEOUT).await {
        Ok(v) => Ok(Json(parse_service_info(&service, &v))),
        Err(e) => {
            // latchkey says "Unknown service: <name>" for a name nobody
            // has registered. That is a state the wizard can act on, so
            // it is reported as `registered: false` rather than folded
            // into the "latchkey could not be asked" note.
            let message = e.to_string();
            let unknown = message.contains("Unknown service");
            Ok(Json(ServiceInfo {
                service,
                auth_options: Vec::new(),
                accounts: Vec::new(),
                registered: !unknown,
                cli: datalib_core::node_runtime::latchkey_cli_hint(),
                error: if unknown { None } else { Some(message) },
            }))
        }
    }
}

fn parse_service_info(service: &str, v: &Value) -> ServiceInfo {
    let auth_options = v
        .get("authOptions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut accounts: Vec<StoredAccount> = v
        .get("credentials")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(account, detail)| StoredAccount {
                    account: account.clone(),
                    credential_type: detail
                        .get("credentialType")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    credential_status: detail
                        .get("credentialStatus")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    // A map has no order; the picker should not shuffle between loads.
    accounts.sort_by(|a, b| a.account.cmp(&b.account));
    ServiceInfo {
        service: service.to_string(),
        auth_options,
        accounts,
        registered: true,
        cli: datalib_core::node_runtime::latchkey_cli_hint(),
        error: None,
    }
}

// POST /api/latchkey/{service}/connect  +  GET /api/latchkey/connect/{id}

#[derive(Debug, Deserialize, Default)]
pub struct ConnectRequest {
    /// Which identity to store the credential under. Omitted (or
    /// empty) stores latchkey's unnamed default for the service.
    #[serde(default)]
    pub account: Option<String>,
    /// How to teach latchkey this service, when it has never heard of
    /// it. A browser login is a property of the *service*, fixed when
    /// it is registered, so a service with none cannot grow one per
    /// account.
    #[serde(default)]
    pub register: Option<ServiceRegistration>,
    /// Run the login without latchkey's saved browser session. Set for
    /// a cookie capture, which cannot see a cookie an already
    /// signed-in session does not re-send — see
    /// [`EPHEMERAL_BROWSER_ENV`].
    #[serde(default)]
    pub ephemeral_browser: bool,
}

/// A `latchkey services register` invocation, as data. The wizard
/// sends it; nothing here composes one.
#[derive(Debug, Deserialize)]
pub struct ServiceRegistration {
    pub base_api_url: String,
    pub login_url: String,
    /// `cookie-capture` or `token-capture` — latchkey's generic
    /// browser logins, for a service it has no built-in support for.
    pub login_flow: String,
    /// That flow's parameters, e.g. `{"cookieKeys": ["sessionKey"]}`.
    pub login_flow_params: Value,
}

/// How one browser-login attempt is going. The UI switches on these
/// words, and the poll endpoint reaps an attempt as soon as it is no
/// longer [`ConnectState::Running`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectState {
    /// `latchkey auth browser` is still running.
    Running,
    /// It exited zero: the credential is stored.
    Ok,
    /// It failed, or ran past `CONNECT_TIMEOUT`.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectStatus {
    pub id: String,
    pub status: ConnectState,
    /// Which account latchkey filed the credential under, when it says.
    /// Not the one that was asked for: `auth browser` ignores
    /// `--account` when storing and uses the identity the login yields
    /// (imbue-ai/latchkey#148) — the signed-in address for an OAuth
    /// service, and the unnamed default for a flow with no identity in
    /// it. Its own report is the only reliable way to know which.
    pub account: Option<String>,
    /// The command's combined output, so a failure is diagnosable
    /// without going to a terminal. Trimmed to the tail — latchkey can
    /// be chatty and the useful part is always at the end.
    pub output: String,
}

/// See the module docs for why this is a global rather than a field on
/// [`crate::AppState`].
fn attempts() -> &'static Mutex<HashMap<String, Arc<Mutex<ConnectStatus>>>> {
    static ATTEMPTS: OnceLock<Mutex<HashMap<String, Arc<Mutex<ConnectStatus>>>>> = OnceLock::new();
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn start_connect(
    State(_s): State<AppState>,
    Path(service): Path<String>,
    body: Option<Json<ConnectRequest>>,
) -> Result<Json<ConnectStatus>, (StatusCode, Json<Value>)> {
    let service = validated_service(&service)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let account = body.account.unwrap_or_default().trim().to_string();
    let register = body.register.map(|r| register_args(&service, &r));
    let login_env: Vec<(&str, &str)> = if body.ephemeral_browser {
        vec![(EPHEMERAL_BROWSER_ENV, "1")]
    } else {
        Vec::new()
    };

    let id = uuid::Uuid::new_v4().to_string();
    let slot = Arc::new(Mutex::new(ConnectStatus {
        id: id.clone(),
        status: ConnectState::Running,
        account: None,
        output: String::new(),
    }));
    attempts()
        .lock()
        .expect("connect attempts mutex")
        .insert(id.clone(), slot.clone());

    // `--account` is a latchkey *global* option and must precede the
    // subcommand — the same rule `datalib_etl::latchkey` writes down
    // for `curl`. Built here rather than reused from there because
    // this crate deliberately links no ETL code.
    let mut args: Vec<String> = Vec::new();
    if !account.is_empty() {
        args.push("--account".into());
        args.push(account.clone());
    }
    args.extend(["auth".to_string(), "browser".to_string(), service.clone()]);

    tokio::spawn(async move {
        // Registering is what makes the browser login exist at all, so
        // it has to happen first. latchkey refuses a name it already
        // holds; that refusal is the desired outcome, not a failure —
        // it is what keeps a hand-made registration untouched.
        if let Some(args) = register {
            if let Err(e) = latchkey_output(&args).await {
                let message = e.to_string();
                if !message.contains("already exists") {
                    let mut slot = slot.lock().expect("connect slot mutex");
                    slot.status = ConnectState::Failed;
                    slot.output = tail(&message);
                    return;
                }
            }
        }
        // latchkey's `auth browser` *refreshes* an account; it will not
        // create one, and refuses a name it has never seen. Seeding it
        // is the whole remedy — the login overwrites the placeholder —
        // so do that rather than handing the person a command. Only on
        // that exact refusal: any other failure is its own problem.
        let mut seeded = false;
        if !account.is_empty() {
            if let Err(e) = latchkey_output(&args).await {
                if e.to_string().contains("No credentials stored for account") {
                    if let Err(e) = latchkey_output(&seed_args(&service, &account)).await {
                        let mut slot = slot.lock().expect("connect slot mutex");
                        slot.status = ConnectState::Failed;
                        slot.output = tail(&e.to_string());
                        return;
                    }
                    seeded = true;
                }
            } else {
                // It succeeded on the first pass; nothing left to do.
                let mut slot = slot.lock().expect("connect slot mutex");
                slot.status = ConnectState::Ok;
                return;
            }
        }

        // Before the login, not lazily after it fails: the refusal names
        // a command, and the person reading it pressed a button
        // precisely so they would not have to run one.
        if let Err(e) = latchkey_output(&ensure_browser_args()).await {
            let mut slot = slot.lock().expect("connect slot mutex");
            slot.status = ConnectState::Failed;
            slot.output = format!(
                "no browser available for the login ({}). Latchkey can install one, which \
                 downloads a Chromium of a few hundred megabytes: run `{} ensure-browser` and \
                 try again.",
                tail(&e.to_string()),
                datalib_core::node_runtime::latchkey_cli_hint(),
            );
            return;
        }

        let outcome =
            tokio::time::timeout(CONNECT_TIMEOUT, latchkey_output_env(&args, &login_env)).await;
        // A placeholder outliving a login that never finished is a
        // stored credential that cannot work, and it would make the
        // account look connected in every account list.
        if seeded && !matches!(outcome, Ok(Ok(_))) {
            let _ = latchkey_output(&clear_args(&service, &account)).await;
        }
        let mut slot = slot.lock().expect("connect slot mutex");
        match outcome {
            Ok(Ok(output)) => {
                slot.status = ConnectState::Ok;
                slot.account = stored_account(&output);
                slot.output = tail(&output);
            }
            Ok(Err(e)) => {
                slot.status = ConnectState::Failed;
                slot.output = tail(&e.to_string());
            }
            Err(_) => {
                slot.status = ConnectState::Failed;
                slot.output = "the browser login did not finish within 15 minutes; start it again"
                    .to_string();
            }
        }
    });

    Ok(Json(ConnectStatus {
        id,
        status: ConnectState::Running,
        account: None,
        output: String::new(),
    }))
}

/// The placeholder that brings a named account into existence so the
/// browser login has something to refresh. Never used as a credential:
/// the login overwrites it, and a login that does not finish has it
/// cleared again.
fn seed_args(service: &str, account: &str) -> Vec<String> {
    vec![
        "--account".to_string(),
        account.to_string(),
        "auth".to_string(),
        "set".to_string(),
        service.to_string(),
        "-H".to_string(),
        "X-Datalib-Placeholder: pending-browser-login".to_string(),
    ]
}

fn clear_args(service: &str, account: &str) -> Vec<String> {
    vec![
        "--account".to_string(),
        account.to_string(),
        "auth".to_string(),
        "clear".to_string(),
        service.to_string(),
    ]
}

/// `ensure-browser`, restricted to the sources that use a browser
/// already on the machine.
///
/// A latchkey store that has never done a browser login has none
/// configured, and `auth browser` refuses outright ("No browser
/// configured. Run 'latchkey ensure-browser' first.") — which is every
/// new user, and was invisible to us because a developer's store is
/// never new.
///
/// The default source list ends in `download-playwright-browser`, so
/// running it unrestricted can pull a Chromium of a hundred-odd
/// megabytes. Nobody pressing "Latchkey auth" asked for that, and a
/// long silent stall behind a spinner is the worst way to deliver it.
/// These three sources configure an existing browser or fail fast; the
/// download stays a thing someone chooses, by running the command
/// themselves.
fn ensure_browser_args() -> Vec<String> {
    vec![
        "ensure-browser".to_string(),
        "--source".to_string(),
        "existing-config,system-browser,existing-playwright-browser".to_string(),
    ]
}

/// The account named in latchkey's "Stored credentials for account
/// 'x'." line, which `auth browser` prints when the login yielded an
/// identity. Absent for a cookie capture, which has none and files
/// under the unnamed default.
fn stored_account(output: &str) -> Option<String> {
    let (_, rest) = output.split_once("Stored credentials for account '")?;
    let (name, _) = rest.split_once('\'')?;
    Some(name.to_string())
}

/// `latchkey services register <name> --base-api-url=… --login-url=…
/// --login-flow=… --login-flow-params=…`, as an argv.
fn register_args(service: &str, r: &ServiceRegistration) -> Vec<String> {
    vec![
        "services".to_string(),
        "register".to_string(),
        service.to_string(),
        format!("--base-api-url={}", r.base_api_url),
        format!("--login-url={}", r.login_url),
        format!("--login-flow={}", r.login_flow),
        format!("--login-flow-params={}", r.login_flow_params),
    ]
}

pub async fn connect_status(
    State(_s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ConnectStatus>, (StatusCode, Json<Value>)> {
    let slot = attempts()
        .lock()
        .expect("connect attempts mutex")
        .get(&id)
        .cloned();
    match slot {
        Some(slot) => {
            let status = slot.lock().expect("connect slot mutex").clone();
            // Reap a finished attempt on read: the client got the
            // answer, and nothing else will ask for it. Without this
            // the map grows for the life of the process.
            if status.status != ConnectState::Running {
                attempts()
                    .lock()
                    .expect("connect attempts mutex")
                    .remove(&id);
            }
            Ok(Json(status))
        }
        None => Err(err(
            StatusCode::NOT_FOUND,
            "no such connection attempt — it may have already been read, or the server restarted",
        )),
    }
}

// POST /api/probe

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
}

pub async fn probe(
    State(_s): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let source_type = validated_type(&req.source_type)?;
    let step_bin = crate::worker::resolve_step_bin().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no `datalib-step` binary found (set $DATALIB_STEP_BIN or $DATALIB_BINARY_DIR). \
             Testing a connection runs the provider's own probe, so it needs the step binary \
             the pipeline uses.",
        )
    })?;
    let params = serde_json::to_string(&req.params).unwrap_or_else(|_| "{}".to_string());

    let mut cmd = Command::new(step_bin);
    cmd.arg("probe")
        .arg(&source_type)
        .arg("--params")
        .arg(params)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e}"))),
        Err(_) => {
            return Err(err(
                StatusCode::GATEWAY_TIMEOUT,
                "the probe did not answer within two minutes",
            ))
        }
    };
    if !out.status.success() {
        // The step prints its error chain to stderr; that chain is the
        // useful message ("Gmail users.getProfile: HTTP 401 …"), so
        // pass it through rather than replacing it with our own.
        return Err(err(
            StatusCode::BAD_GATEWAY,
            &tail(&String::from_utf8_lossy(&out.stderr)),
        ));
    }
    let report: Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            &format!("the probe printed something that isn't JSON: {e}"),
        )
    })?;
    Ok(Json(report))
}

// shared

/// A browser login that must observe a *fresh* sign-in, so latchkey
/// must not restore the session it saved last time.
///
/// Cookie capture reads the `Set-Cookie` headers that arrive while
/// someone signs in. latchkey otherwise seeds the browser with its own
/// persisted state, which lands you already signed in — and a site that
/// sees an established session issues no new cookie, so the capture
/// waits for something that can never arrive and the login hangs with
/// nothing on screen to say why (imbue-ai/latchkey#150). Ephemeral mode
/// neither loads nor saves that state.
///
/// Only for cookie capture. An OAuth login *benefits* from the saved
/// session — it has an identity to re-derive either way, and being
/// already signed in is one less password.
const EPHEMERAL_BROWSER_ENV: &str = "LATCHKEY_EPHEMERAL_BROWSER";

async fn latchkey_output(args: &[String]) -> anyhow::Result<String> {
    latchkey_output_env(args, &[]).await
}

async fn latchkey_output_env(args: &[String], env: &[(&str, &str)]) -> anyhow::Result<String> {
    // The same resolution `datalib_etl::latchkey` uses (bundled Node
    // runtime, else `npx -y latchkey@<pin>`), reached through
    // `datalib_core` so the pin is not spelled twice.
    let mut cmd: Command = datalib_core::node_runtime::bundled_command(
        "latchkey",
        datalib_core::node_runtime::LATCHKEY_VERSION,
        LATCHKEY_ENTRY_REL,
    )
    .unwrap_or_else(|| {
        datalib_core::node_runtime::npx_command(&format!(
            "latchkey@{}",
            datalib_core::node_runtime::LATCHKEY_VERSION
        ))
    })
    .into();
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().await.map_err(|e| {
        anyhow::anyhow!(
            "could not run latchkey ({e}). Install it, or check that {} works.",
            datalib_core::node_runtime::latchkey_cli_hint()
        )
    })?;
    if !out.status.success() {
        anyhow::bail!("{}", tail(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Entry script of the `latchkey` npm package inside a staged runtime
/// tree. Mirrors `datalib_etl::latchkey::LATCHKEY_ENTRY_REL`, which
/// this crate cannot import (it links no ETL code).
const LATCHKEY_ENTRY_REL: &str = "node_modules/latchkey/dist/src/cli.js";

async fn latchkey_json(args: &[&str], timeout: Duration) -> anyhow::Result<Value> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let text = tokio::time::timeout(timeout, latchkey_output(&owned))
        .await
        .map_err(|_| anyhow::anyhow!("latchkey did not answer within {}s", timeout.as_secs()))??;
    serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("latchkey printed something that isn't JSON: {e}"))
}

fn tail(s: &str) -> String {
    let s = s.trim();
    const MAX: usize = 4096;
    if s.len() <= MAX {
        return s.to_string();
    }
    let cut = s.len() - MAX;
    // Land on a char boundary — the tail of a UTF-8 log is not
    // guaranteed to start on one.
    let cut = (cut..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    format!("…{}", &s[cut..])
}

fn validated_service(service: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let s = service.trim();
    if s.is_empty()
        || s.starts_with('-')
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "a latchkey service name is letters, digits, '-', '_' and '.'",
        ));
    }
    Ok(s.to_string())
}

fn validated_type(source_type: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let s = source_type.trim();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "a source type is lowercase letters and underscores",
        ));
    }
    Ok(s.to_string())
}

fn err(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({ "error": message })))
}

#[cfg(test)]
mod ensure_browser_tests {
    use super::ensure_browser_args;

    /// The whole point of naming sources explicitly: the default list
    /// ends in `download-playwright-browser`, and a button press must
    /// not turn into a few hundred megabytes nobody asked for.
    #[test]
    fn never_offers_to_download_a_browser() {
        let args = ensure_browser_args();
        let sources = args.last().expect("a --source value");
        assert!(
            !sources.contains("download"),
            "ensure-browser must not reach the downloading source: {sources}"
        );
        assert!(sources.contains("system-browser"), "{sources}");
    }
}

#[cfg(test)]
mod account_report_tests {
    use super::stored_account;

    /// What `latchkey auth browser fastmail` prints — an OAuth login
    /// derives the address it signed in as, and reports it even when
    /// `--account` asked for something else entirely.
    #[test]
    fn reads_the_account_oauth_reports() {
        assert_eq!(
            stored_account("Done. Stored credentials for account 'thad_imbue@fastmail.com'.\n")
                .as_deref(),
            Some("thad_imbue@fastmail.com"),
        );
    }

    /// A cookie capture has no identity to derive, so it says only
    /// "Done" and the credential lands on latchkey's unnamed default.
    /// `None` has to mean *that*, not "parse failed".
    #[test]
    fn a_capture_that_names_nothing_yields_none() {
        assert_eq!(stored_account("Done\n"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The exact shape `latchkey services info <name>` prints, as
    /// captured from latchkey 3.11.0. If latchkey changes it, this test
    /// is what says so — the handler itself would just start returning
    /// an empty account list.
    #[test]
    fn reads_a_real_services_info_payload() {
        let v = json!({
            "type": "built-in",
            "baseApiUrls": ["https://gmail.googleapis.com/"],
            "authOptions": ["browser", "set"],
            "credentials": {
                "thad@imbue.com": {
                    "credentialType": "oauth",
                    "credentialStatus": "valid"
                }
            }
        });
        let info = parse_service_info("google-gmail", &v);
        assert_eq!(info.service, "google-gmail");
        assert_eq!(info.auth_options, vec!["browser", "set"]);
        assert_eq!(info.accounts.len(), 1);
        assert_eq!(info.accounts[0].account, "thad@imbue.com");
        assert_eq!(info.accounts[0].credential_status.as_deref(), Some("valid"));
        assert!(info.error.is_none());
    }

    /// latchkey spells "the one unnamed account" as an empty key. It
    /// must survive as an account rather than being filtered out —
    /// several services (claude-ai, chatgpt) only ever have that one.
    #[test]
    fn keeps_the_unnamed_default_account() {
        let v = json!({
            "authOptions": ["set"],
            "credentials": { "": { "credentialType": "rawCurl" } }
        });
        let info = parse_service_info("claude-ai", &v);
        assert_eq!(info.accounts.len(), 1);
        assert_eq!(info.accounts[0].account, "");
    }

    /// A service with nothing stored is the new-user case, not an
    /// error: the wizard still needs `authOptions` to know whether to
    /// offer its Connect button.
    #[test]
    fn a_service_with_no_credentials_is_not_an_error() {
        let info = parse_service_info("fastmail", &json!({ "authOptions": ["browser"] }));
        assert!(info.accounts.is_empty());
        assert_eq!(info.auth_options, vec!["browser"]);
        assert!(info.error.is_none());
    }

    /// A map has no order. Without the sort the account dropdown would
    /// reshuffle between loads of the same dialog.
    #[test]
    fn orders_accounts_stably() {
        let v = json!({
            "credentials": { "zoe@x.com": {}, "adam@x.com": {}, "": {} }
        });
        let info = parse_service_info("s", &v);
        let accounts: Vec<&str> = info.accounts.iter().map(|a| a.account.as_str()).collect();
        assert_eq!(accounts, vec!["", "adam@x.com", "zoe@x.com"]);
    }

    #[test]
    fn rejects_a_service_name_that_would_read_as_an_option() {
        assert!(validated_service("--help").is_err());
        assert!(validated_service("a/b").is_err());
        assert!(validated_service("").is_err());
        assert_eq!(validated_service(" google-gmail ").unwrap(), "google-gmail");
    }

    #[test]
    fn rejects_a_source_type_that_is_not_one() {
        assert!(validated_type("--params").is_err());
        assert!(validated_type("Email").is_err());
        assert_eq!(validated_type("email").unwrap(), "email");
        assert_eq!(validated_type("slack").unwrap(), "slack");
    }

    /// The tail is sliced by bytes; a log ending mid-codepoint must not
    /// panic.
    #[test]
    fn tail_lands_on_a_char_boundary() {
        let long = "é".repeat(4000);
        let out = tail(&long);
        assert!(out.len() <= 4200, "{}", out.len());
        assert!(out.starts_with('…'));
    }
}
