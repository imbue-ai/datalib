//! `/api/packages`: the packages a library's config lists and whether each
//! is unpacked here, looking one up for a person to review, and fetching
//! one. The config is the record of what a library uses; the UI writes the
//! `[[packages]]` entries, this only fetches. `docs/dev/packages.md`.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use datalib_packages::{Lookup, Manifest};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// Fetches this process started, by manifest sha256, and how each went.
/// A failure stays until the next try, so the UI can say why.
static FETCHES: LazyLock<Mutex<BTreeMap<String, Fetch>>> = LazyLock::new(Default::default);

#[derive(Clone)]
enum Fetch {
    Running,
    Failed(String),
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageState {
    Installed,
    Fetching,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct PackageView {
    pub id: String,
    pub source: Option<String>,
    pub url: String,
    pub sha256: String,
    pub state: PackageState,
    pub error: Option<String>,
    /// What the package says it is; `None` until it is unpacked here.
    pub manifest: Option<Manifest>,
    /// The groups and applets whose entries name it.
    pub used_by: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PackagesResponse {
    /// This platform, as a package's `artifacts` names it.
    pub target: &'static str,
    pub packages: Vec<PackageView>,
}

/// `GET /api/packages`. Starts fetching any listed package that is not
/// unpacked here yet — a library copied from another machine, or a
/// config edited by hand — so looking is enough to bring it in.
pub async fn list(State(s): State<AppState>) -> Json<PackagesResponse> {
    let text = std::fs::read_to_string(s.config_path()).unwrap_or_default();
    let cfg = datalib_dag::config::check_text(&text).cfg;
    let mut packages = Vec::with_capacity(cfg.packages.len());
    for p in &cfg.packages {
        let manifest = datalib_packages::installed(&s.root, &p.sha256);
        let (state, error) = match (&manifest, fetch_state(&p.sha256)) {
            (Some(_), _) => (PackageState::Installed, None),
            (None, Some(Fetch::Running)) => (PackageState::Fetching, None),
            (None, Some(Fetch::Failed(e))) => (PackageState::Failed, Some(e)),
            (None, None) => {
                start_fetch(s.clone(), p.url.clone(), p.sha256.clone());
                (PackageState::Fetching, None)
            }
        };
        let mut used_by: Vec<String> = cfg
            .steps
            .iter()
            .filter(|st| st.package.as_deref() == Some(p.id.as_str()))
            .map(|st| st.group.clone().unwrap_or_else(|| st.id.clone()))
            .chain(
                cfg.applets
                    .iter()
                    .filter(|a| a.package.as_deref() == Some(p.id.as_str()))
                    .map(|a| a.id.clone()),
            )
            .collect();
        used_by.dedup();
        packages.push(PackageView {
            id: p.id.clone(),
            source: p.source.clone(),
            url: p.url.clone(),
            sha256: p.sha256.clone(),
            state,
            error,
            manifest,
            used_by,
        });
    }
    Json(PackagesResponse {
        target: datalib_packages::platform::this_target(),
        packages,
    })
}

#[derive(Debug, Deserialize)]
pub struct LookupRequest {
    pub source: String,
}

/// `POST /api/packages/lookup`: fetch a manifest for review. Writes nothing.
pub async fn lookup(Json(req): Json<LookupRequest>) -> Result<Json<Lookup>, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || datalib_packages::lookup(&req.source))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

#[derive(Debug, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    pub sha256: String,
}

/// `POST /api/packages/fetch`: unpack the package pinned at (`url`,
/// `sha256`) here, answering once it is. The UI calls it before writing
/// the `[[packages]]` entry, so the steps that name it can run at once.
pub async fn fetch(
    State(s): State<AppState>,
    Json(req): Json<FetchRequest>,
) -> Result<Json<Manifest>, (StatusCode, String)> {
    set_fetch(&req.sha256, Some(Fetch::Running));
    let root = s.root.clone();
    let sha = req.sha256.clone();
    let result =
        tokio::task::spawn_blocking(move || datalib_packages::ensure(&root, &req.url, &sha))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match result {
        Ok(m) => {
            set_fetch(&req.sha256, None);
            crate::reload_applets(&s).await;
            Ok(Json(m))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            set_fetch(&req.sha256, Some(Fetch::Failed(msg.clone())));
            Err((StatusCode::BAD_GATEWAY, msg))
        }
    }
}

fn fetch_state(sha: &str) -> Option<Fetch> {
    FETCHES.lock().unwrap().get(sha).cloned()
}

fn set_fetch(sha: &str, f: Option<Fetch>) {
    let mut map = FETCHES.lock().unwrap();
    match f {
        Some(f) => map.insert(sha.to_string(), f),
        None => map.remove(sha),
    };
}

fn start_fetch(s: AppState, url: String, sha: String) {
    set_fetch(&sha, Some(Fetch::Running));
    tokio::spawn(async move {
        let root = s.root.clone();
        let sha2 = sha.clone();
        let result =
            tokio::task::spawn_blocking(move || datalib_packages::ensure(&root, &url, &sha2)).await;
        match result {
            Ok(Ok(_)) => {
                set_fetch(&sha, None);
                crate::reload_applets(&s).await;
            }
            Ok(Err(e)) => {
                tracing::warn!("package {sha}: {e:#}");
                set_fetch(&sha, Some(Fetch::Failed(format!("{e:#}"))));
            }
            Err(e) => set_fetch(&sha, Some(Fetch::Failed(e.to_string()))),
        }
    });
}
