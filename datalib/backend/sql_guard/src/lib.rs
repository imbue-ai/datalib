//! What keeps SQL a caller wrote to reading a store: an authorizer that
//! lets a statement do only what [`allows`] names, a deadline, and one
//! statement, which [`paged`] reads a page at a time. A read-only open is
//! needed too, and not enough on its own: on one, even a detached one,
//! `SELECT dolt_creds_new()` still writes a key under `~/.doltlite/creds`.
//!
//! Doltlite is the SQLite API, so the C entry points are declared here, as
//! `etl/src/doltlite_raw.rs` does; `libsqlite3-sys` is not a direct
//! dependency.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;
use std::time::Instant;

use sqlx::sqlite::SqliteConnection;

extern "C" {
    fn sqlite3_set_authorizer(
        db: *mut c_void,
        callback: Option<
            unsafe extern "C" fn(
                *mut c_void,
                c_int,
                *const c_char,
                *const c_char,
                *const c_char,
                *const c_char,
            ) -> c_int,
        >,
        arg: *mut c_void,
    ) -> c_int;
    fn sqlite3_progress_handler(
        db: *mut c_void,
        n_ops: c_int,
        callback: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
        arg: *mut c_void,
    );
    fn sqlite3_limit(db: *mut c_void, id: c_int, new_val: c_int) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        n_byte: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    fn sqlite3_bind_parameter_count(stmt: *mut c_void) -> c_int;
    fn sqlite3_bind_parameter_name(stmt: *mut c_void, i: c_int) -> *const c_char;
    fn sqlite3_column_count(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_name(stmt: *mut c_void, i: c_int) -> *const c_char;
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
}

const SQLITE_OK: c_int = 0;
const SQLITE_DENY: c_int = 1;
const SQLITE_INSERT: c_int = 18;
const SQLITE_PRAGMA: c_int = 19;
const SQLITE_READ: c_int = 20;
const SQLITE_SELECT: c_int = 21;
const SQLITE_TRANSACTION: c_int = 22;
const SQLITE_UPDATE: c_int = 23;
const SQLITE_ATTACH: c_int = 24;
const SQLITE_FUNCTION: c_int = 31;
const SQLITE_RECURSIVE: c_int = 33;
const SQLITE_DELETE: c_int = 9;
const SQLITE_LIMIT_ATTACHED: c_int = 7;

/// How many virtual-machine steps pass between two looks at the clock.
const STEPS_PER_CHECK: c_int = 1000;

/// One thing a statement asks to do, as SQLite's authorizer reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action<'a> {
    Read,
    Select,
    Recursive,
    Function(&'a str),
    Pragma(&'a str),
    Other(c_int),
}

impl<'a> Action<'a> {
    fn of(code: c_int, first: &'a str, second: &'a str) -> Self {
        match code {
            SQLITE_READ => Action::Read,
            SQLITE_SELECT => Action::Select,
            SQLITE_RECURSIVE => Action::Recursive,
            SQLITE_FUNCTION => Action::Function(second),
            SQLITE_PRAGMA => Action::Pragma(first),
            other => Action::Other(other),
        }
    }

    fn described(&self) -> String {
        match self {
            Action::Function(name) => format!("the function {name}()"),
            Action::Pragma(name) => format!("PRAGMA {name}"),
            Action::Other(code) => match *code {
                SQLITE_INSERT | SQLITE_UPDATE | SQLITE_DELETE => "a write".to_string(),
                SQLITE_ATTACH => "ATTACH".to_string(),
                SQLITE_TRANSACTION => "a transaction".to_string(),
                code => format!("an action SQLite numbers {code}"),
            },
            Action::Read | Action::Select | Action::Recursive => "a read".to_string(),
        }
    }
}

/// Doltlite's own SQL functions are the ones that reach past the store:
/// `dolt_push`, `dolt_clone`, `dolt_fetch`, `dolt_creds_new`. These only
/// answer. Every other function is SQLite's, and none of those writes:
/// `load_extension` is off unless a connection turns it on, and this
/// library has none of the shell's `readfile`, `writefile` or `fsdir`.
const DOLT_READ_FUNCTIONS: &[&str] = &[
    "dolt_hashof",
    "dolt_hashof_catalog",
    "dolt_hashof_db",
    "dolt_hashof_index",
    "dolt_hashof_table",
    "dolt_merge_base",
    "dolt_version",
];

/// The pragmas a `pragma_<name>(…)` table may read: what a table, an index
/// or the engine holds.
const READ_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "function_list",
    "module_list",
    "pragma_list",
    "collation_list",
    "compile_options",
];

pub fn allows(action: &Action) -> bool {
    match action {
        Action::Read | Action::Select | Action::Recursive => true,
        Action::Function(name) => {
            let name = name.to_ascii_lowercase();
            !name.starts_with("dolt") || DOLT_READ_FUNCTIONS.contains(&name.as_str())
        }
        Action::Pragma(name) => READ_PRAGMAS.iter().any(|p| p.eq_ignore_ascii_case(name)),
        Action::Other(_) => false,
    }
}

thread_local! {
    /// What the authorizer last refused on this thread, for the message.
    /// [`check`] prepares on its own thread, so it reads its own refusal.
    static REFUSED: RefCell<Option<String>> = const { RefCell::new(None) };
}

unsafe fn text<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        ""
    } else {
        // Safety: SQLite hands the authorizer NUL-terminated strings that
        // live for the length of the call.
        unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
    }
}

unsafe extern "C" fn authorize(
    _arg: *mut c_void,
    code: c_int,
    first: *const c_char,
    second: *const c_char,
    _database: *const c_char,
    _trigger: *const c_char,
) -> c_int {
    // Safety: see `text`.
    let action = Action::of(code, unsafe { text(first) }, unsafe { text(second) });
    if allows(&action) {
        return SQLITE_OK;
    }
    REFUSED.with(|r| *r.borrow_mut() = Some(action.described()));
    SQLITE_DENY
}

/// The deadline travels to the progress handler as its argument, in
/// milliseconds since the first deadline this process set: a number, so
/// there is no allocation for a dropped connection to leave dangling.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn millis_since_epoch(t: Instant) -> usize {
    usize::try_from(t.saturating_duration_since(epoch()).as_millis()).unwrap_or(usize::MAX)
}

unsafe extern "C" fn past_deadline(deadline: *mut c_void) -> c_int {
    c_int::from(millis_since_epoch(Instant::now()) >= deadline as usize)
}

/// Install the authorizer and the deadline on `conn`, and allow it no
/// attached database. Every statement prepared on it afterwards is held to
/// [`allows`]; one still running at `deadline` is interrupted.
pub async fn guard(conn: &mut SqliteConnection, deadline: Instant) -> Result<(), sqlx::Error> {
    // Rounded up, so the interrupt never comes before `deadline` itself.
    let deadline = millis_since_epoch(deadline).saturating_add(1);
    let mut handle = conn.lock_handle().await?;
    let db = handle.as_raw_handle().as_ptr() as *mut c_void;
    // Safety: `db` is the live connection sqlx just handed us, and the
    // callbacks match SQLite's signatures. Neither argument is a pointer
    // anything dereferences.
    let rc = unsafe {
        sqlite3_limit(db, SQLITE_LIMIT_ATTACHED, 0);
        sqlite3_progress_handler(
            db,
            STEPS_PER_CHECK,
            Some(past_deadline),
            deadline as *mut c_void,
        );
        sqlite3_set_authorizer(db, Some(authorize), std::ptr::null_mut())
    };
    if rc != SQLITE_OK {
        return Err(sqlx::Error::Configuration(
            format!("install the read-only authorizer: sqlite rc {rc}").into(),
        ));
    }
    Ok(())
}

/// What a statement that passed [`check`] reads out and takes in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub columns: Vec<String>,
    /// Each parameter's name by its index, from 1: `:x`, `@x`, `$x`, `?3`,
    /// or `None` for a bare `?`.
    pub parameters: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The authorizer said no; what it refused.
    Refused(String),
    /// More than one statement, or none.
    NotOneStatement,
    /// SQLite could not prepare it: a syntax error, a missing table.
    Sql(String),
}

/// Prepare `sql` on a guarded connection without running it: whether it is
/// one statement the authorizer allows, and what it reads and binds.
pub async fn check(conn: &mut SqliteConnection, sql: &str) -> Result<Checked, Refusal> {
    let c_sql = CString::new(sql).map_err(|_| Refusal::Sql("the SQL holds a NUL byte".into()))?;
    let mut handle = conn
        .lock_handle()
        .await
        .map_err(|e| Refusal::Sql(e.to_string()))?;
    let db = handle.as_raw_handle().as_ptr() as *mut c_void;
    REFUSED.with(|r| r.borrow_mut().take());
    let mut stmt: *mut c_void = std::ptr::null_mut();
    let mut tail: *const c_char = std::ptr::null();
    // Safety: `db` is live and locked to us; `c_sql` outlives the call, and
    // `tail` points into it.
    let rc = unsafe { sqlite3_prepare_v2(db, c_sql.as_ptr(), -1, &mut stmt, &mut tail) };
    if rc != SQLITE_OK {
        if let Some(refused) = REFUSED.with(|r| r.borrow_mut().take()) {
            return Err(Refusal::Refused(refused));
        }
        // Safety: SQLite's message for the last call on `db`.
        return Err(Refusal::Sql(
            unsafe { text(sqlite3_errmsg(db)) }.to_string(),
        ));
    }
    if stmt.is_null() {
        return Err(Refusal::NotOneStatement);
    }
    // Safety: `tail` points into `c_sql`, at or before its NUL.
    let rest = unsafe { text(tail) };
    // Safety: `stmt` is the statement just prepared, finalized below.
    let checked = unsafe {
        Checked {
            columns: (0..sqlite3_column_count(stmt))
                .map(|i| text(sqlite3_column_name(stmt, i)).to_string())
                .collect(),
            parameters: (1..=sqlite3_bind_parameter_count(stmt))
                .map(|i| {
                    let p = sqlite3_bind_parameter_name(stmt, i);
                    (!p.is_null()).then(|| text(p).to_string())
                })
                .collect(),
        }
    };
    // Safety: finalized once, here.
    unsafe { sqlite3_finalize(stmt) };
    if !rest.trim().is_empty() {
        return Err(Refusal::NotOneStatement);
    }
    Ok(checked)
}

/// The caller's statement as one page of itself, asking one row past the
/// page so the reader knows whether another follows. A trailing `;` is
/// dropped; anything after the statement fails [`check`].
pub fn paged(sql: &str, offset: u64, limit: usize) -> String {
    let body = sql.trim_end().trim_end_matches(';').trim_end();
    format!(
        "SELECT * FROM (\n{body}\n) LIMIT {} OFFSET {offset}",
        limit + 1
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The functions that reach past the store stay out, whatever their
    /// case; the ones that only answer, and SQLite's own, stay in.
    #[test]
    fn doltlite_functions_are_allowed_by_name_and_sqlites_are_not_judged() {
        for refused in [
            "dolt_creds_new",
            "DOLT_PUSH",
            "dolt_clone",
            "dolt_fetch",
            "dolt_commit",
            "dolt_gc",
            "doltlite_engine",
        ] {
            assert!(!allows(&Action::Function(refused)), "{refused}");
        }
        for allowed in ["dolt_hashof", "Dolt_HashOf_Table", "count", "json_extract"] {
            assert!(allows(&Action::Function(allowed)), "{allowed}");
        }
        assert!(allows(&Action::Pragma("table_info")));
        assert!(!allows(&Action::Pragma("writable_schema")));
        assert!(!allows(&Action::Other(SQLITE_ATTACH)));
        assert!(!allows(&Action::Other(SQLITE_INSERT)));
    }
}
