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
//! and the gateway appends the mode flags. There are two modes, and an
//! applet must implement both:
//!
//! ```text
//! datalib-applet <name> --write-frontend-dir <dir> [--params <json>]   # write, then exit
//! datalib-applet <name> -p <port> [--params <json>]                    # bind, then serve
//! ```
//!
//! One binary rather than one per applet for the same reason
//! `datalib-step` is one binary: the shared machinery (arg surface,
//! the two-mode contract, the sidecar reading most applets will want)
//! lives in one place, and packaging ships one file instead of a
//! growing list. Adding an applet is a variant here plus a module, not
//! a new crate, a new BUILD target, and five packaging edits.
//!
//! ## What an applet owes
//!
//! Nothing beyond the two modes. Write mode leaves files in the
//! directory it is handed — a `<sha256>.js` and a `<name>.json` per
//! component, described in `docs/dev/applets.md` — and serve mode
//! answers HTTP on the port. Nothing is read from stdout; stderr is the
//! log, and its tail becomes the error message on a non-zero exit.
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

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "datalib-applet", about = "Applet host for the datalib app")]
struct Cli {
    #[command(subcommand)]
    applet: Which,
    /// Where to write this instance's frontend namespace. The last
    /// segment is the namespace name, which is the only channel by
    /// which an applet learns which instance it is — two instances of
    /// one command differ solely in configuration.
    #[arg(long, global = true)]
    write_frontend_dir: Option<PathBuf>,
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
    match cli.applet {
        Which::Slack => {
            if let Some(dir) = &cli.write_frontend_dir {
                return slack::write_frontend(dir, &params);
            }
            match cli.port {
                Some(p) => slack::serve(p, &params),
                None => anyhow::bail!("expected --write-frontend-dir <dir> or -p <port>"),
            }
        }
    }
}
