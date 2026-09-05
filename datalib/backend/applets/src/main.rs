//! `datalib-applet` — the applet host, one subcommand per applet.
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

fn exit_with_parent() {
    if std::env::var_os("DATALIB_APPLET_PARENT_PIPE").is_none() {
        return;
    }
    std::thread::spawn(|| {
        use std::io::{Read, Write};
        let mut stdin = std::io::stdin().lock();
        let mut scratch = [0u8; 64];
        loop {
            match stdin.read(&mut scratch) {
                // The gateway is gone. Leave the way it would have made
                // us leave.
                Ok(0) => break,
                // Nothing is supposed to arrive, but a byte is not a
                // reason to die.
                Ok(_) => continue,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // Best effort, and it matters which way round: stderr is a
        // pipe to the same gateway that just died, so this write takes
        // EPIPE — and `eprintln!` *panics* on a failed write, which
        // would kill this thread and leave the process running. Which
        // is exactly the leak being fixed, reintroduced one line from
        // the exit that fixes it. `announce_port` above writes
        // best-effort for the same reason.
        let _ = writeln!(
            std::io::stderr(),
            "datalib-applet: parent pipe closed, exiting"
        );
        std::process::exit(0);
    });
}

fn main() {
    exit_with_parent();
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
