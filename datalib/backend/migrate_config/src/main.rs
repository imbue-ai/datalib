// Standalone one-shot CLI: the user runs it directly and its whole
// interface is stdout/stderr. It never runs under the sync pipeline, so
// there are no indicatif bars to corrupt — the case clippy.toml names as
// a legitimate exception to the workspace-wide macro ban.
#![allow(clippy::disallowed_macros)]

//! `datalib-migrate-config` — rewrite a `config.toml` from a shape the
//! runner no longer accepts into the one it does.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use datalib_cli_help::Example;

const EXAMPLES: &[Example] = &[
    Example {
        what: "rewrite a data root's config in place, keeping the original as config.toml.orig",
        command: "datalib-migrate-config ~/datalib --force",
    },
    Example {
        what: "print the rewrite and write nothing",
        command: "datalib-migrate-config ~/datalib/config.toml --stdout",
    },
];

#[derive(Parser)]
#[command(
    name = "datalib-migrate-config",
    bin_name = "datalib-migrate-config",
    about = "Rewrite a config.toml from a shape the runner no longer accepts into the one it does.",
    after_help = datalib_cli_help::render(EXAMPLES)
)]
struct Cli {
    /// A data root (its config.toml is used) or the config file itself.
    #[arg(value_name = "DATA_ROOT|CONFIG")]
    input: PathBuf,
    /// Write here instead of <input dir>/config.toml.
    #[arg(short, long, value_name = "OUT", conflicts_with = "stdout")]
    output: Option<PathBuf>,
    /// Print the converted config; write nothing.
    #[arg(long)]
    stdout: bool,
    /// Overwrite the output file if it exists. Rewriting a config in place
    /// keeps the original beside it as config.toml.orig.
    #[arg(long)]
    force: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        // A migration failure is a dead end for the user, not a panic:
        // report it plainly on stderr and exit non-zero.
        eprintln!("datalib-migrate-config: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let Cli {
        input: arg,
        output,
        stdout: to_stdout,
        force,
    } = cli;

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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn the_help_examples_parse() {
        super::Cli::command().debug_assert();
        datalib_cli_help::assert_examples_parse(super::Cli::command(), super::EXAMPLES);
    }
}
