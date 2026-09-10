// Standalone CLI: --help output and qmd-status pass-through go to
// stderr by design; nothing in this process owns a MultiProgress.
// Exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! CLI entry point. The actual work lives in the library
//! (`datalib_qmd_indexer::run_index`) so other crates (notably
//! `datalib-etl`'s loader) can drive it in-process.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use datalib_qmd_indexer::{discover_groups, run_index, IndexOptions, LEGACY_COLLECTION_NAME};

fn parse_args() -> Result<IndexOptions> {
    let mut root: Option<PathBuf> = None;
    let mut embed: Option<bool> = None;
    let mut qmd_version: Option<String> = None;
    let mut groups: Vec<String> = Vec::new();
    let mut models_dir: Option<PathBuf> = None;
    let mut pull: Option<bool> = None;

    let mut it = std::env::args_os().skip(1);
    while let Some(raw) = it.next() {
        let arg = raw.to_string_lossy().into_owned();
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(next_value(&mut it, "--root")?)),
            "--no-embed" => embed = Some(false),
            "--embed" => embed = Some(true),
            "--qmd-version" => qmd_version = Some(next_value(&mut it, "--qmd-version")?),
            "--group" => groups.push(next_value(&mut it, "--group")?),
            "--models-dir" => {
                models_dir = Some(PathBuf::from(next_value(&mut it, "--models-dir")?))
            }
            // For a caller that has already put the embedding model in
            // `--models-dir` and never queries: `pull` would delete it
            // again. See `IndexOptions::pull`.
            "--no-pull" => pull = Some(false),
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    let root = root.context("--root <DIR> is required")?;
    let mut o = IndexOptions::new(root);
    if let Some(v) = embed {
        o.embed = v;
    }
    if let Some(v) = qmd_version {
        o.qmd_version = v;
    }
    // A caller that named no groups gets every group that has a rendered
    // tree — the standalone CLI has no config to read. `datalib-step`
    // passes the graph's list instead.
    o.groups = if groups.is_empty() {
        discover_groups(&o.root)?
    } else {
        groups
    };
    o.retire_collections = vec![LEGACY_COLLECTION_NAME.to_string()];
    if let Some(v) = models_dir {
        o.models_dir = v;
    }
    if let Some(v) = pull {
        o.pull = v;
    }
    Ok(o)
}

fn next_value<I: Iterator<Item = OsString>>(it: &mut I, flag: &str) -> Result<String> {
    let v = it
        .next()
        .with_context(|| format!("{flag} requires a value"))?;
    Ok(v.to_string_lossy().into_owned())
}

fn print_help() {
    eprintln!(
        "datalib-qmd-indexer --root <DIR> [--no-embed] \
         [--qmd-version <V>] [--group <GROUP>]... \
         [--models-dir <DIR>] [--no-pull]"
    );
}

fn main() -> Result<()> {
    let opts = parse_args()?;
    let outcome = run_index(&opts)?;
    if let Some(status) = outcome.status_output {
        eprintln!("---- qmd status ----");
        eprint!("{status}");
    }
    Ok(())
}
