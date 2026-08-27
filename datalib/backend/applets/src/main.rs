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
//! datalib-applet <name> -p <port> --frontend-dir <dir> [--params <json>]
//! ```
//!
//! One invocation, one process. The applet **writes its frontend
//! directory, then binds the port** — in that order, because the
//! gateway takes "the port accepts" as its signal that the write
//! finished and the store is safe to scan. An applet that bound first
//! would race the scan and intermittently come up with no components.
//!
//! ## What an applet owes
//!
//! Two things, in order: leave files in the directory it is handed — a
//! `<sha256>.js` and a `<name>.json` per component, described in
//! `docs/dev/applets.md` — and then answer HTTP on the port. Nothing is
//! read from stdout; stderr is the log, and the gateway surfaces its
//! tail when an applet fails to come up.
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
    /// Port to serve on. Loopback only; the gateway picks it.
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
    // for the port and then scans the store, so binding early would
    // race the scan.
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
