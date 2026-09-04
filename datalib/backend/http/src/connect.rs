//! Endpoints the Add-a-source wizard needs before a source exists:
//! which latchkey accounts are stored, starting latchkey's browser
//! login, and asking a provider what an account can actually reach.
//!
//! All three shell out. That is deliberate and worth stating, because
//! the alternative looks tempting from here:
//!
//! * **latchkey is a CLI, not a library.** `latchkey services info
//!   <name>` already prints JSON with the stored accounts and their
//!   validity, and `latchkey auth browser <name>` already knows how to
//!   drive an OAuth flow in a real browser. Reimplementing either
//!   against the credential store would be a second thing to keep in
//!   step with the pin in `datalib_core::node_runtime`.
//! * **The probe belongs to the provider.** `datalib-http` links no
//!   provider crate and should not start: knowing that a Gmail label
//!   named `INBOX` is spelled `Inbox` in a filter is exactly the
//!   knowledge the email provider exists to hold. So the probe is
//!   `datalib-step probe <type>`, resolved the same way the sync
//!   worker resolves `datalib-dag`.
//!
//! ### Why the browser login is polled rather than awaited
//!
//! `latchkey auth browser` opens a window and waits for a person. That
//! is tens of seconds at best and unbounded at worst, so the request
//! that starts it returns an id immediately and the UI polls. The
//! alternative — holding the HTTP request open — gives the browser
//! nothing to show and no way to give up.
//!
//! The attempts live in a process-global map rather than on
//! [`crate::AppState`]. They are ephemeral UI state belonging to no
//! data root: one `datalib-http` serves one root, an attempt is
//! meaningless once the process exits, and nothing else in the server
//! reads them. Putting them in the shared state would have added a
//! field to every construction of it, including nine tests, to hold
//! something none of them care about.

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

// ---------------------------------------------------------------------
// GET /api/latchkey/{service}
// ---------------------------------------------------------------------

/// The accounts latchkey holds for one service, and how one could be
/// added.
#[derive(Debug, Serialize)]
pub struct ServiceInfo {
    pub service: String,
    /// `browser`, `set`, … — straight from latchkey. The wizard offers
    /// its "Connect" button only when `browser` is among them.
    pub auth_options: Vec<String>,
    pub accounts: Vec<StoredAccount>,
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
        Err(e) => Ok(Json(ServiceInfo {
            service,
            auth_options: Vec::new(),
            accounts: Vec::new(),
            error: Some(e.to_string()),
        })),
    }
}

/// Reshape `latchkey services info` into what the wizard reads.
///
/// Split out from the handler so the shape is testable without a
/// latchkey on the host — this is a wire format two programs agree on,
/// and a silent change to it would show up as an empty account list
/// rather than as an error.
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
        error: None,
    }
}

// ---------------------------------------------------------------------
// POST /api/latchkey/{service}/connect  +  GET /api/latchkey/connect/{id}
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ConnectRequest {
    /// Which identity to store the credential under. Omitted (or
    /// empty) stores latchkey's unnamed default for the service.
    #[serde(default)]
    pub account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectStatus {
    pub id: String,
    /// `running`, `ok`, or `failed`.
    pub status: &'static str,
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
    let account = body.and_then(|Json(b)| b.account).unwrap_or_default();
    let account = account.trim().to_string();

    let id = uuid::Uuid::new_v4().to_string();
    let slot = Arc::new(Mutex::new(ConnectStatus {
        id: id.clone(),
        status: "running",
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
        args.push(account);
    }
    args.extend(["auth".to_string(), "browser".to_string(), service]);

    tokio::spawn(async move {
        let outcome = tokio::time::timeout(CONNECT_TIMEOUT, latchkey_output(&args)).await;
        let mut slot = slot.lock().expect("connect slot mutex");
        match outcome {
            Ok(Ok(output)) => {
                slot.status = "ok";
                slot.output = tail(&output);
            }
            Ok(Err(e)) => {
                slot.status = "failed";
                slot.output = tail(&e.to_string());
            }
            Err(_) => {
                slot.status = "failed";
                slot.output = "the browser login did not finish within 15 minutes; start it again"
                    .to_string();
            }
        }
    });

    Ok(Json(ConnectStatus {
        id,
        status: "running",
        output: String::new(),
    }))
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
            if status.status != "running" {
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

// ---------------------------------------------------------------------
// POST /api/probe
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    /// The `datalib-step download|render <type>` word.
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

// ---------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------

/// Run latchkey and return stdout, or an error carrying its stderr.
async fn latchkey_output(args: &[String]) -> anyhow::Result<String> {
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

/// Last 4 KiB of a command's output. Enough to carry a stack or a
/// couple of error lines; short enough that a runaway log can't be
/// pushed into a browser.
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

/// A latchkey service name, checked before it becomes an argv element.
///
/// Nothing here reaches a shell, so this is not about quoting: it is
/// that a value beginning with `-` would be read by latchkey as an
/// option rather than a service, and a path separator would let a
/// request name something that is not a service at all.
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

/// A `datalib-step` source type, checked for the same reason.
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
mod tests {
    use super::*;
    use serde_json::json;

    /// The exact shape `latchkey services info <name>` prints, as
    /// captured from latchkey 3.8.0. If latchkey changes it, this test
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
        assert_eq!(validated_type("slack_api").unwrap(), "slack_api");
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
