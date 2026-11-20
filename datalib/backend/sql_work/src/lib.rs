//! Counts what every SQLite statement in this process does, and logs the
//! ones that did much work for what they answered: the statement, the file,
//! the shape of its parameters and SQLite's own counters. A line names work,
//! not seconds, so a cold disk, a lock or a busy machine cannot make a good
//! plan look bad or a bad plan look good (docs/dev/logging.md).
//!
//! [`install`] once, before the first connection opens, and every
//! connection this process opens afterwards is counted. A test can
//! [`record_under`] a folder to read each statement's work back.

mod census;
pub mod judge;
pub mod params;
mod recording;

// Links the SQLite (doltlite) the C calls below resolve against.
use sqlx as _;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::Once;

pub use judge::Work;
pub use recording::{record_under, Ran, Recording};

/// The target of every line this writes, which the runner sums per attempt.
pub const TARGET: &str = "datalib::sql_work";

/// Set to anything but empty to log each heavy statement with its values
/// in place, for a debugging session on one's own library. Values can be a
/// person's words, addresses or ids, so the default is their shapes.
pub const VALUES_ENV: &str = datalib_env::DATALIB_SQL_VALUES;

/// Names a folder to record, in one file per process, every statement that
/// read or wrote rows and whether it walked a whole table or index: the
/// fixture sync's census, which `tests/fixtures/sql_scans_allowed.toml` is
/// checked against.
pub const CENSUS_ENV: &str = datalib_env::DATALIB_SQL_CENSUS;

/// The longest statement text a line carries.
const SQL_CHARS: usize = 4_000;

// Doltlite is the SQLite API, linked through sqlx's libsqlite3-sys; these
// are declared here, as `etl/src/doltlite_raw.rs` does its own.
extern "C" {
    fn sqlite3_auto_extension(entry: *const c_void) -> c_int;
    fn sqlite3_trace_v2(
        db: *mut c_void,
        mask: c_uint,
        callback: Option<
            unsafe extern "C" fn(c_uint, *mut c_void, *mut c_void, *mut c_void) -> c_int,
        >,
        ctx: *mut c_void,
    ) -> c_int;
    fn sqlite3_stmt_status(stmt: *mut c_void, op: c_int, reset: c_int) -> c_int;
    fn sqlite3_sql(stmt: *mut c_void) -> *const c_char;
    fn sqlite3_expanded_sql(stmt: *mut c_void) -> *mut c_char;
    fn sqlite3_free(p: *mut c_void);
    fn sqlite3_db_handle(stmt: *mut c_void) -> *mut c_void;
    fn sqlite3_db_filename(db: *mut c_void, name: *const c_char) -> *const c_char;
    fn sqlite3_stmt_readonly(stmt: *mut c_void) -> c_int;
    fn sqlite3_changes64(db: *mut c_void) -> i64;
}

const SQLITE_TRACE_PROFILE: c_uint = 0x02;
const SQLITE_TRACE_ROW: c_uint = 0x04;
const SQLITE_STMTSTATUS_FULLSCAN_STEP: c_int = 1;
const SQLITE_STMTSTATUS_SORT: c_int = 2;
const SQLITE_STMTSTATUS_AUTOINDEX: c_int = 3;
const SQLITE_STMTSTATUS_VM_STEP: c_int = 4;

/// Counts every statement of every connection this process opens from now
/// on. Calling it again does nothing.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Safety: `on_open` has the signature SQLite calls an automatic
        // extension with; SQLite keeps the pointer for the process's life.
        let rc = unsafe { sqlite3_auto_extension(on_open as *const c_void) };
        if rc != 0 {
            tracing::error!(target: TARGET, rc, "could not count SQLite statements");
        }
    });
}

unsafe extern "C" fn on_open(
    db: *mut c_void,
    _err: *mut *mut c_char,
    _api: *const c_void,
) -> c_int {
    // Safety: `db` is the connection SQLite is opening, and `on_trace`
    // matches `sqlite3_trace_v2`'s callback.
    unsafe {
        sqlite3_trace_v2(
            db,
            SQLITE_TRACE_PROFILE | SQLITE_TRACE_ROW,
            Some(on_trace),
            std::ptr::null_mut(),
        )
    };
    0
}

thread_local! {
    /// Rows returned so far by each statement running on this thread. A
    /// connection runs on one thread at a time, and a statement's rows and
    /// its end are reported on the thread that steps it.
    static ROWS: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    /// Set while a line is being written, so a statement the log itself
    /// runs is not judged inside the judging of another.
    static WRITING: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" fn on_trace(
    kind: c_uint,
    _ctx: *mut c_void,
    p: *mut c_void,
    x: *mut c_void,
) -> c_int {
    // A panic must not cross into SQLite's C frames.
    let r = std::panic::catch_unwind(|| match kind {
        SQLITE_TRACE_ROW => {
            ROWS.with(|rows| *rows.borrow_mut().entry(p as usize).or_default() += 1)
        }
        // Safety: for a profile event SQLite passes the statement and a
        // pointer to its run time in nanoseconds.
        SQLITE_TRACE_PROFILE => unsafe { finished(p, *(x as *const i64)) },
        _ => {}
    });
    if r.is_err() {
        // Our bug, not the statement's: say so, and leave SQLite be.
        tracing::error!(target: TARGET, "the SQLite statement counter panicked");
    }
    0
}

/// One run of `stmt` has ended: read and reset its counters, log it if it
/// was heavy, and record it in the census if one was asked for.
unsafe fn finished(stmt: *mut c_void, nanos: i64) {
    // Safety, for every call below: `stmt` is the statement SQLite has
    // just finished running, alive until this callback returns.
    let status = |op| unsafe { sqlite3_stmt_status(stmt, op, 1) }.max(0) as u64;
    let returned = ROWS
        .with(|rows| rows.borrow_mut().remove(&(stmt as usize)))
        .unwrap_or(0);
    let mut work = Work {
        vm_steps: status(SQLITE_STMTSTATUS_VM_STEP),
        fullscan_steps: status(SQLITE_STMTSTATUS_FULLSCAN_STEP),
        autoindex_rows: status(SQLITE_STMTSTATUS_AUTOINDEX),
        sorts: status(SQLITE_STMTSTATUS_SORT),
        rows: returned,
        nanos: nanos.max(0) as u64,
    };
    let census = census::dir();
    let recording = recording::any();
    if !(work.is_heavy() || census.is_some() || recording) || WRITING.with(Cell::get) {
        return;
    }
    let sql = text(unsafe { sqlite3_sql(stmt) });
    if !judge::reads_or_writes_rows(&sql) {
        return;
    }
    let db = unsafe { sqlite3_db_handle(stmt) };
    let file = text(unsafe { sqlite3_db_filename(db, c"main".as_ptr()) });
    if let Some(dir) = census {
        census::note(dir, &sql, &file, work.fullscan_steps > 0);
    }
    if unsafe { sqlite3_stmt_readonly(stmt) } == 0 {
        work.rows = work
            .rows
            .max(unsafe { sqlite3_changes64(db) }.max(0) as u64);
    }
    if recording {
        recording::note(&sql, &file, work);
    }
    if !work.is_heavy() {
        return;
    }
    let expanded = unsafe { sqlite3_expanded_sql(stmt) };
    let expanded_text = text(expanded);
    unsafe { sqlite3_free(expanded as *mut c_void) };
    WRITING.with(|w| w.set(true));
    say(&work, &sql, &file, &expanded_text);
    WRITING.with(|w| w.set(false));
}

fn say(work: &Work, sql: &str, file: &str, expanded: &str) {
    let values = std::env::var_os(VALUES_ENV).is_some_and(|v| !v.is_empty());
    let shown = if values { expanded } else { sql };
    let shown: String = shown.split_whitespace().collect::<Vec<_>>().join(" ");
    let shown: String = shown.chars().take(SQL_CHARS).collect();
    let params = match params::shapes(sql, expanded) {
        Some(shapes) => shapes.join(", "),
        None => "(unread)".to_string(),
    };
    let msg = format!(
        "heavy statement: {} steps for {} rows",
        work.vm_steps, work.rows
    );
    tracing::warn!(
        target: TARGET,
        statement = shown.as_str(),
        file,
        params = params.as_str(),
        vm_steps = work.vm_steps,
        fullscan_steps = work.fullscan_steps,
        autoindex_rows = work.autoindex_rows,
        sorts = work.sorts,
        rows = work.rows,
        elapsed_secs = work.nanos as f64 / 1e9,
        "{msg}"
    );
}

/// A C string SQLite owns, copied; empty for a null pointer.
fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // Safety: SQLite hands back a NUL-terminated string, alive until the
    // statement or connection that owns it changes, after this copy.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
