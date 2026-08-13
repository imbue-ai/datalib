//! `datalib-migrate-config` — convert a pre-TOML `config.yaml` into the
//! `config.toml` the pipeline reads today.
//!
//! ```sh
//! datalib-migrate-config ~/datalib          # writes ~/datalib/config.toml
//! datalib-migrate-config old.yaml -o new.toml
//! datalib-migrate-config ~/datalib --stdout # print, write nothing
//! ```
//!
//! Both legacy formats are handled and told apart by their content: the
//! steps schema written in YAML, and the retired stanza-based `sources:`
//! one. The output is a reviewable draft — comments from the old file
//! are not carried over — but it is re-parsed and graph-checked before
//! being written, so what lands is something the runner accepts.
//!
//! The old file is never touched or deleted, and an existing
//! `config.toml` is never clobbered without `--force`. Recovering from a
//! bad conversion should always be "delete the new file", never "restore
//! from a backup you didn't take".

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

const USAGE: &str = "\
usage: datalib-migrate-config <data-root|config.yaml> [-o OUT] [--stdout] [--force]

  <data-root|config.yaml>  A data root (its config.yaml is used) or the
                           legacy config file itself.
  -o, --output OUT         Write here instead of <input dir>/config.toml.
  --stdout                 Print the converted config; write nothing.
  --force                  Overwrite the output file if it exists.
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
            bail!(
                "no legacy config at {} — nothing to migrate",
                input.display()
            );
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
    std::fs::write(&out, &converted).with_context(|| format!("write {}", out.display()))?;

    eprintln!("migrated {} -> {}", input.display(), out.display());
    eprintln!(
        "Review it, then remove {} once you're happy.",
        input.display()
    );
    Ok(())
}
