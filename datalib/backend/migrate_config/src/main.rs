// Standalone one-shot CLI: the user runs it directly and its whole
// interface is stdout/stderr. It never runs under the sync pipeline, so
// there are no indicatif bars to corrupt — the case clippy.toml names as
// a legitimate exception to the workspace-wide macro ban.
#![allow(clippy::disallowed_macros)]

//! `datalib-migrate-config` — rewrite a `config.toml` from a shape the
//! runner no longer accepts into the one it does.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

const USAGE: &str = "\
usage: datalib-migrate-config <data-root|config.toml> [-o OUT] [--stdout] [--force]

  <data-root|config.toml>  A data root (its config.toml is used) or the
                           config file itself.
  -o, --output OUT         Write here instead of <input dir>/config.toml.
  --stdout                 Print the converted config; write nothing.
  --force                  Overwrite the output file if it exists. Rewriting
                           a config in place keeps the original beside it as
                           config.toml.orig.
  -h, --help               Show this message.";

fn main() -> Result<()> {
    if let Err(e) = run() {
        // A migration failure is a dead end for the user, not a panic:
        // report it plainly on stderr and exit non-zero.
        eprintln!("datalib-migrate-config: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

fn run() -> Result<()> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut to_stdout = false;
    let mut force = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-o" | "--output" => {
                output = Some(PathBuf::from(args.next().context("-o needs a value")?))
            }
            "--stdout" => to_stdout = true,
            "--force" => force = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if input.is_none() => input = Some(PathBuf::from(a)),
            other => bail!("unexpected argument {other:?}\n\n{USAGE}"),
        }
    }
    let Some(arg) = input else {
        bail!("no input given\n\n{USAGE}");
    };
    if to_stdout && output.is_some() {
        bail!("--stdout and -o are mutually exclusive");
    }

    let input = datalib_migrate_config::resolve_input(&arg);
    if !input.exists() {
        // The data-root case is the one worth explaining: the user
        // pointed at a directory and we looked inside it.
        if arg.is_dir() {
            bail!("no config at {} — nothing to migrate", input.display());
        }
        bail!("{} does not exist", input.display());
    }
    let text =
        std::fs::read_to_string(&input).with_context(|| format!("read {}", input.display()))?;

    let converted = datalib_migrate_config::convert(&text)
        .with_context(|| format!("migrate {}", input.display()))?;

    if to_stdout {
        print!("{converted}");
        return Ok(());
    }

    let out = output.unwrap_or_else(|| datalib_migrate_config::default_output(&input));
    if out.exists() && !force {
        bail!(
            "{} already exists — pass --force to overwrite, or --stdout to \
             print the conversion instead",
            out.display()
        );
    }
    let in_place = same_file(&input, &out);
    if in_place {
        let orig = out.with_extension("toml.orig");
        std::fs::copy(&input, &orig)
            .with_context(|| format!("keep the original as {}", orig.display()))?;
        eprintln!("kept the original as {}", orig.display());
    }
    std::fs::write(&out, &converted).with_context(|| format!("write {}", out.display()))?;

    eprintln!("migrated {} -> {}", input.display(), out.display());
    if !in_place {
        eprintln!(
            "Review it, then remove {} once you're happy.",
            input.display()
        );
    }
    Ok(())
}

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}
