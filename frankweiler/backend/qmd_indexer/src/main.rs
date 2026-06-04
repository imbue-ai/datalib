// Standalone CLI: --help output and qmd-status pass-through go to
// stderr by design; nothing in this process owns a MultiProgress.
// Exempt from the workspace-wide ban defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! CLI entry point. The actual work lives in the library
//! (`frankweiler_qmd_indexer::run_index`) so other crates (notably
//! `frankweiler-etl`'s loader) can drive it in-process.
//!
//! Usage:
//!   frankweiler-qmd-indexer --root <DIR> [--no-embed] [--qmd-version <V>]
//!                           [--collection-name <N>] [--mask <GLOB>]
//!                           [--models-dir <DIR>]

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use frankweiler_qmd_indexer::{run_index, IndexOptions};

fn parse_args() -> Result<IndexOptions> {
    let mut root: Option<PathBuf> = None;
    let mut embed: Option<bool> = None;
    let mut qmd_version: Option<String> = None;
    let mut collection_name: Option<String> = None;
    let mut mask: Option<String> = None;
    let mut models_dir: Option<PathBuf> = None;

    let mut it = std::env::args_os().skip(1);
    while let Some(raw) = it.next() {
        let arg = raw.to_string_lossy().into_owned();
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(next_value(&mut it, "--root")?)),
            "--no-embed" => embed = Some(false),
            "--embed" => embed = Some(true),
            "--qmd-version" => qmd_version = Some(next_value(&mut it, "--qmd-version")?),
            "--collection-name" => {
                collection_name = Some(next_value(&mut it, "--collection-name")?)
            }
            "--mask" => mask = Some(next_value(&mut it, "--mask")?),
            "--models-dir" => {
                models_dir = Some(PathBuf::from(next_value(&mut it, "--models-dir")?))
            }
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
    if let Some(v) = collection_name {
        o.collection_name = v;
    }
    if let Some(v) = mask {
        o.mask = v;
    }
    if let Some(v) = models_dir {
        o.models_dir = v;
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
        "frankweiler-qmd-indexer --root <DIR> [--no-embed] \
         [--qmd-version <V>] [--collection-name <N>] [--mask <GLOB>] \
         [--models-dir <DIR>]"
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
