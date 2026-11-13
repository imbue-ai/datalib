//! What keeps SQL a caller wrote to reading a store, and runs it: a
//! [`Reader`] opens the store read-only, holds every statement to an
//! authorizer that lets it do only what [`allows`] names, stops one that
//! runs past a deadline, and reads one statement a page at a time. A
//! read-only open is needed too, and not enough on its own: on one, even a
//! detached one, `SELECT dolt_creds_new()` still writes a key under
//! `~/.doltlite/creds`.
//!
//! The statement runs through SQLite's C API rather than sqlx, so the one
//! the authorizer checked is the one that runs, and its parameters bind by
//! name as well as by position (sqlx takes only `?` and `?NNN`). Doltlite
//! is the SQLite API, so the entry points are declared here, as
//! `etl/src/doltlite_raw.rs` does; `libsqlite3-sys` links them, by way of
//! sqlx.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use sqlx as _;

extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut c_void,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close_v2(db: *mut c_void) -> c_int;
    fn sqlite3_busy_timeout(db: *mut c_void, ms: c_int) -> c_int;
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
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
    fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        n_byte: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    fn sqlite3_stmt_readonly(stmt: *mut c_void) -> c_int;
    fn sqlite3_bind_parameter_count(stmt: *mut c_void) -> c_int;
    fn sqlite3_bind_parameter_name(stmt: *mut c_void, i: c_int) -> *const c_char;
    fn sqlite3_bind_null(stmt: *mut c_void, i: c_int) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut c_void, i: c_int, value: i64) -> c_int;
    fn sqlite3_bind_double(stmt: *mut c_void, i: c_int, value: f64) -> c_int;
    // The destructor is `SQLITE_TRANSIENT`, -1 cast to a function pointer:
    // SQLite copies the bytes before the call returns.
    fn sqlite3_bind_text(
        stmt: *mut c_void,
        i: c_int,
        text: *const c_char,
        n: c_int,
        destructor: isize,
    ) -> c_int;
    fn sqlite3_bind_blob(
        stmt: *mut c_void,
        i: c_int,
        blob: *const c_void,
        n: c_int,
        destructor: isize,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_count(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_name(stmt: *mut c_void, i: c_int) -> *const c_char;
    fn sqlite3_column_type(stmt: *mut c_void, i: c_int) -> c_int;
    fn sqlite3_column_int64(stmt: *mut c_void, i: c_int) -> i64;
    fn sqlite3_column_double(stmt: *mut c_void, i: c_int) -> f64;
    fn sqlite3_column_text(stmt: *mut c_void, i: c_int) -> *const u8;
    fn sqlite3_column_blob(stmt: *mut c_void, i: c_int) -> *const c_void;
    fn sqlite3_column_bytes(stmt: *mut c_void, i: c_int) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_DENY: c_int = 1;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READONLY: c_int = 0x1;
const SQLITE_TRANSIENT: isize = -1;
const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_BLOB: c_int = 4;
const SQLITE_NULL: c_int = 5;
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

/// How long a read waits on a plain SQLite file another process is
/// checkpointing, as sqlx's default.
const BUSY_TIMEOUT_MS: c_int = 5000;

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
    /// A statement is prepared on the thread that reads it, so it reads
    /// its own refusal.
    static REFUSED: RefCell<Option<String>> = const { RefCell::new(None) };
}

unsafe fn text<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        ""
    } else {
        // Safety: SQLite hands back NUL-terminated strings that live until
        // the next call on the same statement or connection.
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
/// there is no allocation for a closed connection to leave dangling.
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

/// A value SQLite holds: what a row's cell is, and what a parameter binds.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A statement's parameter values: a list for `?` and `?NNN`, or names for
/// `:x`, `@x` and `$x`, each keyed with or without its sigil.
#[derive(Debug, Clone, PartialEq)]
pub enum Params {
    ByPosition(Vec<Value>),
    ByName(Vec<(String, Value)>),
}

/// Why a statement was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The authorizer said no; what it refused.
    Refused(String),
    /// More than one statement, or none.
    NotOneStatement,
    /// The parameters sent do not fit the statement's.
    Params(String),
    /// SQLite could not prepare or run it: a syntax error, a missing
    /// table, the deadline.
    Sql(String),
}

/// One page of a statement's rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Read {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Whether a row follows the last one here.
    pub more: bool,
}

/// One page to read: rows `offset ..` of the statement, at most `limit` of
/// them, stopping early once their values pass `max_bytes`.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub offset: u64,
    pub limit: usize,
    pub max_bytes: usize,
}

/// A store opened read-only, with the authorizer and the deadline on it
/// and no database attachable. Every call on it blocks; it closes on drop.
pub struct Reader {
    db: *mut c_void,
}

// Safety: a connection may move between threads while one thread at a
// time uses it, which `&mut self` ensures; SQLite is built threadsafe.
unsafe impl Send for Reader {}

impl Drop for Reader {
    fn drop(&mut self) {
        // Safety: `db` came from `sqlite3_open_v2` and is closed once.
        unsafe { sqlite3_close_v2(self.db) };
    }
}

/// A prepared statement, finalized on drop.
struct Statement(*mut c_void);

impl Drop for Statement {
    fn drop(&mut self) {
        // Safety: finalized once; a null statement is a no-op.
        unsafe { sqlite3_finalize(self.0) };
    }
}

impl Reader {
    /// Open the file SQLite names `name`, read-only: a literal path, or
    /// `<path>@<commit>` for a doltlite revision
    /// (`datalib_sqlite_open::literal`, `::revision_name`). A statement
    /// still running at `deadline` is interrupted.
    pub fn open(name: &Path, deadline: Instant) -> Result<Reader, String> {
        let shown = name.display();
        let c_name = name
            .to_str()
            .and_then(|s| CString::new(s).ok())
            .ok_or_else(|| format!("{shown} is not a name SQLite can open"))?;
        let mut db: *mut c_void = std::ptr::null_mut();
        // Safety: `c_name` outlives the call. Without SQLITE_OPEN_URI the
        // name is a file's, never a URI; the callers' names are absolute
        // or start `./`, so none reads as `:memory:`.
        let rc = unsafe {
            sqlite3_open_v2(
                c_name.as_ptr(),
                &mut db,
                SQLITE_OPEN_READONLY,
                std::ptr::null(),
            )
        };
        // Owned from here, so an open that failed is closed too.
        let reader = Reader { db };
        if rc != SQLITE_OK {
            return Err(format!("open {shown}: {}", reader.message()));
        }
        // Rounded up, so the interrupt never comes before `deadline` itself.
        let deadline = millis_since_epoch(deadline).saturating_add(1);
        // Safety: `db` is open and ours; the callbacks match SQLite's
        // signatures, and neither argument is a pointer anything reads.
        let rc = unsafe {
            sqlite3_busy_timeout(db, BUSY_TIMEOUT_MS);
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
            return Err(format!("install the read-only authorizer: sqlite rc {rc}"));
        }
        Ok(reader)
    }

    fn message(&self) -> String {
        // Safety: SQLite's message for the last call on `db`; a null `db`
        // (an open that could not allocate) has one too.
        unsafe { text(sqlite3_errmsg(self.db)) }.to_string()
    }

    /// One page of `sql`'s rows, its parameters bound from `params`. The
    /// statement must be one the authorizer allows, and only one.
    pub fn read(&mut self, sql: &str, params: &Params, window: Window) -> Result<Read, Refusal> {
        let statement = self.prepare(&paged(sql, window.offset, window.limit))?;
        let stmt = statement.0;
        // Safety, for the block: `stmt` is live until `statement` drops.
        let names: Vec<Option<String>> = (1..=unsafe { sqlite3_bind_parameter_count(stmt) })
            .map(|i| {
                let p = unsafe { sqlite3_bind_parameter_name(stmt, i) };
                (!p.is_null()).then(|| unsafe { text(p) }.to_string())
            })
            .collect();
        for (i, value) in (1..).zip(ordered(params, &names).map_err(Refusal::Params)?) {
            self.bind(stmt, i, &value)?;
        }
        let columns: Vec<String> = (0..unsafe { sqlite3_column_count(stmt) })
            .map(|i| unsafe { text(sqlite3_column_name(stmt, i)) }.to_string())
            .collect();
        let mut rows = Vec::new();
        let mut bytes = 0;
        loop {
            match unsafe { sqlite3_step(stmt) } {
                SQLITE_ROW => {}
                SQLITE_DONE => {
                    return Ok(Read {
                        columns,
                        rows,
                        more: false,
                    })
                }
                _ => return Err(Refusal::Sql(self.message())),
            }
            if rows.len() == window.limit || bytes > window.max_bytes {
                return Ok(Read {
                    columns,
                    rows,
                    more: true,
                });
            }
            let row: Vec<Value> = (0..columns.len() as c_int)
                .map(|i| unsafe { cell(stmt, i) })
                .collect();
            bytes += row.iter().map(Value::size).sum::<usize>();
            rows.push(row);
        }
    }

    /// Whether `sql`, as it is, is one statement the authorizer allows;
    /// runs nothing.
    pub fn check(&mut self, sql: &str) -> Result<(), Refusal> {
        self.prepare(sql).map(drop)
    }

    fn prepare(&mut self, sql: &str) -> Result<Statement, Refusal> {
        let c_sql =
            CString::new(sql).map_err(|_| Refusal::Sql("the SQL holds a NUL byte".into()))?;
        REFUSED.with(|r| r.borrow_mut().take());
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let mut tail: *const c_char = std::ptr::null();
        // Safety: `db` is live; `c_sql` outlives the call, and `tail`
        // points into it.
        let rc = unsafe { sqlite3_prepare_v2(self.db, c_sql.as_ptr(), -1, &mut stmt, &mut tail) };
        let statement = Statement(stmt);
        if rc != SQLITE_OK {
            if let Some(refused) = REFUSED.with(|r| r.borrow_mut().take()) {
                return Err(Refusal::Refused(refused));
            }
            return Err(Refusal::Sql(self.message()));
        }
        // Safety: `tail` points into `c_sql`, at or before its NUL.
        if stmt.is_null() || !unsafe { text(tail) }.trim().is_empty() {
            return Err(Refusal::NotOneStatement);
        }
        // A second lock behind the authorizer, which already refused any
        // statement that writes.
        // Safety: `stmt` is the statement just prepared.
        if unsafe { sqlite3_stmt_readonly(stmt) } == 0 {
            return Err(Refusal::Refused("a write".into()));
        }
        Ok(statement)
    }

    fn bind(&self, stmt: *mut c_void, i: c_int, value: &Value) -> Result<(), Refusal> {
        let too_long = |n: usize| {
            c_int::try_from(n).map_err(|_| Refusal::Params(format!("parameter {i} is too long")))
        };
        // Safety: `stmt` is live and `i` one of its parameters; text and
        // blob bytes are copied (SQLITE_TRANSIENT) before the call returns.
        let rc = unsafe {
            match value {
                Value::Null => sqlite3_bind_null(stmt, i),
                Value::Integer(n) => sqlite3_bind_int64(stmt, i, *n),
                Value::Real(f) => sqlite3_bind_double(stmt, i, *f),
                Value::Text(s) => sqlite3_bind_text(
                    stmt,
                    i,
                    s.as_ptr().cast(),
                    too_long(s.len())?,
                    SQLITE_TRANSIENT,
                ),
                Value::Blob(b) => sqlite3_bind_blob(
                    stmt,
                    i,
                    b.as_ptr().cast(),
                    too_long(b.len())?,
                    SQLITE_TRANSIENT,
                ),
            }
        };
        if rc != SQLITE_OK {
            return Err(Refusal::Params(format!(
                "bind parameter {i}: {}",
                self.message()
            )));
        }
        Ok(())
    }
}

/// Column `i` of the row `stmt` is on.
///
/// Safety: `stmt` has just returned SQLITE_ROW and `i` is a column of it.
unsafe fn cell(stmt: *mut c_void, i: c_int) -> Value {
    let bytes = |p: *const u8| {
        let n = usize::try_from(unsafe { sqlite3_column_bytes(stmt, i) }).unwrap_or(0);
        if p.is_null() || n == 0 {
            &[][..]
        } else {
            // Safety: SQLite's buffer of `n` bytes, valid until the next step.
            unsafe { std::slice::from_raw_parts(p, n) }
        }
    };
    unsafe {
        match sqlite3_column_type(stmt, i) {
            SQLITE_NULL => Value::Null,
            SQLITE_INTEGER => Value::Integer(sqlite3_column_int64(stmt, i)),
            SQLITE_FLOAT => Value::Real(sqlite3_column_double(stmt, i)),
            SQLITE_BLOB => Value::Blob(bytes(sqlite3_column_blob(stmt, i).cast()).to_vec()),
            _ => Value::Text(
                String::from_utf8_lossy(bytes(sqlite3_column_text(stmt, i))).into_owned(),
            ),
        }
    }
}

impl Value {
    /// About how many bytes the value holds, for a page's size.
    fn size(&self) -> usize {
        8 + match self {
            Value::Text(s) => s.len(),
            Value::Blob(b) => b.len(),
            Value::Null | Value::Integer(_) | Value::Real(_) => 0,
        }
    }
}

/// The caller's statement as one page of itself, asking one row past the
/// page so the reader knows whether another follows. A trailing `;` is
/// dropped; anything after the statement is refused when it is prepared.
pub fn paged(sql: &str, offset: u64, limit: usize) -> String {
    let body = sql.trim_end().trim_end_matches(';').trim_end();
    format!(
        "SELECT * FROM (\n{body}\n) LIMIT {} OFFSET {offset}",
        limit + 1
    )
}

/// The value of each parameter, in index order: `names[i]` is parameter
/// `i + 1`'s name as SQLite reports it (`:x`, `@x`, `$x`, `?3`), or `None`
/// for a bare `?`. Every parameter needs a value and every value a
/// parameter, so a misspelt name is an error rather than a NULL.
pub fn ordered(params: &Params, names: &[Option<String>]) -> Result<Vec<Value>, String> {
    let is_named = |name: &Option<String>| name.as_deref().is_some_and(|n| !n.starts_with('?'));
    match params {
        Params::ByPosition(values) => {
            if let Some(named) = names
                .iter()
                .find(|n| is_named(n))
                .and_then(Option::as_deref)
            {
                return Err(format!(
                    "{named} is a named parameter: send params as an object, {{\"{}\": …}}",
                    &named[1..]
                ));
            }
            if values.len() != names.len() {
                return Err(format!(
                    "the statement has {} parameter(s) and params holds {}",
                    names.len(),
                    values.len()
                ));
            }
            Ok(values.clone())
        }
        Params::ByName(pairs) => {
            let mut used = vec![false; pairs.len()];
            let mut values = Vec::with_capacity(names.len());
            let mut missing = Vec::new();
            for (i, name) in (1..).zip(names) {
                let Some(name) = name.as_deref().filter(|n| !n.starts_with('?')) else {
                    return Err(format!(
                        "parameter {i} is a `?`, which takes its value by position: \
                         name it (`:x`) or send params as a list"
                    ));
                };
                let bare = &name[1..];
                let found: Vec<usize> = (0..pairs.len())
                    .filter(|&k| pairs[k].0 == name || pairs[k].0 == bare)
                    .collect();
                match found[..] {
                    [k] => {
                        used[k] = true;
                        values.push(pairs[k].1.clone());
                    }
                    [] => missing.push(name.to_string()),
                    _ => return Err(format!("params gives {name} twice, as {bare} and {name}")),
                }
            }
            if !missing.is_empty() {
                return Err(format!("params has no value for {}", missing.join(", ")));
            }
            let unused: Vec<&str> = (0..pairs.len())
                .filter(|&k| !used[k])
                .map(|k| pairs[k].0.as_str())
                .collect();
            if !unused.is_empty() {
                return Err(format!(
                    "the statement has no parameter {}",
                    unused.join(", ")
                ));
            }
            Ok(values)
        }
    }
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

    fn names(names: &[Option<&str>]) -> Vec<Option<String>> {
        names.iter().map(|n| n.map(str::to_string)).collect()
    }

    fn by_name(pairs: &[(&str, i64)]) -> Params {
        Params::ByName(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), Value::Integer(*v)))
                .collect(),
        )
    }

    /// A name binds with or without its sigil, every parameter SQLite
    /// numbers for it, and a name used twice in the statement is one
    /// parameter to SQLite, so one value.
    #[test]
    fn a_named_parameter_takes_its_value_by_name_with_or_without_its_sigil() {
        let stmt = names(&[Some(":who"), Some("@n"), Some("$tag")]);
        assert_eq!(
            ordered(&by_name(&[("n", 2), (":who", 1), ("tag", 3)]), &stmt),
            Ok(vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3)
            ])
        );
        assert_eq!(
            ordered(&Params::ByPosition(vec![Value::Null]), &names(&[None])),
            Ok(vec![Value::Null])
        );
        assert_eq!(
            ordered(
                &Params::ByPosition(vec![Value::Null, Value::Integer(1)]),
                &names(&[None, Some("?2")])
            ),
            Ok(vec![Value::Null, Value::Integer(1)])
        );
    }

    #[test]
    fn a_parameter_without_a_value_or_a_value_without_a_parameter_is_refused() {
        let stmt = names(&[Some(":who"), Some(":n")]);
        let refused = |params: &Params, stmt: &[Option<String>], says: &str| {
            let err = ordered(params, stmt).expect_err(says);
            assert!(err.contains(says), "{err:?} should say {says:?}");
        };
        refused(&by_name(&[("who", 1)]), &stmt, "no value for :n");
        refused(
            &by_name(&[("who", 1), ("n", 2), ("nn", 3)]),
            &stmt,
            "no parameter nn",
        );
        refused(
            &by_name(&[("who", 1), (":who", 1), ("n", 2)]),
            &stmt,
            "twice",
        );
        refused(&by_name(&[("x", 1)]), &names(&[None]), "by position");
        refused(
            &Params::ByPosition(vec![Value::Null, Value::Null]),
            &stmt,
            "send params as an object, {\"who\"",
        );
        refused(
            &Params::ByPosition(vec![]),
            &names(&[None]),
            "1 parameter(s) and params holds 0",
        );
    }
}
