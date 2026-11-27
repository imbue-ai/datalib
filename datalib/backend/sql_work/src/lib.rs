//! Counts what every SQLite statement in this process does, under the name
//! it carries (`datalib_sql`): how often it ran, the rows it answered and
//! the rows each of its loops visited, by the table or index the loop read,
//! and its time. It logs the statements that did much work for what they
//! answered: the statement, the file, the shape of its parameters and
//! SQLite's own counters. A line names work, not seconds, so a cold disk, a
//! lock or a busy machine cannot make a good plan look bad or a bad plan
//! look good (docs/dev/logging.md).
//!
//! [`install`] once, before the first connection opens, and every
//! connection this process opens afterwards is counted. [`totals`] is what
//! this process's statements came to, by name; a server asks [`counted`]
//! what one request's statements did; a test can [`record_under`] a folder
//! to read each statement's work back.

mod census;
pub mod judge;
pub mod params;
mod recording;
mod rows_read;
mod totals;

// Links the SQLite (doltlite) the C calls below resolve against.
use sqlx as _;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

pub use judge::Work;
pub use recording::{record_under, Ran, Recording};
pub use rows_read::{
    counted, counted_blocking, header_value, with_count, Counted, ROWS_READ_HEADER, SQL_WORK_HEADER,
};
pub use totals::{merge_into, series, since, totals, ByTag, OneRun, Series, TagWork};

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
    fn sqlite3_stmt_scanstatus_v2(
        stmt: *mut c_void,
        idx: c_int,
        op: c_int,
        flags: c_int,
        out: *mut c_void,
    ) -> c_int;
    fn sqlite3_stmt_scanstatus_reset(stmt: *mut c_void);
}

const SQLITE_TRACE_STMT: c_uint = 0x01;
const SQLITE_TRACE_PROFILE: c_uint = 0x02;
const SQLITE_TRACE_ROW: c_uint = 0x04;
const SQLITE_STMTSTATUS_FULLSCAN_STEP: c_int = 1;
const SQLITE_STMTSTATUS_SORT: c_int = 2;
const SQLITE_STMTSTATUS_AUTOINDEX: c_int = 3;
const SQLITE_STMTSTATUS_VM_STEP: c_int = 4;
const SQLITE_SCANSTAT_NVISIT: c_int = 1;
const SQLITE_SCANSTAT_NAME: c_int = 3;
const SQLITE_SCANSTAT_EXPLAIN: c_int = 4;

/// The table-valued functions a statement is handed a set of keys through.
/// A loop over one visits the keys, not a table.
const SET_FUNCTIONS: [&str; 2] = ["json_each", "json_tree"];

static INSTALLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

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
            return;
        }
        INSTALLED.store(true, Ordering::Relaxed);
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
            SQLITE_TRACE_STMT | SQLITE_TRACE_PROFILE | SQLITE_TRACE_ROW,
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
    /// When each statement running on this thread started its run: SQLite
    /// times a run in whole milliseconds on a mac, which most statements
    /// take less than.
    static STARTED: RefCell<HashMap<usize, std::time::Instant>> = RefCell::new(HashMap::new());
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
        // A run starts with no rows. Doltlite runs statements of its own
        // whose rows SQLite reports and whose end it does not, and the next
        // statement made at the same address would take their count. A
        // trigger starting inside a run says so with a `--` comment, and
        // the run's rows go on.
        // Safety: for a start SQLite passes the statement's text.
        SQLITE_TRACE_STMT if !unsafe { borrowed(x as *const c_char) }.starts_with("--") => {
            ROWS.with(|rows| rows.borrow_mut().remove(&(p as usize)));
            STARTED.with(|s| s.borrow_mut().insert(p as usize, std::time::Instant::now()));
        }
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

/// One run of `stmt` has ended: read and reset its counters, add its work
/// to this process's totals and to the count it ran inside, log it if it
/// was heavy, and record it in the census if one was asked for. A statement
/// run while a line is being written is the log's own, and counts nowhere.
unsafe fn finished(stmt: *mut c_void, nanos: i64) {
    if WRITING.with(Cell::get) {
        return;
    }
    // Safety, for every call below: `stmt` is the statement SQLite has
    // just finished running, alive until this callback returns, and so is
    // every string it hands back.
    let status = |op| unsafe { sqlite3_stmt_status(stmt, op, 1) }.max(0) as u64;
    let returned = ROWS
        .with(|rows| rows.borrow_mut().remove(&(stmt as usize)))
        .unwrap_or(0);
    let nanos = STARTED
        .with(|s| s.borrow_mut().remove(&(stmt as usize)))
        .map_or(nanos.max(0) as u64, |at| at.elapsed().as_nanos() as u64);
    let sql = unsafe { borrowed(sqlite3_sql(stmt)) };
    let db = unsafe { sqlite3_db_handle(stmt) };
    let (loops, inputs) = unsafe { loops_of(stmt) };
    let mut work = Work {
        vm_steps: status(SQLITE_STMTSTATUS_VM_STEP),
        fullscan_steps: status(SQLITE_STMTSTATUS_FULLSCAN_STEP),
        autoindex_rows: status(SQLITE_STMTSTATUS_AUTOINDEX),
        sorts: status(SQLITE_STMTSTATUS_SORT),
        rows: returned,
        inputs,
        nanos,
    };
    // Doltlite marks some reads as writes, and the connection's count of
    // changes is then its last write's: a read counts the rows it returned.
    if unsafe { sqlite3_stmt_readonly(stmt) } == 0 && judge::changes_rows(&sql) {
        work.rows = work
            .rows
            .max(unsafe { sqlite3_changes64(db) }.max(0) as u64);
    }
    let name = datalib_sql::tag::filed_under(&sql);
    let run = OneRun {
        rows: work.rows,
        vm_steps: work.vm_steps,
        nanos: work.nanos,
        loops: &loops,
    };
    totals::note(&name, &run);
    rows_read::note(returned, &name, &run);
    let census = census::dir();
    let recording = recording::any();
    if !(work.is_heavy() || census.is_some() || recording) || !judge::reads_or_writes_rows(&sql) {
        return;
    }
    let file = text(unsafe { sqlite3_db_filename(db, c"main".as_ptr()) });
    if let Some(dir) = census {
        census::note(dir, &sql, &file, work.fullscan_steps > 0);
    }
    if recording {
        recording::note(&sql, &file, work);
    }
    if !work.is_heavy() || judge::walks_by_design(&sql) {
        return;
    }
    let expanded = unsafe { sqlite3_expanded_sql(stmt) };
    let expanded_text = text(expanded);
    unsafe { sqlite3_free(expanded as *mut c_void) };
    WRITING.with(|w| w.set(true));
    say(&work, &sql, &file, &expanded_text, &loops);
    WRITING.with(|w| w.set(false));
}

/// Each loop of the run that just ended, as SQLite explains it (`SCAN t`,
/// `SEARCH t USING INDEX t_by_x (x=?)`), with the rows it visited, and the
/// keys its set loops visited ([`SET_FUNCTIONS`]); then the counters start
/// again for the statement's next run. A set loop is known by the table it
/// reads, not by its explanation, which names an alias where one is given.
///
/// # Safety
/// `stmt` is a statement SQLite has just finished running, and the strings
/// borrowed from it are used before the callback returns.
unsafe fn loops_of<'s>(stmt: *mut c_void) -> (Vec<(&'s str, u64)>, u64) {
    let mut loops = Vec::new();
    let mut inputs = 0;
    for idx in 0.. {
        let mut visited: i64 = 0;
        let mut explain: *const c_char = std::ptr::null();
        // Safety: each out pointer is the type the op writes.
        let rc = unsafe {
            sqlite3_stmt_scanstatus_v2(
                stmt,
                idx,
                SQLITE_SCANSTAT_NVISIT,
                0,
                (&mut visited as *mut i64).cast(),
            )
        };
        if rc != 0 {
            break;
        }
        unsafe {
            sqlite3_stmt_scanstatus_v2(
                stmt,
                idx,
                SQLITE_SCANSTAT_EXPLAIN,
                0,
                (&mut explain as *mut *const c_char).cast(),
            )
        };
        if explain.is_null() {
            continue;
        }
        let mut table: *const c_char = std::ptr::null();
        unsafe {
            sqlite3_stmt_scanstatus_v2(
                stmt,
                idx,
                SQLITE_SCANSTAT_NAME,
                0,
                (&mut table as *mut *const c_char).cast(),
            )
        };
        let visited = visited.max(0) as u64;
        // Safety, for both: SQLite's own NUL-terminated text (or null, for
        // the table of a loop that reads none), alive with `stmt`.
        if SET_FUNCTIONS.contains(&unsafe { borrowed(table) }.as_ref()) {
            inputs += visited;
        }
        let explain = unsafe { CStr::from_ptr(explain) }.to_str().unwrap_or("?");
        loops.push((explain, visited));
    }
    unsafe { sqlite3_stmt_scanstatus_reset(stmt) };
    (loops, inputs)
}

/// A C string SQLite owns, borrowed when it is UTF-8; empty for a null
/// pointer.
///
/// # Safety
/// `p` is null or NUL-terminated, and alive while the result is.
unsafe fn borrowed<'s>(p: *const c_char) -> std::borrow::Cow<'s, str> {
    if p.is_null() {
        return "".into();
    }
    // Safety: as the caller promises.
    unsafe { CStr::from_ptr(p) }.to_string_lossy()
}

fn say(work: &Work, sql: &str, file: &str, expanded: &str, loops: &[(&str, u64)]) {
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
    let loops = loops_text(loops);
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
        inputs = work.inputs,
        loops = loops.as_str(),
        elapsed_secs = work.nanos as f64 / 1e9,
        "{msg}"
    );
}

/// Each loop with the rows it visited, in SQLite's order: which one did
/// the walking.
fn loops_text(loops: &[(&str, u64)]) -> String {
    loops
        .iter()
        .map(|(explain, visited)| format!("{explain}: {visited}"))
        .collect::<Vec<_>>()
        .join("; ")
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
