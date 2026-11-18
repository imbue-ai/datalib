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
/// reads the keychain, where a macOS prompt can wait on a person typing
/// a password, then makes a validation request per stored credential.
const SERVICES_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a browser login may stay pending before we call it lost.
/// Long, because the clock is a person reading a consent screen.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Set by whoever runs datalib behind a latchkey gateway. With it set,
/// the `latchkey` this module spawns is a thin client: `services info`,
/// `auth browser` and `curl` are forwarded to the gateway, and every
/// command that manages local state — `ensure-browser`, `services
/// register`, `auth set`, `auth clear` — is refused outright. The same
/// rule `datalib_etl_web::latchkey` applies, restated here because this
/// crate links no ETL code.
const GATEWAY_ENV_VAR: &str = datalib_env::LATCHKEY_GATEWAY;

/// An answer slower than this has usually waited on something other than
/// latchkey: a keychain prompt, or a service validating a credential.
const SLOW_ANSWER: Duration = Duration::from_secs(5);
/// latchkey's own name for its keychain item, when nothing renames it.
const DEFAULT_KEYCHAIN_ITEM: &str = "latchkey";

/// Which sign-ins this server's latchkey reaches, and where their key
/// comes from. Holds whether a key was handed over, never the key.
#[derive(Debug, PartialEq, Eq)]
struct LatchkeySetup {
    store: Option<String>,
    keychain_item: Option<String>,
    key_in_environment: bool,
    gateway: Option<String>,
    protected_library: bool,
}

fn latchkey_setup(
    store: Option<std::path::PathBuf>,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> LatchkeySetup {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    let gateway = gateway_from(var(GATEWAY_ENV_VAR));
    let key_in_environment = set(datalib_env::LATCHKEY_ENCRYPTION_KEY).is_some();
    let keychain_item = (gateway.is_none() && !key_in_environment).then(|| {
        set(datalib_env::LATCHKEY_KEYRING_SERVICE_NAME).map_or_else(
            || DEFAULT_KEYCHAIN_ITEM.to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    });
    LatchkeySetup {
        store: store.map(|dir| dir.display().to_string()),
        keychain_item,
        key_in_environment,
        gateway,
        protected_library: set(datalib_env::DATALIB_PROTECTED_LIBRARY).is_some(),
    }
}

fn log_setup(setup: &LatchkeySetup) {
    tracing::info!(
        store = setup.store.as_deref(),
        keychain_item = setup.keychain_item.as_deref(),
        key_in_environment = setup.key_in_environment,
        gateway = setup.gateway.as_deref(),
        protected_library = setup.protected_library,
        "where this library's latchkey keeps its sign-ins and finds their key"
    );
}

/// Says at launch what the log cannot otherwise show: `/api/latchkey` and
/// a step's `latchkey curl` go wherever these point.
pub fn log_latchkey_setup() {
    log_setup(&latchkey_setup(plugins::latchkey_dir(), |name| {
        std::env::var_os(name)
    }));
}

/// The request's own line is `debug` and says only the time.
fn note_slow_answer(service: &str, took: Duration) {
    if took >= SLOW_ANSWER {
        tracing::info!(
            service,
            ms = took.as_millis() as u64,
            "latchkey was slow to answer about a service; a keychain prompt may have been \
             waiting on a person"
        );
    }
}

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
    /// `--account` decides, a new name included: a service whose login
    /// cannot tell who signed in (every one registered with `latchkey
    /// services register`), and one with no browser login, which only a
    /// paste reaches.
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
    /// From `latchkey services info`'s `capabilities.detectsLoginAccount`.
    /// The flag is about a browser login, and latchkey says `true` for
    /// `fastmail-dav`, which has none, so the browser login is checked
    /// here. A gateway running a latchkey older than 3.18 reports no
    /// flag; the box then stays free to type in, as when latchkey cannot
    /// be asked at all.
    fn of_service(detects_login_account: Option<bool>, auth_options: &[String]) -> Self {
        let has_browser_login = auth_options.iter().any(|o| o == "browser");
        if has_browser_login && detects_login_account == Some(true) {
            AccountNaming::Service
        } else {
            AccountNaming::Chosen
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
    let command = format!("services info {service}");
    let asked = std::time::Instant::now();
    let answer = latchkey_json(&["services", "info", &service], SERVICES_TIMEOUT).await;
    note_slow_answer(&service, asked.elapsed());
    match answer {
        Ok(v) => Ok(Json(ServiceInfo {
            gateway,
            ..parse_service_info(&service, &v)
        })),
        Err(failed) => {
            // A name nobody has registered is a state the wizard can act
            // on, so it is reported as `registered: false` rather than
            // folded into the "latchkey could not be asked" note.
            let unknown = failed == LatchkeyFailed::UnknownService;
            if let Some(info) = unknown
                .then(|| plugin_service_info(&service, gateway.as_deref()))
                .flatten()
            {
                return Ok(Json(info));
            }
            Ok(Json(service_info_after_failure(
                service, gateway, &command, &failed,
            )))
        }
    }
}

/// The answer is still a 200, so the request's own log line says nothing
/// of a latchkey that could not be asked; the warning here is what does.
fn service_info_after_failure(
    service: String,
    gateway: Option<String>,
    command: &str,
    failed: &LatchkeyFailed,
) -> ServiceInfo {
    let unknown = *failed == LatchkeyFailed::UnknownService;
    if !unknown {
        tracing::warn!(
            service = service.as_str(),
            issue = failed.issue().as_str(),
            "could not ask latchkey about a service: {}",
            failed.detail(command)
        );
    }
    ServiceInfo {
        service,
        auth_options: Vec::new(),
        set_example: None,
        accounts: Vec::new(),
        registered: !unknown,
        cli: datalib_core::node_runtime::latchkey_cli_hint(),
        issue: (!unknown).then(|| failed.issue()),
        gateway,
        error: (!unknown).then(|| failed.detail(command)),
        installs_plugin: None,
        // Not registered yet: the wizard registers it, and a
        // registered service names nothing. Unreachable latchkey:
        // nothing to go on, and the box stays free to type in.
        account_naming: AccountNaming::Chosen,
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
    let auth_options: Vec<String> = v
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
    let account_naming = AccountNaming::of_service(
        v.pointer("/capabilities/detectsLoginAccount")
            .and_then(Value::as_bool),
        &auth_options,
    );
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
        account_naming,
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
    /// Run the login without latchkey's saved browser session — see
    /// [`EPHEMERAL_BROWSER_ENV`].
    #[serde(default)]
    pub ephemeral_browser: bool,
    /// Create the OAuth client the service signs in with before the
    /// login, with `latchkey auth browser-prepare`: Google's services,
    /// once latchkey has said it holds none ([`IssueKind::NoOauthClient`]).
    #[serde(default)]
    pub create_oauth_client: bool,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::VariantArray)]
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
    /// What went wrong, once `Failed`: the tail of latchkey's message, or
    /// ours when it said nothing useful.
    pub detail: String,
    /// What the attempt is doing now, while it runs.
    pub phase: ConnectPhase,
    /// What kind of trouble it was, once `Failed`.
    pub issue: Option<IssueKind>,
}

/// What a running browser login is doing, so the wizard can say what
/// it is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum ConnectPhase {
    /// Registering the service, finding a browser.
    Preparing,
    /// No browser on the machine: fetching one, a one-time download.
    DownloadingBrowser,
    /// The browser is open on Google Cloud's console, where latchkey
    /// creates a project and an OAuth client; minutes, mostly unattended.
    CreatingClient,
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
/// `auth set … $(pbpaste)` the docs give for a terminal, and one only an
/// upstream change can close (`docs/dev/latchkey.md` §"A pasted
/// credential is on latchkey's command line").
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
        Err(failed) => Err(refused(&service, &format!("auth set {service}"), failed)),
    }
}

/// A paste latchkey would not store, as the log and the wizard hear of
/// it.
fn refused(service: &str, command: &str, failed: LatchkeyFailed) -> (StatusCode, Json<Value>) {
    let issue = failed.issue();
    let detail = failed.detail(command);
    tracing::error!(
        service,
        issue = issue.as_str(),
        "latchkey {command} failed: {detail}"
    );
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "error": detail, "issue": issue })),
    )
}

// POST /api/latchkey/{service}/oauth_client

#[derive(Debug, Deserialize)]
pub struct SetOauthClientRequest {
    /// What the person pasted: Google Cloud's `client_secret.json`, or
    /// latchkey's own `{"clientId", "clientSecret"}`.
    pub client: String,
}

/// Store a pasted OAuth client as the service's preparation, with
/// `latchkey auth prepare`, so that a browser login can follow. Like a
/// pasted credential, the secret is on the child's command line for the
/// second it runs ([`set_credential`] says why).
pub async fn set_oauth_client(
    State(_s): State<AppState>,
    Path(service): Path<String>,
    Json(body): Json<SetOauthClientRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let service = validated_service(&service)?;
    if let Some(gateway) = latchkey_gateway() {
        return Err(err(StatusCode::CONFLICT, &gateway_refusal(&gateway)));
    }
    let client = crate::oauth_client::latchkey_client_json(&body.client)
        .map_err(|m| err(StatusCode::BAD_REQUEST, &m))?;
    let args = vec![
        "auth".to_string(),
        "prepare".to_string(),
        service.clone(),
        client,
    ];
    match latchkey_output(&args).await {
        Ok(_) => {
            tracing::info!(service, "latchkey stored a pasted OAuth client");
            Ok(Json(serde_json::json!({ "ok": true })))
        }
        Err(failed) => Err(refused(
            &service,
            &format!("auth prepare {service}"),
            failed,
        )),
    }
}

/// `[--account <a>] auth set <service> <curl args>`, or why the pasted
/// credential cannot be one. Every piece is its own argv element, so
/// nothing is parsed by a shell; the checks are about what curl will
/// later make of it. A header is printable ASCII; a name, a password or
/// a folder may be any text without a control character.
fn set_args(
    service: &str,
    account: &str,
    credential: &PastedCredential,
) -> Result<Vec<String>, String> {
    let ascii = |s: &str| s.chars().all(|c| c == ' ' || c.is_ascii_graphic());
    let text = |s: &str| {
        s.chars()
            .all(|c| c == ' ' || c.is_ascii_graphic() || (!c.is_ascii() && !c.is_control()))
    };
    let mut args: Vec<String> = Vec::new();
    if !account.is_empty() {
        if account.starts_with('-') || !text(account) {
            return Err("an account name is one line of text and does not start with '-'".into());
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
                if !name_ok || !ascii(header) || value.trim().is_empty() {
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
            if username.contains(':') || !text(username) || !text(password) {
                return Err(
                    "the username has no ':' and neither has a line break or control character"
                        .into(),
                );
            }
            args.extend(["-u".to_string(), format!("{username}:{password}")]);
        }
        PastedCredential::Directory { path } => {
            let path = path.trim();
            if path.is_empty() || path.starts_with('-') || !text(path) {
                return Err("a folder is one line of text and does not start with '-'".into());
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
    let create_oauth_client = body.create_oauth_client;
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
        detail: String::new(),
        phase: ConnectPhase::Preparing,
        issue: None,
    }));
    attempts()
        .lock()
        .expect("connect attempts mutex")
        .insert(id.clone(), slot.clone());

    // `--account` is a latchkey *global* option and must precede the
    // subcommand — the same rule `datalib_etl_web::latchkey` writes down
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
            fail(&slot, &service, IssueKind::Unknown, message);
            return;
        }
        // Registering is what makes the browser login exist at all, so
        // it has to happen first. latchkey refuses a name it already
        // holds; that refusal is the desired outcome, not a failure —
        // it is what keeps a hand-made registration untouched.
        if let Some(args) = register {
            match latchkey_output(&args).await {
                Ok(_) | Err(LatchkeyFailed::AlreadyRegistered) => {}
                Err(failed) => {
                    let command = format!("services register {service}");
                    fail(&slot, &service, failed.issue(), failed.detail(&command));
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
            if let Err(failed) = latchkey_output(&download_browser_args()).await {
                let message = format!(
                    "No browser found for the sign-in, and fetching one failed: {}",
                    failed.detail("ensure-browser --source download-playwright-browser"),
                );
                fail(&slot, &service, IssueKind::NoBrowser, message);
                return;
            }
        }
        if create_oauth_client {
            slot.lock().expect("connect slot mutex").phase = ConnectPhase::CreatingClient;
            let prepare = browser_prepare_args(&service);
            match tokio::time::timeout(CONNECT_TIMEOUT, latchkey_output(&prepare)).await {
                Ok(Ok(_)) => tracing::info!(service, "latchkey created an OAuth client"),
                Ok(Err(failed)) => {
                    let command = prepare.join(" ");
                    fail(&slot, &service, failed.issue(), failed.detail(&command));
                    return;
                }
                Err(_) => {
                    let message = "creating the OAuth client did not finish within 15 minutes; \
                                   start it again";
                    fail(&slot, &service, IssueKind::Unknown, message.to_string());
                    return;
                }
            }
        }
        slot.lock().expect("connect slot mutex").phase = ConnectPhase::SigningIn;

        let outcome =
            tokio::time::timeout(CONNECT_TIMEOUT, latchkey_output_env(&args, &login_env)).await;

        match outcome {
            Ok(Ok(output)) => succeed(&slot, &service, stored_account(&output)),
            Ok(Err(failed)) => {
                let command = format!("auth browser {service}");
                fail(&slot, &service, failed.issue(), failed.detail(&command));
            }
            Err(_) => fail(
                &slot,
                &service,
                IssueKind::Unknown,
                "the browser login did not finish within 15 minutes; start it again".to_string(),
            ),
        }
    });

    Ok(Json(ConnectStatus {
        id,
        status: ConnectState::Running,
        account: None,
        detail: String::new(),
        phase: ConnectPhase::Preparing,
        issue: None,
    }))
}

/// The attempt's outcome goes to the log as well as to the slot, which
/// is reaped on the first poll that reads it.
fn fail(slot: &Arc<Mutex<ConnectStatus>>, service: &str, issue: IssueKind, detail: String) {
    if failure_is_an_error(issue) {
        tracing::error!(
            service,
            issue = issue.as_str(),
            "latchkey login failed: {detail}"
        );
    } else {
        tracing::info!(
            service,
            issue = issue.as_str(),
            "latchkey login stopped: {detail}"
        );
    }
    let mut slot = slot.lock().expect("connect slot mutex");
    slot.status = ConnectState::Failed;
    slot.issue = Some(issue);
    slot.detail = detail;
}

/// latchkey holding no OAuth client is how every first Google sign-in
/// ends; the wizard answers it with a button that creates one.
fn failure_is_an_error(issue: IssueKind) -> bool {
    issue != IssueKind::NoOauthClient
}

fn succeed(slot: &Arc<Mutex<ConnectStatus>>, service: &str, account: Option<String>) {
    match &account {
        Some(a) => tracing::info!(service, account = %a, "latchkey login stored a credential"),
        None => tracing::info!(service, "latchkey login stored a credential"),
    }
    let mut slot = slot.lock().expect("connect slot mutex");
    slot.status = ConnectState::Ok;
    slot.account = account;
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

/// `auth browser-prepare`, which drives Google Cloud's console in the
/// browser to make a project, turn on the service's APIs and create a
/// Desktop OAuth client, then stores it as the service's preparation.
fn browser_prepare_args(service: &str) -> Vec<String> {
    vec![
        "auth".to_string(),
        "browser-prepare".to_string(),
        service.to_string(),
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

/// A browser login that neither loads nor saves latchkey's one saved
/// browser session, which every service and account shares. The wizard
/// asks for it when the login could otherwise file whoever is still
/// signed in under the wrong account (`freshBrowser` in
/// `ui/src/config/accountNaming.ts`).
const EPHEMERAL_BROWSER_ENV: &str = "LATCHKEY_EPHEMERAL_BROWSER";

/// Why a latchkey command failed. latchkey's own message is kept and
/// shown: every error latchkey 3.22.1 prints is one of its typed
/// messages, none of which repeats a header or token it was handed. (The
/// one that quotes its input, `auth prepare`'s JSON parse error, cannot
/// happen: `oauth_client.rs` always hands it valid JSON.)
#[derive(Debug, Clone, PartialEq, Eq)]
enum LatchkeyFailed {
    /// latchkey could not be started; our own words say where it was
    /// looked for.
    NotRun(String),
    /// latchkey has never heard of the service.
    UnknownService,
    /// `services register` for a name latchkey already holds.
    AlreadyRegistered,
    /// latchkey did not answer in the time allowed.
    TimedOut(Duration),
    /// It answered with something other than the JSON asked for.
    NotJson,
    /// Any other failure: its kind, and the tail of what latchkey said.
    Said { issue: IssueKind, stderr: String },
}

impl LatchkeyFailed {
    fn from_stderr(stderr: &str, gateway: bool) -> Self {
        if stderr.contains("Error: Unknown service:") {
            LatchkeyFailed::UnknownService
        } else if stderr.contains("already exists") {
            LatchkeyFailed::AlreadyRegistered
        } else {
            LatchkeyFailed::Said {
                issue: classify(stderr, gateway),
                stderr: tail(stderr),
            }
        }
    }

    fn issue(&self) -> IssueKind {
        match self {
            LatchkeyFailed::NotRun(_) => IssueKind::NoRuntime,
            LatchkeyFailed::UnknownService => IssueKind::NoCredential,
            LatchkeyFailed::AlreadyRegistered
            | LatchkeyFailed::TimedOut(_)
            | LatchkeyFailed::NotJson => IssueKind::Unknown,
            LatchkeyFailed::Said { issue, .. } => *issue,
        }
    }

    /// `command` is the latchkey subcommand that failed, for the
    /// failures latchkey said nothing useful about.
    fn detail(&self, command: &str) -> String {
        match self {
            LatchkeyFailed::NotRun(why) => format!("Could not run latchkey: {why}"),
            LatchkeyFailed::UnknownService => format!("latchkey {command}: no such service."),
            LatchkeyFailed::AlreadyRegistered => {
                format!("latchkey {command}: the service is already registered.")
            }
            LatchkeyFailed::TimedOut(limit) => format!(
                "latchkey {command} did not answer within {}s.",
                limit.as_secs()
            ),
            LatchkeyFailed::NotJson => {
                format!("latchkey {command} printed something that isn't JSON.")
            }
            LatchkeyFailed::Said { stderr, .. } => stderr.clone(),
        }
    }
}

async fn latchkey_output(args: &[String]) -> Result<String, LatchkeyFailed> {
    latchkey_output_env(args, &[]).await
}

async fn latchkey_output_env(
    args: &[String],
    env: &[(&str, &str)],
) -> Result<String, LatchkeyFailed> {
    // The same resolution `datalib_etl_web::latchkey` uses, reached through
    // `datalib_core` so the pin is not spelled twice.
    let mut cmd: Command = datalib_core::node_runtime::latchkey_command()
        .map_err(|e| LatchkeyFailed::NotRun(format!("{e:#}")))?
        .into();
    // A login that runs past `CONNECT_TIMEOUT` is dropped, and
    // `auth browser-prepare` waits for a Google sign-in with no limit of
    // its own; without this it would outlive the attempt indefinitely.
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }
    datalib_env::inherit_only(cmd.as_std_mut(), datalib_env::Child::Latchkey, &[]);
    let out = cmd.output().await.map_err(|e| {
        LatchkeyFailed::NotRun(format!(
            "{e}. Install it, or check that {} works.",
            datalib_core::node_runtime::latchkey_cli_hint()
        ))
    })?;
    if !out.status.success() {
        return Err(LatchkeyFailed::from_stderr(
            &String::from_utf8_lossy(&out.stderr),
            latchkey_gateway().is_some(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn latchkey_json(args: &[&str], timeout: Duration) -> Result<Value, LatchkeyFailed> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let text = tokio::time::timeout(timeout, latchkey_output(&owned))
        .await
        .map_err(|_| LatchkeyFailed::TimedOut(timeout))??;
    serde_json::from_str(&text).map_err(|_| LatchkeyFailed::NotJson)
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
mod latchkey_failed_tests {
    use super::*;

    fn logged_while(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = captured.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    fn environment<'a>(
        vars: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, value)| value.into())
        }
    }

    /// A protected library's log says which store and keychain item its
    /// latchkey uses, which a person otherwise cannot see from outside it.
    #[test]
    fn the_launch_says_which_store_and_keychain_item_a_library_uses() {
        let store = std::path::PathBuf::from("/c/Libraries/Mail/system/latchkey");
        let setup = latchkey_setup(
            Some(store),
            environment(&[
                ("LATCHKEY_KEYRING_SERVICE_NAME", "Datalib Latchkey - Mail"),
                ("DATALIB_PROTECTED_LIBRARY", "1"),
            ]),
        );
        let log = logged_while(|| log_setup(&setup));
        assert!(log.contains("INFO"), "{log}");
        assert!(log.contains("/c/Libraries/Mail/system/latchkey"), "{log}");
        assert!(log.contains("Datalib Latchkey - Mail"), "{log}");
        assert!(log.contains("protected_library=true"), "{log}");
    }

    /// An ordinary library uses latchkey's own item; one handed a key or a
    /// gateway reads no keychain, and the line must not suggest it does.
    #[test]
    fn the_setup_names_a_keychain_item_only_when_one_is_read() {
        let item = |vars: &[(&str, &str)]| latchkey_setup(None, environment(vars)).keychain_item;
        assert_eq!(item(&[]).as_deref(), Some("latchkey"));
        assert_eq!(item(&[("LATCHKEY_ENCRYPTION_KEY", "k")]), None);
        assert_eq!(item(&[("LATCHKEY_GATEWAY", "http://g")]), None);
        assert_eq!(
            item(&[("LATCHKEY_ENCRYPTION_KEY", "")]).as_deref(),
            Some("latchkey")
        );
    }

    /// The line about a key handed over must never carry the key.
    #[test]
    fn the_launch_line_does_not_carry_a_key() {
        let setup = latchkey_setup(
            None,
            environment(&[("LATCHKEY_ENCRYPTION_KEY", "tea-earl-grey-hot")]),
        );
        assert!(setup.key_in_environment);
        let log = logged_while(|| log_setup(&setup));
        assert!(log.contains("key_in_environment=true"), "{log}");
        assert!(!log.contains("tea-earl-grey-hot"), "{log}");
    }

    /// Someone typing a keychain password used to leave only the request's
    /// `debug` line, however long it took.
    #[test]
    fn a_slow_answer_is_in_the_log_and_a_quick_one_is_not() {
        let slow = logged_while(|| note_slow_answer("github", Duration::from_secs(40)));
        assert!(slow.contains("INFO"), "{slow}");
        assert!(slow.contains("github"), "{slow}");
        assert!(slow.contains("ms=40000"), "{slow}");
        assert!(slow.contains("keychain"), "{slow}");
        let quick = logged_while(|| note_slow_answer("github", Duration::from_millis(311)));
        assert_eq!(quick, "");
    }

    /// A keychain prompt nobody answered in time used to leave only the
    /// request's `debug` line, a 200, which the log card does not show.
    #[test]
    fn a_latchkey_that_timed_out_is_a_warning_and_the_wizards_error() {
        let command = "services info github";
        let failed = LatchkeyFailed::TimedOut(SERVICES_TIMEOUT);
        let mut info = None;
        let log = logged_while(|| {
            info = Some(service_info_after_failure(
                "github".to_string(),
                None,
                command,
                &failed,
            ));
        });
        let said = "latchkey services info github did not answer within 120s.";
        assert_eq!(info.unwrap().error.as_deref(), Some(said));
        assert!(log.contains("WARN"), "{log}");
        assert!(log.contains(said), "{log}");
    }

    /// A name latchkey has not heard of is what the wizard registers
    /// next, not something that went wrong.
    #[test]
    fn a_service_latchkey_has_not_heard_of_is_not_a_warning() {
        let mut info = None;
        let log = logged_while(|| {
            info = Some(service_info_after_failure(
                "claude-ai".to_string(),
                None,
                "services info claude-ai",
                &LatchkeyFailed::UnknownService,
            ));
        });
        assert!(!info.unwrap().registered);
        assert_eq!(log, "");
    }

    /// latchkey's own words are what the wizard shows and the log keeps:
    /// they say what went wrong better than any sentence of ours.
    #[test]
    fn latchkeys_message_is_the_detail() {
        let said = "Error: Login failed: access_denied";
        let failed = LatchkeyFailed::from_stderr(said, false);
        assert_eq!(failed.issue(), IssueKind::Unknown);
        assert_eq!(failed.detail("auth browser google-gmail"), said);
    }

    /// The kind is read off what latchkey said, so the wizard says the
    /// right sentence above it.
    #[test]
    fn the_kind_is_read_off_latchkeys_message() {
        let f = |s: &str| LatchkeyFailed::from_stderr(s, false);
        assert_eq!(
            f("Error: Unknown service: claude-ai"),
            LatchkeyFailed::UnknownService
        );
        assert_eq!(
            f("Error: Service 'claude-ai' already exists."),
            LatchkeyFailed::AlreadyRegistered
        );
        assert_eq!(
            f("Error: Service google-gmail requires preparation first.").issue(),
            IssueKind::NoOauthClient
        );
        assert_eq!(
            f("No browser configured. Run 'latchkey ensure-browser' first.").issue(),
            IssueKind::NoBrowser
        );
        assert_eq!(f("anything else").issue(), IssueKind::Unknown);
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
mod failure_level_tests {
    use super::{classify, failure_is_an_error};

    /// The first try of a Google sign-in ends in latchkey saying it holds
    /// no OAuth client yet, which the wizard answers with a button. Logged
    /// as an error, it read as a sign-in that had failed.
    #[test]
    fn a_missing_oauth_client_is_not_an_error() {
        let refusal = "Error: Service google-gmail requires preparation first. Run \
                       'latchkey auth browser-prepare google-gmail' before logging in, or \
                       pass --account to reuse the client stored with an existing account's \
                       credentials.";
        assert!(!failure_is_an_error(classify(refusal, false)));
        let timeout = "locator.waitFor: Timeout 12000ms exceeded.";
        assert!(failure_is_an_error(classify(timeout, false)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape `latchkey services info <name>` printed in 3.18.0. A
    /// literal, so it cannot notice latchkey changing; `e2e_auth`'s
    /// `accounts.spec.ts` asks the real one.
    #[test]
    fn reads_a_services_info_payload() {
        let v = json!({
            "type": "built-in",
            "baseApiUrls": ["https://gmail.googleapis.com/"],
            "authOptions": ["browser", "set"],
            "capabilities": { "detectsLoginAccount": true },
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

    /// A login that cannot tell who signed in stores under the name in
    /// the box: every registered service, and built-ins like `openrouter`.
    #[test]
    fn a_login_that_cannot_tell_who_signed_in_lets_the_person_name_the_account() {
        let v = json!({
            "type": "user-registered",
            "authOptions": ["browser", "set"],
            "capabilities": { "detectsLoginAccount": false },
        });
        assert_eq!(
            parse_service_info("claude-ai", &v).account_naming,
            AccountNaming::Chosen
        );
    }

    /// A gateway on a latchkey older than 3.18 reports no capabilities;
    /// the box stays free to type in rather than claim the service
    /// names the account.
    #[test]
    fn no_capabilities_lets_the_person_name_the_account() {
        let v = json!({ "type": "built-in", "authOptions": ["browser", "set"] });
        assert_eq!(
            parse_service_info("slack", &v).account_naming,
            AccountNaming::Chosen
        );
    }

    /// `fastmail-dav`, `notion` and `gitlab` have no browser login, so a
    /// pasted credential is the only way in and the person names it —
    /// though latchkey reports `detectsLoginAccount: true` for them.
    #[test]
    fn a_built_in_service_without_a_browser_login_lets_the_person_name_it() {
        let v = json!({
            "type": "built-in",
            "authOptions": ["set"],
            "capabilities": { "detectsLoginAccount": true },
        });
        assert_eq!(
            parse_service_info("fastmail-dav", &v).account_naming,
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

    /// Only a line break was refused, so a NUL, a tab or an escape
    /// sequence reached latchkey's argv and curl's header. A header is
    /// printable ASCII; a name, password or folder is text without a
    /// control character.
    #[test]
    fn a_control_character_is_refused_anywhere() {
        let bearer = || PastedCredential::Headers {
            headers: vec!["Authorization: Bearer t".into()],
        };
        for bad in ["\0", "\t", "\x1b[2J", "\x7f", "\u{85}"] {
            assert!(
                set_args("fastmail", &format!("pic{bad}ard"), &bearer()).is_err(),
                "account {bad:?}"
            );
            let header = PastedCredential::Headers {
                headers: vec![format!("Authorization: Bearer t{bad}t")],
            };
            assert!(set_args("fastmail", "", &header).is_err(), "header {bad:?}");
            let basic = PastedCredential::Basic {
                username: "picard".into(),
                password: format!("en{bad}gage"),
            };
            assert!(
                set_args("fastmail", "", &basic).is_err(),
                "password {bad:?}"
            );
            let folder = PastedCredential::Directory {
                path: format!("/tmp/gar{bad}min"),
            };
            assert!(set_args("garmin", "", &folder).is_err(), "folder {bad:?}");
        }
        let header = PastedCredential::Headers {
            headers: vec!["Authorization: Bearer tö".into()],
        };
        assert!(set_args("fastmail", "", &header).is_err());
        let folder = PastedCredential::Directory {
            path: "/Users/picard/Téléchargements/garmin".into(),
        };
        assert!(set_args("garmin", "Jean-Luc Picard", &folder).is_ok());
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
