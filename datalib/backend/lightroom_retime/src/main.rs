// A one-shot command a person runs by hand; its whole interface is
// stdout and stderr, the case clippy.toml allows printing for.
#![allow(clippy::disallowed_macros)]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use datalib_lightroom_retime::plan::{self, Outcome};
use datalib_lightroom_retime::{edits, run, Report};

#[derive(Parser)]
#[command(
    name = "datalib-lightroom-retime",
    about = "Apply a JSON Lines file of capture-time edits to a Lightroom Classic catalog. \
             Prints what would change; --write applies it, after a backup, with Lightroom closed."
)]
struct Cli {
    /// The catalog, a `.lrcat` file.
    catalog: PathBuf,
    /// One JSON object per line: id_global, expected, capture_time, why.
    edits: PathBuf,
    /// Write the edits. Without it, nothing is written.
    #[arg(long)]
    write: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let text = std::fs::read_to_string(&cli.edits)
        .with_context(|| format!("reading {}", cli.edits.display()))?;
    let edits = match edits::parse(&text) {
        Ok(e) => e,
        Err(problems) => {
            for p in problems {
                eprintln!("{}: {p}", cli.edits.display());
            }
            return Ok(ExitCode::FAILURE);
        }
    };
    let stamp = chrono::Local::now().format("%Y%m%dT%H%M%S").to_string();
    let report = run(&cli.catalog, edits, cli.write, &stamp).await?;
    print(&report, cli.write);
    Ok(if report.refused() > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print(report: &Report, write: bool) {
    for row in &report.rows {
        let who = row.name.as_deref().unwrap_or(&row.edit.id_global);
        let line = row.edit.line;
        match &row.outcome {
            Outcome::Write(w) => println!(
                "line {line} {who}: {} -> {} ({})",
                row.was.as_deref().unwrap_or("no time"),
                w.capture_time,
                plan::xmp_time(&row.edit.target),
            ),
            Outcome::AlreadyDone => println!("line {line} {who}: already {}", row.edit.target),
            Outcome::Refused(why) => println!("line {line} {who}: refused: {why}"),
        }
    }
    println!(
        "{} to write, {} already done, {} refused.",
        report.to_write(),
        report.already_done(),
        report.refused()
    );
    match &report.written {
        Some(w) => {
            println!("Written. The catalog as it was is {}.", w.backup.display());
            if let Some(to) = &w.cache_moved_to {
                println!(
                    "Moved the Metadata panel's cache to {}; Lightroom builds a new one on its next open.",
                    to.display()
                );
            }
        }
        None if report.refused() > 0 => println!("Nothing written: every edit applies, or none."),
        None if !write => println!("Nothing written; --write applies these."),
        None => println!("Nothing to write."),
    }
}
