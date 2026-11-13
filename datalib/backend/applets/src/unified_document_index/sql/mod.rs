//! `POST /sql` and `GET /sql/stores`: any store under the data root, read
//! with SQL the caller writes, a page at a time, as JSON. What it promises a
//! caller is in `docs/user/applets.md` § "Reading any store with SQL"; what
//! keeps a query to reading is `datalib_sql_guard`, and which files it opens
//! is `store.rs`.

mod store;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;

use datalib_sql_guard as guard;

use super::Index;
pub use store::StoreKind;

/// Under the gateway's 30 s proxy timeout (`APPLET_READ_TIMEOUT` in
/// datalib-http), so a query that runs long ends with a message saying so
/// rather than a bare proxy timeout. Opening the store counts against it.
pub const DEADLINE: Duration = Duration::from_secs(20);

const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 10_000;

/// A page stops early past this many bytes of values, so a table of large
/// blobs comes back in pieces; a single row larger than this still comes
/// back whole, alone on its page.
const MAX_PAGE_BYTES: usize = 8 << 20;

/// Queries running at once; more wait their turn, inside their deadline.
pub const AT_ONCE: usize = 4;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRequest {
    pub store: String,
    pub sql: String,
    /// The statement's parameters: a list for `?` and `?NNN`, an object
    /// for `:name`, `@name` and `$name`.
    #[serde(default)]
    pub params: Value,
    /// A doltlite store's commit to read; its newest when absent.
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub store: String,
    pub kind: StoreKind,
    /// The commit read, for a doltlite store: pass it back with the next
    /// page's `offset` to read the same commit.
    pub commit: Option<String>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Where the next page starts; `None` when this one is the last.
    pub next_offset: Option<u64>,
}

#[derive(Debug)]
pub struct Failure {
    pub status: StatusCode,
    pub error: String,
}

impl Failure {
    fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Failure {
            status,
            error: error.into(),
        }
    }

    fn bad(error: impl Into<String>) -> Self {
        Failure::new(StatusCode::BAD_REQUEST, error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.error })),
        )
            .into_response()
    }
}

pub(super) async fn handler(
    State(index): State<Index>,
    Json(request): Json<SqlRequest>,
) -> Result<Json<Page>, Failure> {
    run(&index.root, &index.sql_slots, request, DEADLINE)
        .await
        .map(Json)
}

#[derive(Debug, Serialize)]
pub struct Stores {
    pub stores: Vec<store::Listed>,
}

pub(super) async fn stores_handler(State(index): State<Index>) -> Result<Json<Stores>, Failure> {
    let root = index.root.clone();
    tokio::task::spawn_blocking(move || store::list(&root))
        .await
        .map(|stores| Json(Stores { stores }))
        .map_err(|e| Failure::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn run(
    root: &Path,
    slots: &Semaphore,
    request: SqlRequest,
    budget: Duration,
) -> Result<Page, Failure> {
    let deadline = Instant::now() + budget;
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Failure::bad(format!(
            "limit {limit}: a page holds 1 to {MAX_LIMIT} rows"
        )));
    }
    let params = params_of(request.params).map_err(Failure::bad)?;
    let store = store::resolve(root, &request.store).map_err(|e| match e {
        store::NotAStore::Refused(why) => Failure::bad(why),
        store::NotAStore::Missing(why) => Failure::new(StatusCode::NOT_FOUND, why),
    })?;
    let _slot = tokio::time::timeout_at(deadline.into(), slots.acquire())
        .await
        .map_err(|_| past_deadline(budget))?
        .map_err(|e| Failure::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (name, commit) = name_to_open(&store, request.commit.as_deref()).await?;
    let window = guard::Window {
        offset: request.offset,
        limit,
        max_bytes: MAX_PAGE_BYTES,
    };
    let (sql, kind, store_name) = (request.sql, store.kind, store.name.clone());
    let read = tokio::task::spawn_blocking(move || {
        let mut reader = guard::Reader::open(&name, deadline).map_err(|e| match kind {
            StoreKind::Doltlite => Failure::new(StatusCode::NOT_FOUND, e),
            StoreKind::Sqlite => Failure::bad(e),
        })?;
        reader.read(&sql, &params, window).map_err(refused)
    })
    .await
    .map_err(|e| Failure::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        if Instant::now() >= deadline {
            past_deadline(budget)
        } else {
            e
        }
    })?;
    let next_offset = read.more.then(|| request.offset + read.rows.len() as u64);
    Ok(Page {
        store: store_name,
        kind,
        commit,
        columns: read.columns,
        rows: read
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(json_value).collect())
            .collect(),
        next_offset,
    })
}

fn past_deadline(budget: Duration) -> Failure {
    Failure::new(
        StatusCode::GATEWAY_TIMEOUT,
        format!(
            "the query ran past {}s and was stopped; narrow it, or page with a smaller limit",
            budget.as_secs_f32()
        ),
    )
}

fn refused(refusal: guard::Refusal) -> Failure {
    match refusal {
        guard::Refusal::Refused(what) => Failure::new(
            StatusCode::FORBIDDEN,
            format!("this reads and nothing else, and the statement asks for {what}"),
        ),
        guard::Refusal::NotOneStatement => {
            Failure::bad("send one SELECT statement (a WITH … SELECT is one)")
        }
        guard::Refusal::Params(why) | guard::Refusal::Sql(why) => Failure::bad(why),
    }
}

/// The name SQLite opens for `store`, and the commit it reads: the one
/// asked for, or the newest.
async fn name_to_open(
    store: &store::Store,
    commit: Option<&str>,
) -> Result<(PathBuf, Option<String>), Failure> {
    match store.kind {
        StoreKind::Sqlite => match commit {
            Some(_) => Err(Failure::bad(format!(
                "{} is plain SQLite, which has no commits to read",
                store.name
            ))),
            None => Ok((datalib_sqlite_open::literal(&store.path), None)),
        },
        StoreKind::Doltlite => {
            let pin = match commit {
                Some(commit) => {
                    datalib_pin::Pin::at(commit).map_err(|e| Failure::bad(e.to_string()))?
                }
                None => head(store).await?,
            };
            Ok((
                datalib_sqlite_open::revision_name(&store.path, pin.commit()),
                Some(pin.commit().to_string()),
            ))
        }
    }
}

async fn head(store: &store::Store) -> Result<datalib_pin::Pin, Failure> {
    let pool = datalib_pin::open_reader(&store.path)
        .await
        .map_err(|e| Failure::bad(format!("open {}: {e}", store.name)))?;
    let head = datalib_pin::head(&pool).await;
    pool.close().await;
    head.map_err(|e| Failure::bad(format!("{}: {e}", store.name)))?
        .ok_or_else(|| {
            Failure::new(
                StatusCode::NOT_FOUND,
                format!("{} has no commit to read yet", store.name),
            )
        })
}

/// A cell as JSON. A blob is `{"base64": …}`; a real JSON cannot hold (an
/// infinity) is its text.
fn json_value(value: guard::Value) -> Value {
    match value {
        guard::Value::Null => Value::Null,
        guard::Value::Integer(i) => i.into(),
        guard::Value::Real(f) => {
            serde_json::Number::from_f64(f).map_or_else(|| f.to_string().into(), Value::Number)
        }
        guard::Value::Text(s) => s.into(),
        guard::Value::Blob(b) => {
            serde_json::json!({ "base64": base64::engine::general_purpose::STANDARD.encode(b) })
        }
    }
}

/// `params` as the request sent it: absent, a list, or an object of names.
pub fn params_of(params: Value) -> Result<guard::Params, String> {
    Ok(match params {
        Value::Null => guard::Params::ByPosition(Vec::new()),
        Value::Array(values) => {
            guard::Params::ByPosition(values.iter().map(bind_of).collect::<Result<_, _>>()?)
        }
        Value::Object(pairs) => guard::Params::ByName(
            pairs
                .into_iter()
                .map(|(k, v)| Ok((k, bind_of(&v)?)))
                .collect::<Result<_, String>>()?,
        ),
        _ => return Err("params is a list (for `?1`, `?2`) or an object (for `:name`)".into()),
    })
}

fn bind_of(value: &Value) -> Result<guard::Value, String> {
    Ok(match value {
        Value::Null => guard::Value::Null,
        Value::Bool(b) => guard::Value::Integer(i64::from(*b)),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => guard::Value::Integer(i),
            (None, Some(_)) if n.is_u64() => {
                return Err(format!("{n} is past SQLite's largest integer"))
            }
            (None, Some(f)) => guard::Value::Real(f),
            (None, None) => return Err(format!("{n} is not a number SQLite holds")),
        },
        Value::String(s) => guard::Value::Text(s.clone()),
        Value::Object(o) if o.len() == 1 && o.contains_key("base64") => {
            let blob = o["base64"]
                .as_str()
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .ok_or("{\"base64\": …} holds a blob as standard base64 text")?;
            guard::Value::Blob(blob)
        }
        Value::Array(_) | Value::Object(_) => {
            return Err("a parameter is a string, a number, a boolean, null or \
                 {\"base64\": …}; pass JSON as a string and read it with json(?)"
                .into())
        }
    })
}
