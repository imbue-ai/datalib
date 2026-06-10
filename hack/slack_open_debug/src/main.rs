//! Slack-doltlite open-time A/B between:
//!   1. raw `sqlite3_open_v2` against our Bazel-built doltlite static archive
//!      (no libsqlite3-sys, no sqlx, just `extern "C"` FFI)
//!   2. the same archive via sqlx's pool open path
//!
//! Goal: definitively attribute the ~60s `frankweiler_etl::doltlite_raw::open`
//! cost on the production slack.doltlite_db. If (1) is fast and (2) is slow,
//! the slowness is in sqlx (worker-thread setup / pragmas / pool acquire). If
//! (1) is also slow, the slowness is in the C library + our compile flags.
//!
//! Run:
//!
//!   bazelisk run //hack/slack_open_debug -- /path/to/slack.doltlite_db
//!
//! Optional: `--iters N` to repeat each measurement, `--sqlx-only` /
//! `--raw-only` to isolate one side.

// Standalone debug A/B benchmark. Doesn't run under frankweiler-sync,
// has no indicatif progress bars to corrupt, and emits its
// measurements directly to stderr. See the sibling allow in
// hack/sqlx_doltlite_loadtest/src/main.rs.
#![allow(clippy::disallowed_macros)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::PathBuf;
use std::ptr;
use std::str::FromStr;
use std::time::{Duration, Instant};

use clap::Parser;

// Minimal FFI for the C symbols we need. The whole point of this binary
// is to avoid pulling in libsqlite3-sys, so declare them directly. These
// match the doltlite (== sqlite3) ABI and the static archive is linked
// in via the Bazel cc_library dep on `//third-party/doltlite:sqlite3`.
#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

const SQLITE_OK: c_int = 0;
const SQLITE_OPEN_READWRITE: c_int = 0x00000002;
const SQLITE_OPEN_READONLY: c_int = 0x00000001;
const SQLITE_OPEN_NOMUTEX: c_int = 0x00008000;

extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        ppdb: *mut *mut sqlite3,
        flags: c_int,
        zvfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_libversion() -> *const c_char;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        callback: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
}

#[derive(Parser, Debug)]
#[command(
    name = "slack_open_debug",
    about = "Time raw sqlite3_open_v2 vs sqlx pool open on a doltlite file."
)]
struct Args {
    /// Path to the .doltlite_db file to probe.
    db_path: PathBuf,

    /// Repeat each measurement this many times. Useful for spotting
    /// page-cache warm-up effects and timing variance.
    #[arg(long, default_value_t = 1)]
    iters: u32,

    /// Skip the sqlx open. When investigating just the C side.
    #[arg(long)]
    raw_only: bool,

    /// Skip the raw FFI open. When you just want sqlx timing.
    #[arg(long)]
    sqlx_only: bool,

    /// Open read-only on the raw side. Removes the `create_if_missing`
    /// equivalent path. Default is SQLITE_OPEN_READWRITE to match
    /// `doltlite_raw::open`.
    #[arg(long)]
    readonly: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let version = unsafe {
        let p = sqlite3_libversion();
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    };
    eprintln!("linked sqlite3_libversion: {version}");
    eprintln!("db path: {}", args.db_path.display());
    let meta = std::fs::metadata(&args.db_path)?;
    eprintln!("file size: {} bytes ({:.1} MB)", meta.len(), meta.len() as f64 / 1e6);

    if !args.sqlx_only {
        eprintln!("\n--- raw sqlite3_open_v2 ---");
        for i in 0..args.iters {
            let elapsed = raw_open_close(&args.db_path, args.readonly)?;
            eprintln!("iter {}: {:?}", i + 1, elapsed);
        }
    }

    if !args.raw_only {
        eprintln!("\n--- sqlx pool connect_with ---");
        for i in 0..args.iters {
            let elapsed = sqlx_pool_open(&args.db_path).await?;
            eprintln!("iter {}: {:?}", i + 1, elapsed);
        }
    }

    Ok(())
}

fn raw_open_close(path: &std::path::Path, readonly: bool) -> Result<Duration, Box<dyn std::error::Error>> {
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())?;
    let flags = if readonly {
        SQLITE_OPEN_READONLY | SQLITE_OPEN_NOMUTEX
    } else {
        SQLITE_OPEN_READWRITE | SQLITE_OPEN_NOMUTEX
    };
    let t = Instant::now();
    let mut db: *mut sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open_v2(c_path.as_ptr(), &mut db, flags, ptr::null()) };
    if rc != SQLITE_OK {
        let msg = if db.is_null() {
            format!("rc={rc} (db null)")
        } else {
            let s = unsafe { std::ffi::CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy().into_owned() };
            unsafe { sqlite3_close(db) };
            format!("rc={rc}: {s}")
        };
        return Err(format!("sqlite3_open_v2: {msg}").into());
    }
    // Match what sqlx does: send PRAGMA foreign_keys = ON. Keeps the
    // comparison apples-to-apples; the previous trace showed this
    // PRAGMA takes ~1ms, so it doesn't materially change timing.
    let pragma = CString::new("PRAGMA foreign_keys = ON;")?;
    let mut errmsg: *mut c_char = ptr::null_mut();
    let rc = unsafe {
        sqlite3_exec(db, pragma.as_ptr(), None, ptr::null_mut(), &mut errmsg)
    };
    if rc != SQLITE_OK {
        let s = if errmsg.is_null() {
            format!("rc={rc}")
        } else {
            let s = unsafe { std::ffi::CStr::from_ptr(errmsg).to_string_lossy().into_owned() };
            format!("rc={rc}: {s}")
        };
        unsafe { sqlite3_close(db) };
        return Err(format!("PRAGMA foreign_keys: {s}").into());
    }
    let elapsed = t.elapsed();
    let close_rc = unsafe { sqlite3_close(db) };
    if close_rc != SQLITE_OK {
        return Err(format!("sqlite3_close rc={close_rc}").into());
    }
    Ok(elapsed)
}

async fn sqlx_pool_open(path: &std::path::Path) -> Result<Duration, Box<dyn std::error::Error>> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(false);
    let t = Instant::now();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(600))
        .connect_with(opts)
        .await?;
    let elapsed = t.elapsed();
    pool.close().await;
    Ok(elapsed)
}
