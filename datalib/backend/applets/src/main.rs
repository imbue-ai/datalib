//! `datalib-applet` — the applet host, one subcommand per applet.
//!
//! The same shape as `datalib-step`: one binary, a nested subcommand
//! naming which thing to run, and a shared flag surface. A config entry
//! says
//!
//! ```toml
//! [[applets]]
//! id = "slack_work"
//! command = "datalib-applet slack"
//! ```
//!
//! and the gateway appends the rest:
//!
//! ```text
//! datalib-applet <name> -p 0 --frontend-dir <dir> [--params <json>]
//! ```
//!
//! One invocation, one process. The applet **writes its frontend
//! directory, then binds, then announces the port it bound** — in that
//! order, because the gateway takes the announcement as its signal
//! that the write finished and the store is safe to scan. An applet
//! that announced first would race the scan and intermittently come up
//! with no components.
//!
//! ## What an applet owes
//!
//! Three things, in order: leave files in the directory it is handed —
//! a `<sha256>.js` and a `<name>.json` per component, described in
//! `docs/dev/applets.md` — bind a port, and print
//! `DATALIB_APPLET_PORT=<port>` to stdout. Then answer HTTP there.
//!
//! The gateway passes `-p 0`, so the port is the OS's choice and the
//! announcement is how the gateway learns it. That direction is
//! load-bearing: a port the gateway picked would have to be released
//! before the applet could bind it, and "something is accepting on
//! that port" cannot tell this applet apart from whoever won the race
//! for it.
//!
//! Everything else on stdout is ignored, and stderr is the log: the
//! gateway forwards it and surfaces its tail when an applet fails to
//! come up.
//!
//! ## Printing
//!
//! The workspace bans `println!`/`eprintln!` because they corrupt a
//! pipeline's indicatif progress display. An applet runs no bars: it is
//! a standalone server whose stderr is its log, captured by the gateway
//! and surfaced in a `502` when it fails to start. Lifted here for the
//! same reason it is lifted in `datalib-http`.
#![allow(clippy::disallowed_macros)]

mod slack;
mod unified_index;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "datalib-applet", about = "Applet host for the datalib app")]
struct Cli {
    #[command(subcommand)]
    applet: Which,
    /// Where to write this instance's frontend namespace, before
    /// serving. The last segment is the namespace name, which is the
    /// only channel by which an applet learns which instance it is —
    /// two instances of one command differ solely in configuration.
    ///
    /// Optional so an applet that contributes no components can be run
    /// without one; the gateway always passes it.
    #[arg(long, global = true)]
    frontend_dir: Option<PathBuf>,
    /// Port to serve on. Loopback only. `0` — what the gateway passes
    /// — means "any": the OS picks, and [`announce_port`] reports back
    /// which one it picked.
    #[arg(short = 'p', long, global = true)]
    port: Option<u16>,
    /// The config entry's `params`, as JSON.
    #[arg(long, global = true)]
    params: Option<String>,
}

#[derive(Subcommand)]
enum Which {
    /// Browse a Slack mirror: channels, then one channel's threads,
    /// then a whole thread.
    Slack,
    /// Serve the grid index and the qmd index: search, columns, the
    /// document list, one document, and the files beside it.
    ///
    /// Spelled with an underscore so the subcommand matches the applet
    /// id and the tree it reads (`<root>/unified_index/`); clap would
    /// otherwise kebab-case it and the three names would disagree.
    #[command(name = "unified_index")]
    UnifiedIndex,
}

/// Tell the gateway which port this applet bound.
///
/// Called *after* the frontend directory is written and the listener is
/// up, because the gateway reads this line as "both of those are done"
/// and scans the store on the strength of it.
///
/// The prefix is `datalib_http::applets::APPLET_PORT_LINE`, spelled
/// literally here so this binary stays free of a dependency on the
/// gateway — the same trade `DATALIB_APPLET_ID` makes. A disagreement
/// is not silent: no applet would ever start.
pub fn announce_port(port: u16) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    // Best effort on both counts: a gateway that has already given up
    // leaves us writing to a closed pipe, and there is nothing useful
    // to do about it that killing the process would not do worse.
    let _ = writeln!(out, "DATALIB_APPLET_PORT={port}");
    let _ = out.flush();
}

fn main() {
    if let Err(e) = run() {
        eprintln!("datalib-applet: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let params: serde_json::Value = match &cli.params {
        Some(json) => serde_json::from_str(json).context("--params is not valid JSON")?,
        None => serde_json::Value::Null,
    };
    // Write, then serve. The order is the contract: the gateway waits
    // for `announce_port` and then scans the store, so announcing
    // early would race the scan.
    match cli.applet {
        Which::Slack => {
            if let Some(dir) = &cli.frontend_dir {
                slack::write_frontend(dir, &params)?;
            }
            let port = cli.port.context("-p <port> is required")?;
            slack::serve(port, &params)
        }
        // Contributes no components, so there is nothing to write
        // first: the app's grid and document views are builtins, and
        // this applet only serves the endpoints behind them.
        Which::UnifiedIndex => {
            let port = cli.port.context("-p <port> is required")?;
            unified_index::serve(port, &params)
        }
    }
}
