// A one-shot command a person or an agent runs by hand; its whole
// interface is stdout and stderr, the case clippy.toml allows printing for.
#![allow(clippy::disallowed_macros)]

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use datalib_cli_help::Example;
use datalib_lightroom_retime::plan::{self, Outcome};
use datalib_lightroom_retime::{edits, run, Report, Row};
use serde_json::json;

const EXAMPLES: &[Example] = &[
    Example {
        what: "print what the edits would change, writing nothing",
        command: "datalib-lightroom-retime ~/Pictures/Lightroom/Catalog.lrcat edits.jsonl",
    },
    Example {
        what: "apply them, with Lightroom closed",
        command: "datalib-lightroom-retime ~/Pictures/Lightroom/Catalog.lrcat edits.jsonl --write",
    },
    Example {
        what: "read the edits from standard input and report one JSON object per line",
        command: "datalib-lightroom-retime Catalog.lrcat - --json",
    },
];

const DETAILS: &str = "\
The edits file is JSON Lines, one photo per line:
  {\"id_global\":\"FFBB4B7B-…\",\"expected\":\"2024-07-01T12:00:00.000\",\
\"capture_time\":\"2024-07-01T14:00:00.000+02:00\",\"why\":\"camera on UTC\"}
  id_global     the photo's Adobe_images.id_global
  expected      its captureTime exactly as read when the edit was made
  capture_time  the local clock time with its offset, to the millisecond at most
  why           optional: what gave this time

For each photo it writes captureTime (the sort), the XMP's
exif:DateTimeOriginal and photoshop:DateCreated (what the Metadata panel
shows) and the day columns (the Library filter's Date). It never touches
the photo files.

--write refuses a catalog Lightroom has open, and writes every line or
none: a line is refused when the photo's captureTime is no longer
`expected`. It first copies the catalog to <catalog>.bak_timestamps_<time>,
and afterwards moves `<catalog> Helper.lrdata`, the Metadata panel's cache,
aside under the same suffix; Lightroom builds a new one. A photo that
already has its target is counted as done, so running it again is safe.

Exit status: 0 when nothing was refused; 1 when a line was refused (and
nothing written), the edits file did not parse, or the catalog could not
be read or written; 2 for a command line clap could not parse.";

#[derive(Parser)]
#[command(
    name = "datalib-lightroom-retime",
    version,
    about = "Apply a JSON Lines file of capture-time edits to a Lightroom Classic catalog.",
    long_about = "Apply a JSON Lines file of capture-time edits to a Lightroom Classic catalog. \
                  Prints what would change; --write applies it, after a backup, with Lightroom closed.",
    after_long_help = after_long_help(),
)]
struct Cli {
    /// The catalog, a `.lrcat` file. Quit Lightroom before --write.
    #[arg(value_name = "CATALOG.lrcat")]
    catalog: PathBuf,
    /// The edits, JSON Lines; `-` reads them from standard input.
    #[arg(value_name = "EDITS.jsonl")]
    edits: PathBuf,
    /// Write the edits. Without it, nothing is written.
    #[arg(long)]
    write: bool,
    /// Report one JSON object per edit, then one for the run, instead of text.
    #[arg(long)]
    json: bool,
}

fn after_long_help() -> String {
    let mut examples = String::from("Examples:\n");
    for e in EXAMPLES {
        examples.push_str(&format!("  # {}\n  {}\n\n", e.what, e.command));
    }
    format!("{DETAILS}\n\n{}", examples.trim_end())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let text = read_edits(&cli.edits)?;
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
    if cli.json {
        print_json(&report);
    } else {
        print_text(&report, cli.write);
    }
    Ok(if report.refused() > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn read_edits(path: &PathBuf) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading the edits from standard input")?;
        return Ok(text);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn print_text(report: &Report, write: bool) {
    for row in &report.rows {
        let who = row.name.as_deref().unwrap_or(&row.edit.id_global);
        let line = row.edit.line;
        let target = plan::xmp_time(&row.edit.target);
        match &row.outcome {
            Outcome::Write(w) => println!(
                "line {line} {who}: {} -> {} ({target})",
                row.was.as_deref().unwrap_or("no time"),
                w.capture_time,
            ),
            Outcome::AlreadyDone => println!("line {line} {who}: already {target}"),
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

fn print_json(report: &Report) {
    for row in &report.rows {
        println!("{}", row_json(row));
    }
    let written = report.written.as_ref();
    println!(
        "{}",
        json!({
            "to_write": report.to_write(),
            "already_done": report.already_done(),
            "refused": report.refused(),
            "written": written.is_some(),
            "backup": written.map(|w| w.backup.display().to_string()),
            "cache_moved_to": written
                .and_then(|w| w.cache_moved_to.as_ref())
                .map(|p| p.display().to_string()),
        })
    );
}

fn row_json(row: &Row) -> serde_json::Value {
    let (status, reason) = match &row.outcome {
        Outcome::Write(_) => ("write", None),
        Outcome::AlreadyDone => ("already_done", None),
        Outcome::Refused(why) => ("refused", Some(why.as_str())),
    };
    json!({
        "line": row.edit.line,
        "id_global": row.edit.id_global,
        "name": row.name,
        "status": status,
        "was": row.was,
        "capture_time": plan::xmp_time(&row.edit.target),
        "reason": reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed_and_its_examples_parse() {
        Cli::command().debug_assert();
        datalib_cli_help::assert_examples_parse(Cli::command(), EXAMPLES);
    }
}
