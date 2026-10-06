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
use strum::{EnumString, IntoStaticStr, VariantArray};
use tokio::process::Command;

use datalib_probe::issue::{classify, IssueKind};

use crate::{plugins, AppState};

/// How long to wait on `latchkey services info` before giving up. It
/// makes a validation request per stored credential, so it is a network
/// call, not a keyring read.
const SERVICES_TIMEOUT: Duration = Duration::from_secs(45);
/// How long a browser login may stay pending before we call it lost.
/// Long, because the clock is a person reading a consent screen.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Set by whoever runs datalib behind a latchkey gateway. With it set,
/// the `latchkey` this module spawns is a thin client: `services info`,
/// `auth browser` and `curl` are forwarded to the gateway, and every
/// command that manages local state — `ensure-browser`, `services
/// register`, `auth set`, `auth clear` — is refused outright. The same
/// rule `datalib_etl::latchkey` applies, restated here because this
/// crate links no ETL code.
const GATEWAY_ENV_VAR: &str = "LATCHKEY_GATEWAY";

/// The gateway's URL, when this process is pointed at one.
pub(crate) fn latchkey_gateway() -> Option<String> {
    gateway_from(std::env::var_os(GATEWAY_ENV_VAR))
}

/// An empty setting is no gateway, the way latchkey itself reads it.
fn gateway_from(value: Option<std::ffi::OsString>) -> Option<String> {
    let value = value?.to_string_lossy().trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn gateway_refusal(gateway: &str) -> String {
    format!(
        "credentials are held by a latchkey gateway ({gateway}), and a login cannot be \
         started from here. Sign in where that gateway is managed, then test the connection."
    )
}

// GET /api/latchkey/{service}

/// The accounts latchkey holds for one service, and how one could be
/// added.
#[derive(Debug, Serialize)]
pub struct ServiceInfo {
    pub service: String,
    /// `browser`, `set`, … — straight from latchkey. The wizard offers
    /// its "Connect" button only when `browser` is among them.
    pub auth_options: Vec<String>,
    /// latchkey's own `auth set` command line for this service, which
    /// knows the credential's shape (a bearer header, a `user:password`
    /// pair). Shown to a person who has to paste one.
    pub set_example: Option<String>,
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
    /// The latchkey gateway this process is pointed at, if any. When
    /// set, the wizard offers no login of its own: the credentials live
    /// on the gateway, and so does the browser that signs in to them.
    pub gateway: Option<String>,
    /// Set when latchkey could not answer at all (not installed, no
    /// keyring access). The wizard still lets you type an account name
    /// by hand, so this is a note rather than an error.
    pub error: Option<String>,
    /// What kind of trouble `error` is, for the wizard's one sentence.
    pub issue: Option<IssueKind>,
    /// Where signing in will install the latchkey plugin that adds this
    /// service, when latchkey lacks it and datalib ships one
    /// ([`crate::plugins`]). The wizard says so before anything is
    /// written.
    pub installs_plugin: Option<String>,
    /// Who names the account a browser login adds.
    pub account_naming: AccountNaming,
}

/// Who names the account a browser login adds. `docs/dev/latchkey.md`
/// §"Accounts: who names them" has the rules this stands for.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AccountNaming {
    /// The login reports who signed in and stores under that; latchkey
    /// accepts `--account` only for an account it already holds. Every
    /// built-in service and plugin datalib signs in to with a browser.
    Service,
    /// `--account` decides, a new name included: a service registered
    /// with `latchkey services register`, which has no identity to
    /// report.
    Chosen,
}

impl AccountNaming {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
    /// From the `type` `latchkey services info` reports, which stands in
    /// for the rule until latchkey reports it (imbue-ai/latchkey#169). A
    /// plugin reports `built-in`.
    fn of_service_type(service_type: Option<&str>) -> Self {
        match service_type {
            Some("user-registered") => AccountNaming::Chosen,
            _ => AccountNaming::Service,
        }
    }
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
    let gateway = latchkey_gateway();
    match latchkey_json(&["services", "info", &service], SERVICES_TIMEOUT).await {
        Ok(v) => Ok(Json(ServiceInfo {
            gateway,
            ..parse_service_info(&service, &v)
        })),
        Err(e) => {
            // latchkey says "Unknown service: <name>" for a name nobody
            // has registered. That is a state the wizard can act on, so
            // it is reported as `registered: false` rather than folded
            // into the "latchkey could not be asked" note.
            let message = e.to_string();
            let unknown = message.contains("Unknown service");
            if let Some(info) = unknown
                .then(|| plugin_service_info(&service, gateway.as_deref()))
                .flatten()
            {
                return Ok(Json(info));
            }
            Ok(Json(ServiceInfo {
                service,
                auth_options: Vec::new(),
                set_example: None,
                accounts: Vec::new(),
                registered: !unknown,
                cli: datalib_core::node_runtime::latchkey_cli_hint(),
                issue: (!unknown).then(|| classify(&message, gateway.is_some())),
                gateway,
                error: if unknown { None } else { Some(message) },
                installs_plugin: None,
                // Not registered yet: the wizard registers it, and a
                // registered service names nothing. Unreachable latchkey:
                // nothing to go on, and the box stays free to type in.
                account_naming: AccountNaming::Chosen,
            }))
        }
    }
}

/// What a service latchkey lacks will offer once its plugin is in, for
/// a service datalib ships the plugin for. Not under a gateway: the
/// plugin would have to be on the gateway's machine.
fn plugin_service_info(service: &str, gateway: Option<&str>) -> Option<ServiceInfo> {
    let plugin = plugins::for_service(service)?;
    if gateway.is_some() {
        return None;
    }
    let dir = plugins::plugin_dir(&plugins::latchkey_dir()?, plugin);
    Some(ServiceInfo {
        service: service.to_string(),
        auth_options: plugin.auth_options.iter().map(|o| o.to_string()).collect(),
        set_example: Some(plugin.set_example.to_string()),
        accounts: Vec::new(),
        registered: false,
        cli: datalib_core::node_runtime::latchkey_cli_hint(),
        gateway: None,
        error: None,
        issue: None,
        installs_plugin: Some(dir.display().to_string()),
        // Garmin's names its own; `plugins::tests` checks it still does.
        account_naming: AccountNaming::Service,
    })
}

/// Puts the plugin that adds `service` where latchkey loads it, when
/// datalib ships one: before a sign-in or a paste, which are the first
/// things that need latchkey to know the service.
async fn install_plugin_for(service: &str) -> Result<(), String> {
    let Some(plugin) = plugins::for_service(service) else {
        return Ok(());
    };
    let Some(latchkey_dir) = plugins::latchkey_dir() else {
        return Err(format!(
            "neither $LATCHKEY_DIRECTORY nor $HOME is set, so there is nowhere to install \
             latchkey's {service} plugin"
        ));
    };
    let dir = plugins::plugin_dir(&latchkey_dir, plugin);
    let at = dir.clone();
    let found = tokio::task::spawn_blocking(move || plugins::install(plugin, &at))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not install latchkey's {service} plugin: {e}"))?;
    match found {
        plugins::Found::Theirs => {
            tracing::info!(service, dir = %dir.display(), "left a latchkey plugin datalib did not install")
        }
        plugins::Found::Ours(v) if v == plugin.version => {}
        _ => tracing::info!(service, dir = %dir.display(), "installed a latchkey plugin"),
    }
    Ok(())
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
        set_example: v
            .get("setCredentialsExample")
            .and_then(Value::as_str)
            .map(str::to_string),
        accounts,
        registered: true,
        cli: datalib_core::node_runtime::latchkey_cli_hint(),
        gateway: None,
        error: None,
        issue: None,
        installs_plugin: None,
        account_naming: AccountNaming::of_service_type(v.get("type").and_then(Value::as_str)),
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
    /// Which account latchkey filed the credential under, when it says:
    /// the one asked for, or with none asked for, the identity the login
    /// yields.
    pub account: Option<String>,
    /// The command's combined output, so a failure is diagnosable
    /// without going to a terminal. Trimmed to the tail — latchkey can
    /// be chatty and the useful part is always at the end.
    pub output: String,
    /// What the attempt is doing now, while it runs.
    pub phase: ConnectPhase,
    /// What kind of trouble `output` is, once `Failed`.
    pub issue: Option<IssueKind>,
}

/// What a running browser login is doing, so the wizard can say what
/// it is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectPhase {
    /// Registering the service, finding a browser.
    Preparing,
    /// No browser on the machine: fetching one, a one-time download.
    DownloadingBrowser,
    /// The browser is open on the service's login page.
    SigningIn,
}

// POST /api/latchkey/{service}/credential

/// A credential a person pasted into the wizard, in the one of two
/// shapes latchkey's `auth set` takes that the wizard offers.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PastedCredential {
    /// Headers sent as they are: `Authorization: Bearer …`,
    /// `Cookie: sessionKey=…`. Several for a service that needs more
    /// than one.
    Headers { headers: Vec<String> },
    /// HTTP Basic, which is what an app password is (Fastmail's DAV).
    Basic { username: String, password: String },
    /// A folder the service's plugin reads the credential from, stored
    /// with `auth set-nocurl` (Garmin's garth tokens).
    Directory { path: String },
}

#[derive(Debug, Deserialize)]
pub struct SetCredentialRequest {
    /// Which account to file it under. Empty lets latchkey choose,
    /// which replaces the service's one stored credential if it has
    /// exactly one — the wizard warns before sending that.
    #[serde(default)]
    pub account: String,
    pub credential: PastedCredential,
}

/// Store a pasted credential with `latchkey auth set`. latchkey takes
/// the credential only as arguments, so it is on this child's command
/// line for the second it runs — the same exposure as the
/// `auth set … $(pbpaste)` the docs give for a terminal.
pub async fn set_credential(
    State(_s): State<AppState>,
    Path(service): Path<String>,
    Json(body): Json<SetCredentialRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let service = validated_service(&service)?;
    if let Some(gateway) = latchkey_gateway() {
        return Err(err(StatusCode::CONFLICT, &gateway_refusal(&gateway)));
    }
    let args = set_args(&service, body.account.trim(), &body.credential)
        .map_err(|m| err(StatusCode::BAD_REQUEST, &m))?;
    install_plugin_for(&service)
        .await
        .map_err(|m| err(StatusCode::INTERNAL_SERVER_ERROR, &m))?;
    match latchkey_output(&args).await {
        Ok(_) => {
            tracing::info!(service, "latchkey stored a pasted credential");
            Ok(Json(serde_json::json!({ "ok": true })))
        }
        Err(e) => {
            let message = scrub(&e.to_string());
            tracing::error!(service, "latchkey auth set failed: {message}");
            Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": message,
                    "issue": classify(&message, false),
                })),
            ))
        }
    }
}

/// `[--account <a>] auth set <service> <curl args>`, or why the pasted
/// credential cannot be one. Every piece is its own argv element, so
/// nothing is parsed by a shell; the checks are about what curl will
/// later make of it.
fn set_args(
    service: &str,
    account: &str,
    credential: &PastedCredential,
) -> Result<Vec<String>, String> {
    let one_line = |s: &str| !s.contains(['\r', '\n']);
    let mut args: Vec<String> = Vec::new();
    if !account.is_empty() {
        if account.starts_with('-') || !one_line(account) {
            return Err("an account name is one line and does not start with '-'".into());
        }
        args.extend(["--account".to_string(), account.to_string()]);
    }
    let set = match credential {
        PastedCredential::Directory { .. } => "set-nocurl",
        _ => "set",
    };
    args.extend(["auth".to_string(), set.to_string(), service.to_string()]);
    match credential {
        PastedCredential::Headers { headers } => {
            if headers.is_empty() {
                return Err("paste the credential first".into());
            }
            for header in headers {
                let (name, value) = header.split_once(':').ok_or_else(|| {
                    format!("a header is `Name: value`; got {:?}", redact(header))
                })?;
                let name_ok = name.starts_with(|c: char| c.is_ascii_alphabetic())
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                if !name_ok || !one_line(header) || value.trim().is_empty() {
                    return Err(format!(
                        "a header is one line, `Name: value`, with a value; got {:?}",
                        redact(header)
                    ));
                }
                args.extend(["-H".to_string(), header.trim().to_string()]);
            }
        }
        PastedCredential::Basic { username, password } => {
            let username = username.trim();
            if username.is_empty() || password.is_empty() {
                return Err("both the username and the password are needed".into());
            }
            if username.contains(':') || !one_line(username) || !one_line(password) {
                return Err("the username has no ':' and neither has a line break".into());
            }
            args.extend(["-u".to_string(), format!("{username}:{password}")]);
        }
        PastedCredential::Directory { path } => {
            let path = path.trim();
            if path.is_empty() || path.starts_with('-') || !one_line(path) {
                return Err("a folder is one line and does not start with '-'".into());
            }
            args.push(path.to_string());
        }
    }
    Ok(args)
}

/// A header's name, for an error message that must not echo the
/// secret back.
fn redact(header: &str) -> String {
    match header.split_once(':') {
        Some((name, _)) => format!("{name}: …"),
        None => "…".to_string(),
    }
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
    // Before anything is spawned: the first latchkey command below is
    // `ensure-browser`, whose refusal under a gateway names a command
    // that would configure a browser on the wrong machine.
    if let Some(gateway) = latchkey_gateway() {
        return Err(err(StatusCode::CONFLICT, &gateway_refusal(&gateway)));
    }
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
        phase: ConnectPhase::Preparing,
        issue: None,
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
        if let Err(message) = install_plugin_for(&service).await {
            fail(&slot, &service, message);
            return;
        }
        // Registering is what makes the browser login exist at all, so
        // it has to happen first. latchkey refuses a name it already
        // holds; that refusal is the desired outcome, not a failure —
        // it is what keeps a hand-made registration untouched.
        if let Some(args) = register {
            if let Err(e) = latchkey_output(&args).await {
                let message = e.to_string();
                if !message.contains("already exists") {
                    fail(&slot, &service, tail(&message));
                    return;
                }
            }
        }
        // Before the login, not lazily after it fails: the refusal names
        // a command, and the person reading it pressed a button
        // precisely so they would not have to run one.
        if latchkey_output(&ensure_browser_args()).await.is_err() {
            // No browser on the machine. The person pressed "Sign in
            // with browser", so fetching one is what they asked for;
            // the wizard says it is happening, since it takes a while.
            slot.lock().expect("connect slot mutex").phase = ConnectPhase::DownloadingBrowser;
            if let Err(e) = latchkey_output(&download_browser_args()).await {
                let message = format!(
                    "No browser found for the sign-in, and fetching one failed: {}",
                    tail(&e.to_string()),
                );
                fail(&slot, &service, message);
                return;
            }
        }
        slot.lock().expect("connect slot mutex").phase = ConnectPhase::SigningIn;

        let outcome =
            tokio::time::timeout(CONNECT_TIMEOUT, latchkey_output_env(&args, &login_env)).await;

        match outcome {
            Ok(Ok(output)) => succeed(&slot, &service, stored_account(&output), tail(&output)),
            Ok(Err(e)) => fail(&slot, &service, tail(&e.to_string())),
            Err(_) => fail(
                &slot,
                &service,
                "the browser login did not finish within 15 minutes; start it again".to_string(),
            ),
        }
    });

    Ok(Json(ConnectStatus {
        id,
        status: ConnectState::Running,
        account: None,
        output: String::new(),
        phase: ConnectPhase::Preparing,
        issue: None,
    }))
}

/// The attempt's outcome goes to this process's stderr as well as to
/// the slot: the slot is reaped on the first poll that reads it, and a
/// login that "worked" but stored a dead credential (latchkey#152) is
/// only diagnosable afterwards if something durable says what latchkey
/// printed. The output is latchkey's, so it is scrubbed first.
fn fail(slot: &Arc<Mutex<ConnectStatus>>, service: &str, output: String) {
    tracing::error!(service, "latchkey login failed: {}", scrub(&output));
    let mut slot = slot.lock().expect("connect slot mutex");
    slot.status = ConnectState::Failed;
    slot.issue = Some(classify(&output, false));
    slot.output = output;
}

fn succeed(
    slot: &Arc<Mutex<ConnectStatus>>,
    service: &str,
    account: Option<String>,
    output: String,
) {
    match &account {
        Some(a) => tracing::info!(service, account = %a, "latchkey login stored a credential"),
        None => tracing::info!(service, "latchkey login stored a credential"),
    }
    let mut slot = slot.lock().expect("connect slot mutex");
    slot.status = ConnectState::Ok;
    slot.account = account;
    slot.output = output;
}

/// `ensure-browser`, restricted to the sources that use a browser
/// already on the machine. When it finds none, [`download_browser_args`]
/// fetches one.
///
/// A latchkey store that has never done a browser login has none
/// configured, and `auth browser` refuses outright ("No browser
/// configured. Run 'latchkey ensure-browser' first.") — which is every
/// new user, and was invisible to us because a developer's store is
/// never new.
///
/// The default source list ends in `download-playwright-browser`, which
/// pulls a Chromium of a hundred-odd megabytes. A browser already on
/// the machine is always preferred, so the download is its own step,
/// taken only when these find nothing and said on screen while it runs
/// rather than a silent stall behind a spinner.
/// `datalib/tauri/check-app.sh` runs the same sources against every
/// built .app.
fn ensure_browser_args() -> Vec<String> {
    vec![
        "ensure-browser".to_string(),
        "--source".to_string(),
        "existing-config,system-browser,existing-playwright-browser".to_string(),
    ]
}

/// `ensure-browser` from the one source that downloads.
fn download_browser_args() -> Vec<String> {
    vec![
        "ensure-browser".to_string(),
        "--source".to_string(),
        "download-playwright-browser".to_string(),
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
/// already signed in is one less password. Whether token capture needs
/// it is open (imbue-ai/latchkey#152); until it is shown to, a sign-in
/// every time is a cost nobody asked for.
const EPHEMERAL_BROWSER_ENV: &str = "LATCHKEY_EPHEMERAL_BROWSER";

async fn latchkey_output(args: &[String]) -> anyhow::Result<String> {
    latchkey_output_env(args, &[]).await
}

async fn latchkey_output_env(args: &[String], env: &[(&str, &str)]) -> anyhow::Result<String> {
    // The same resolution `datalib_etl::latchkey` uses, reached through
    // `datalib_core` so the pin is not spelled twice.
    let mut cmd: Command = datalib_core::node_runtime::latchkey_command()?.into();
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

async fn latchkey_json(args: &[&str], timeout: Duration) -> anyhow::Result<Value> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let text = tokio::time::timeout(timeout, latchkey_output(&owned))
        .await
        .map_err(|_| anyhow::anyhow!("latchkey did not answer within {}s", timeout.as_secs()))??;
    serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("latchkey printed something that isn't JSON: {e}"))
}

pub(crate) fn tail(s: &str) -> String {
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

/// The `error: …` lines `datalib-step probe` prints on failure, without
/// the tracing output around them. Falls back to the whole tail when a
/// crash left no chain.
pub(crate) fn error_chain(stderr: &str) -> String {
    let chain: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("error: "))
        .collect();
    if chain.is_empty() {
        tail(stderr)
    } else {
        chain.join(" | ")
    }
}

/// Blank anything credential-shaped before a line goes to the log.
/// Nothing logged here is *meant* to carry one — `latchkey curl` runs
/// without `-v`, and a probe's error is a status line and a body
/// preview — but latchkey's own output on a failed login is whatever it
/// chose to print, and a log file outlives the dialog.
///
/// Word-based, so it is deliberately blunt: the word after `Bearer`,
/// the rest of a line after `Cookie:` / `Set-Cookie:`, any JWT, and the
/// value of any `k=v` whose key mentions a token, secret, password,
/// session or cookie.
pub(crate) fn scrub(s: &str) -> String {
    const BLANK: &str = "<redacted>";
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut blank_next = false;
        let mut words = line.split(' ').peekable();
        let mut first = true;
        while let Some(word) = words.next() {
            if !first {
                out.push(' ');
            }
            first = false;
            let lower = word.to_ascii_lowercase();
            if blank_next && !word.is_empty() {
                out.push_str(BLANK);
                blank_next = false;
                continue;
            }
            if lower == "cookie:" || lower == "set-cookie:" {
                out.push_str(word);
                if words.peek().is_some() {
                    out.push(' ');
                    out.push_str(BLANK);
                }
                break;
            }
            if lower == "bearer" {
                out.push_str(word);
                blank_next = true;
                continue;
            }
            if looks_like_jwt(word) {
                out.push_str(BLANK);
                continue;
            }
            if let Some((key, _)) = word.split_once('=') {
                let k = key.to_ascii_lowercase();
                if ["token", "secret", "password", "session", "cookie"]
                    .iter()
                    .any(|needle| k.contains(needle))
                {
                    out.push_str(key);
                    out.push('=');
                    out.push_str(BLANK);
                    continue;
                }
            }
            out.push_str(word);
        }
    }
    out
}

/// Three base64url segments, the first a `{"` header (`eyJ`), which is
/// what every JWT starts with.
fn looks_like_jwt(word: &str) -> bool {
    let trimmed = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
    let mut parts = trimmed.split('.');
    let (Some(h), Some(p), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let b64url = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    h.starts_with("eyJ") && b64url(h) && b64url(p) && b64url(sig)
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

pub(crate) fn validated_type(source_type: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let s = source_type.trim();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "a source type is lowercase letters and underscores",
        ));
    }
    Ok(s.to_string())
}

pub(crate) fn err(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({ "error": message })))
}

#[cfg(test)]
mod scrub_tests {
    use super::{error_chain, scrub};

    /// The message this exists for — the chatgpt 401 — must survive
    /// whole: it names the endpoint, the status and the body's code.
    #[test]
    fn keeps_the_chatgpt_401_intact() {
        let msg = "error: chatgpt.com credentials are not set up: GET /backend-api/me -> \
                   HTTP 401 cf-mitigated=None body=\"{\\n \"code\": \"token_expired\"}\"";
        assert_eq!(scrub(msg), msg);
    }

    #[test]
    fn blanks_bearer_tokens_cookies_and_jwts() {
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjE3NTc3NjAwMDB9.c2lnbmF0dXJl";
        assert_eq!(
            scrub(&format!("-H Authorization: Bearer {jwt} sent")),
            "-H Authorization: Bearer <redacted> sent"
        );
        assert_eq!(
            scrub(&format!("stored {jwt} for chatgpt")),
            "stored <redacted> for chatgpt"
        );
        assert_eq!(
            scrub("Cookie: sessionKey=sk-abc; other=1"),
            "Cookie: <redacted>"
        );
        assert_eq!(
            scrub("GET https://x/y?access_token=abc&limit=5 ok"),
            "GET https://x/y?access_token=<redacted> ok"
        );
        assert_eq!(
            scrub("line one\nSet-Cookie: a=b\nline three"),
            "line one\nSet-Cookie: <redacted>\nline three"
        );
    }

    #[test]
    fn error_chain_keeps_only_the_error_lines() {
        let stderr = "{\"level\":\"INFO\",\"msg\":\"probing\"}\nerror: fetch /me: boom\nerror: caused by x\n";
        assert_eq!(
            error_chain(stderr),
            "error: fetch /me: boom | error: caused by x"
        );
        assert_eq!(error_chain("segfault\n"), "segfault");
    }
}

#[cfg(test)]
mod ensure_browser_tests {
    use super::{download_browser_args, ensure_browser_args};

    /// A browser already on the machine is always tried first: the
    /// download is its own step, so the wizard can say it is happening.
    #[test]
    fn the_first_look_never_downloads_and_the_second_only_does() {
        let first = ensure_browser_args();
        let sources = first.last().expect("a --source value");
        assert!(!sources.contains("download"), "{sources}");
        assert!(sources.contains("system-browser"), "{sources}");
        let second = download_browser_args();
        assert_eq!(
            second.last().map(String::as_str),
            Some("download-playwright-browser")
        );
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::{gateway_from, gateway_refusal};

    /// `LATCHKEY_GATEWAY=` (set but empty) is how a shell script
    /// disables the gateway; treating it as one would refuse every
    /// login on a machine that has no gateway at all.
    #[test]
    fn an_empty_setting_is_no_gateway() {
        assert_eq!(gateway_from(None), None);
        assert_eq!(gateway_from(Some("".into())), None);
        assert_eq!(gateway_from(Some("  ".into())), None);
        assert_eq!(
            gateway_from(Some("http://127.0.0.1:9".into())).as_deref(),
            Some("http://127.0.0.1:9")
        );
    }

    /// The refusal has to say where the credentials are and must not
    /// point at `ensure-browser`, which is the command the old failure
    /// path named — and which does nothing useful under a gateway.
    #[test]
    fn the_refusal_names_the_gateway_and_no_command() {
        let message = gateway_refusal("http://gw.example:8080");
        assert!(message.contains("http://gw.example:8080"), "{message}");
        assert!(!message.contains("ensure-browser"), "{message}");
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
    /// captured from latchkey 3.11.0 (unchanged through 3.14.0). If it changes, this test
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
        assert_eq!(info.account_naming, AccountNaming::Service);
        assert_eq!(info.auth_options, vec!["browser", "set"]);
        assert_eq!(info.accounts.len(), 1);
        assert_eq!(info.accounts[0].account, "thad@imbue.com");
        assert_eq!(info.accounts[0].credential_status.as_deref(), Some("valid"));
        assert!(info.error.is_none());
    }

    /// What the wizard's paste form sends becomes exactly the `auth set`
    /// a person would type: `--account` first (a global option), each
    /// header or the `user:password` pair its own argument.
    #[test]
    fn a_pasted_credential_becomes_auth_set_arguments() {
        let basic = PastedCredential::Basic {
            username: " picard@enterprise.test ".into(),
            password: "tea-earl-grey".into(),
        };
        assert_eq!(
            set_args("fastmail-dav", "picard@enterprise.test", &basic).unwrap(),
            vec![
                "--account",
                "picard@enterprise.test",
                "auth",
                "set",
                "fastmail-dav",
                "-u",
                "picard@enterprise.test:tea-earl-grey",
            ]
        );
        let headers = PastedCredential::Headers {
            headers: vec!["Authorization: Bearer ro-token".into()],
        };
        assert_eq!(
            set_args("fastmail", "", &headers).unwrap(),
            vec![
                "auth",
                "set",
                "fastmail",
                "-H",
                "Authorization: Bearer ro-token"
            ]
        );
    }

    /// A service the wizard registered names no accounts, so the name in
    /// the box is the one a browser login stores under.
    #[test]
    fn a_registered_service_lets_the_person_name_the_account() {
        let v = json!({ "type": "user-registered", "authOptions": ["browser", "set"] });
        assert_eq!(
            parse_service_info("claude-ai", &v).account_naming,
            AccountNaming::Chosen
        );
    }

    #[test]
    fn strum_and_serde_spell_account_naming_the_same() {
        for naming in AccountNaming::VARIANTS {
            let json = serde_json::to_string(naming).unwrap();
            assert_eq!(json, format!("\"{}\"", naming.as_str()));
            assert_eq!(AccountNaming::parse(naming.as_str()), Some(*naming));
        }
    }

    /// A plugin's credential from files is stored with `set-nocurl`,
    /// which takes the folder as its only argument.
    #[test]
    fn a_token_folder_becomes_auth_set_nocurl_arguments() {
        let folder = PastedCredential::Directory {
            path: " ~/.garth ".into(),
        };
        assert_eq!(
            set_args("garmin", "picard", &folder).unwrap(),
            vec![
                "--account",
                "picard",
                "auth",
                "set-nocurl",
                "garmin",
                "~/.garth"
            ]
        );
        let refuse = |path: &str| {
            set_args(
                "garmin",
                "",
                &PastedCredential::Directory { path: path.into() },
            )
            .unwrap_err()
        };
        refuse("");
        refuse("--help");
        refuse("a\nb");
    }

    #[test]
    fn a_service_datalib_has_a_plugin_for_offers_its_ways_in_before_it_is_installed() {
        let info = plugin_service_info("garmin", None).unwrap();
        assert!(!info.registered);
        assert_eq!(info.auth_options, vec!["browser", "set"]);
        assert!(info.installs_plugin.unwrap().ends_with("plugins/garmin"));
        assert!(plugin_service_info("garmin", Some("http://gw")).is_none());
        assert!(plugin_service_info("slack", None).is_none());
    }

    /// A refusal names what is wrong without echoing the secret.
    #[test]
    fn a_malformed_credential_is_refused_without_repeating_it() {
        let refuse = |c: PastedCredential| set_args("fastmail", "", &c).unwrap_err();
        let e = refuse(PastedCredential::Headers {
            headers: vec!["Bearer s3cret".into()],
        });
        assert!(!e.contains("s3cret"), "{e}");
        let e = refuse(PastedCredential::Headers {
            headers: vec!["Authorization: Bearer s3cret\nX-Evil: 1".into()],
        });
        assert!(!e.contains("s3cret"), "{e}");
        refuse(PastedCredential::Headers { headers: vec![] });
        refuse(PastedCredential::Basic {
            username: "a:b".into(),
            password: "p".into(),
        });
        refuse(PastedCredential::Basic {
            username: "u".into(),
            password: String::new(),
        });
        let flag_account = PastedCredential::Headers {
            headers: vec!["Authorization: Bearer t".into()],
        };
        assert!(set_args("fastmail", "--all", &flag_account).is_err());
    }

    /// A set-only service's paste note has to name the credential's
    /// shape: `fastmail-dav` takes `-u user:password`, not the bearer
    /// header most services take. Shape from latchkey 3.15.0.
    #[test]
    fn carries_latchkeys_own_set_example() {
        let v = json!({
            "type": "built-in",
            "authOptions": ["set"],
            "credentials": {},
            "setCredentialsExample":
                "latchkey auth set fastmail-dav -u \"you@fastmail.com:<app password>\""
        });
        let info = parse_service_info("fastmail-dav", &v);
        assert_eq!(
            info.set_example.as_deref(),
            Some("latchkey auth set fastmail-dav -u \"you@fastmail.com:<app password>\"")
        );
        assert!(parse_service_info("x", &json!({})).set_example.is_none());
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
