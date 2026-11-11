//! `POST /sql` and `GET /sql/stores`: any store under the data root, read
//! with SQL the caller writes, a page at a time, as JSON. What it promises a
//! caller is in `docs/user/applets.md` § "Reading any store with SQL"; what
//! keeps a query to reading is `datalib_sql_guard`, and which files it opens
//! is `store.rs`.

mod store;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sqlx::{Connection, Row, TypeInfo, ValueRef};
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
    /// The values of the statement's `?` or `?NNN` parameters, in order.
    #[serde(default)]
    pub params: Vec<Value>,
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
    let store = store::resolve(root, &request.store).map_err(|e| match e {
        store::NotAStore::Refused(why) => Failure::bad(why),
        store::NotAStore::Missing(why) => Failure::new(StatusCode::NOT_FOUND, why),
    })?;
    let _slot = tokio::time::timeout_at(deadline.into(), slots.acquire())
        .await
        .map_err(|_| past_deadline(budget))?
        .map_err(|e| Failure::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (mut conn, commit) = open(&store, request.commit.as_deref()).await?;
    let read = read_page(&mut conn, &request, limit, deadline).await;
    // A read-only connection has nothing to flush; a failed close loses nothing.
    let _ = conn.close().await;
    let (columns, rows, next_offset) = read.map_err(|e| {
        if Instant::now() >= deadline {
            past_deadline(budget)
        } else {
            e
        }
    })?;
    Ok(Page {
        store: store.name,
        kind: store.kind,
        commit,
        columns,
        rows,
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

async fn open(
    store: &store::Store,
    commit: Option<&str>,
) -> Result<(SqliteConnection, Option<String>), Failure> {
    let failed = |e: sqlx::Error| Failure::bad(format!("open {}: {e}", store.name));
    match store.kind {
        StoreKind::Sqlite => {
            if commit.is_some() {
                return Err(Failure::bad(format!(
                    "{} is plain SQLite, which has no commits to read",
                    store.name
                )));
            }
            let opts = datalib_sqlite_open::file(&store.path)
                .read_only(true)
                .create_if_missing(false);
            let conn = SqliteConnection::connect_with(&opts)
                .await
                .map_err(failed)?;
            Ok((conn, None))
        }
        StoreKind::Doltlite => {
            let pin = match commit {
                Some(commit) => {
                    datalib_pin::Pin::at(commit).map_err(|e| Failure::bad(e.to_string()))?
                }
                None => head(store).await?,
            };
            let opts = datalib_sqlite_open::revision(&store.path, pin.commit())
                .read_only(true)
                .create_if_missing(false);
            let conn = SqliteConnection::connect_with(&opts).await.map_err(|e| {
                Failure::new(
                    StatusCode::NOT_FOUND,
                    format!("{} at {}: {e}", store.name, pin.commit()),
                )
            })?;
            Ok((conn, Some(pin.commit().to_string())))
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

type PageRows = (Vec<String>, Vec<Vec<Value>>, Option<u64>);

async fn read_page(
    conn: &mut SqliteConnection,
    request: &SqlRequest,
    limit: usize,
    deadline: Instant,
) -> Result<PageRows, Failure> {
    guard::guard(conn, deadline)
        .await
        .map_err(|e| Failure::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let sql = guard::paged(&request.sql, request.offset, limit);
    let checked = guard::check(conn, &sql)
        .await
        .map_err(|refusal| match refusal {
            guard::Refusal::Refused(what) => Failure::new(
                StatusCode::FORBIDDEN,
                format!("this reads and nothing else, and the statement asks for {what}"),
            ),
            guard::Refusal::NotOneStatement => {
                Failure::bad("send one SELECT statement (a WITH … SELECT is one)")
            }
            guard::Refusal::Sql(message) => Failure::bad(message),
        })?;
    let binds = ordered(&request.params, &checked.parameters).map_err(Failure::bad)?;

    // Audited: the caller's SQL is what this route runs. `guard::check`
    // has just prepared this exact text on this connection, which is
    // read-only and holds every statement to the authorizer; the LIMIT and
    // OFFSET are integers this function formatted.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for bind in binds {
        query = match bind {
            Bind::Null => query.bind(None::<i64>),
            Bind::Integer(i) => query.bind(i),
            Bind::Real(f) => query.bind(f),
            Bind::Text(s) => query.bind(s),
        };
    }
    let mut stream = query.fetch(&mut *conn);
    let mut rows = Vec::new();
    let mut bytes = 0;
    let mut more = false;
    while let Some(row) = stream
        .try_next()
        .await
        .map_err(|e| Failure::bad(sql_message(&e)))?
    {
        if rows.len() == limit {
            more = true;
            break;
        }
        let (values, size) = json_row(&row).map_err(|e| Failure::bad(sql_message(&e)))?;
        rows.push(values);
        bytes += size;
        if bytes > MAX_PAGE_BYTES {
            more = true;
            break;
        }
    }
    let next_offset = more.then(|| request.offset + rows.len() as u64);
    Ok((checked.columns, rows, next_offset))
}

fn sql_message(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => db.message().to_string(),
        other => other.to_string(),
    }
}

/// A row as JSON, and about how many bytes its values hold. A blob is
/// `{"base64": …}`; a real SQLite cannot write as JSON (an infinity) is
/// its text.
fn json_row(row: &SqliteRow) -> Result<(Vec<Value>, usize), sqlx::Error> {
    let mut values = Vec::with_capacity(row.len());
    let mut size = 0;
    for i in 0..row.len() {
        let raw = row.try_get_raw(i)?;
        let value = if raw.is_null() {
            Value::Null
        } else {
            match raw.type_info().name() {
                "INTEGER" => row.try_get_unchecked::<i64, _>(i)?.into(),
                "REAL" => {
                    let f = row.try_get_unchecked::<f64, _>(i)?;
                    serde_json::Number::from_f64(f)
                        .map_or_else(|| f.to_string().into(), Value::Number)
                }
                "BLOB" => {
                    let bytes = row.try_get_unchecked::<Vec<u8>, _>(i)?;
                    size += bytes.len();
                    serde_json::json!({
                        "base64": base64::engine::general_purpose::STANDARD.encode(bytes)
                    })
                }
                _ => {
                    let text = row.try_get_unchecked::<String, _>(i)?;
                    size += text.len();
                    text.into()
                }
            }
        };
        size += 8;
        values.push(value);
    }
    Ok((values, size))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Bind {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
}

fn bind_of(value: &Value) -> Result<Bind, String> {
    Ok(match value {
        Value::Null => Bind::Null,
        Value::Bool(b) => Bind::Integer(i64::from(*b)),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Bind::Integer(i),
            (None, Some(_)) if n.is_u64() => {
                return Err(format!("{n} is past SQLite's largest integer"))
            }
            (None, Some(f)) => Bind::Real(f),
            (None, None) => return Err(format!("{n} is not a number SQLite holds")),
        },
        Value::String(s) => Bind::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err("a parameter is a string, a number, a boolean or null; \
                 pass JSON as a string and read it with json(?)"
                .into())
        }
    })
}

/// The values to bind, one per parameter (`names[i]` is parameter
/// `i + 1`'s name). sqlx binds by position only, so a named parameter
/// (`:x`, `@x`, `$x`) is refused rather than left unbound.
pub fn ordered(params: &[Value], names: &[Option<String>]) -> Result<Vec<Bind>, String> {
    if let Some(named) = names.iter().flatten().find(|name| !name.starts_with('?')) {
        return Err(format!(
            "{named} is a named parameter; write `?1`, `?2`, … and send params as a list"
        ));
    }
    if params.len() != names.len() {
        return Err(format!(
            "the statement has {} parameter(s) and params holds {}",
            names.len(),
            params.len()
        ));
    }
    params.iter().map(bind_of).collect()
}
