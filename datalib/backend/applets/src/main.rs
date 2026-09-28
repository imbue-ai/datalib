//! `datalib-applet` — the applet host, one subcommand per applet.
#![allow(clippy::disallowed_macros)]

mod gate;
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
    #[arg(long, global = true)]
    frontend_dir: Option<PathBuf>,
    /// Port to serve on. Loopback only. `0` — what the gateway passes
    /// — means "any": the OS picks, and [`announce_port`] reports back
    /// which one it picked.
    #[arg(short = 'p', long, global = true)]
    port: Option<u16>,
    /// A JSON file holding the config entry's `params`. A file rather
    /// than an argument because params can carry tokens, and argv is
    /// readable by every user on the machine.
    #[arg(long = "params-file", global = true)]
    params_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Which {
    /// Browse a Slack mirror: channels, then one channel's threads,
    /// then a whole thread.
    Slack,
    /// Serve the grid index and the qmd index: search, columns, the
    /// document list, one document, and the files beside it.
    #[command(name = "unified_index")]
    UnifiedIndex,
}

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
    if let Err(e) = datalib_parent_watch::exit_with_parent(|| {
        datalib_parent_watch::report("datalib-applet: parent gone, exiting");
        std::process::exit(0);
    }) {
        eprintln!("datalib-applet: {e}");
        std::process::exit(2);
    }
    // The unified_index applet runs qmd; from a release tarball that
    // ships no `runtime/`, the first search fetches the manifest's.
    datalib_fetch::enable_runtime_fetch();
    if let Err(e) = run() {
        eprintln!("datalib-applet: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let params: serde_json::Value = match &cli.params_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read the params file {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", path.display()))?
        }
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
            unified_index::serve(port)
        }
    }
}
