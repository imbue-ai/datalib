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
    // A config can name a secret in a step's `env`: owner-only, as the app
    // writes one.
    datalib_runtime::atomic::write_owner_only(&out, converted.as_bytes())
        .with_context(|| format!("write {}", out.display()))?;

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

    /// The rewrite took the umask's mode, or kept a loose one it wrote
    /// over, though the app writes every config owner-only.
    #[cfg(unix)]
    #[test]
    fn the_rewritten_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("old.toml");
        std::fs::write(
            &input,
            "[[steps]]\nid = \"slack/raw\"\ncommand = \"datalib-step download slack_api\"\n",
        )
        .unwrap();
        let fresh = dir.path().join("fresh.toml");
        let loose = dir.path().join("loose.toml");
        std::fs::write(&loose, "").unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();

        for out in [&fresh, &loose] {
            super::run(super::Cli {
                input: input.clone(),
                output: Some(out.clone()),
                stdout: false,
                force: true,
            })
            .unwrap();
            let mode = std::fs::metadata(out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", out.display());
        }
    }

    #[test]
    fn the_help_examples_parse() {
        super::Cli::command().debug_assert();
        datalib_cli_help::assert_examples_parse(super::Cli::command(), super::EXAMPLES);
    }

    #[test]
    fn the_commands_the_docs_show_parse() {
        let commands = datalib_agent_docs::commands_running("datalib-migrate-config");
        assert!(!commands.is_empty(), "no doc runs datalib-migrate-config");
        datalib_cli_help::assert_argvs_parse(super::Cli::command(), &commands);
    }
}
