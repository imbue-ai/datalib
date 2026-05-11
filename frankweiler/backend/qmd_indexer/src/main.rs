//! Drive the `qmd` CLI to (re)build a BM25 + embedding index over the
//! rendered conversation markdown tree at a given root.
//!
//! QMD (https://github.com/tobi/qmd) is an npm package. We invoke it via
//! `npx -y @tobilu/qmd@<version>` so callers don't need a global install.
//!
//! QMD stores its index under `$XDG_CACHE_HOME/qmd/index.sqlite`, which
//! normally falls back to the user's home cache. We pin it inside the data
//! root by setting `XDG_CACHE_HOME=<root>/.frankweiler`, so the resulting
//! index lives at `<root>/.frankweiler/qmd/index.sqlite` alongside everything
//! else the backend owns.
//!
//! The run is non-incremental: we wipe `<root>/.frankweiler/qmd/` first so
//! repeated invocations produce the same logical state from scratch.
//!
//! qmd stores its ~300MB embedding model under `<XDG_CACHE_HOME>/qmd/models/`,
//! which would otherwise land inside the data root and bloat any archive of
//! it. The models cache is independent of the index, so we pre-create
//! `<root>/.frankweiler/qmd/models` as a symlink to a shared `--models-dir`
//! (default `~/.cache/qmd-models`). qmd treats the symlink transparently and
//! models stay outside the data root.
//!
//! Usage:
//!   frankweiler-qmd-indexer --root <DIR> [--no-embed] [--qmd-version <V>]
//!                           [--collection-name <N>] [--mask <GLOB>]
//!                           [--models-dir <DIR>]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const DEFAULT_QMD_VERSION: &str = "2.1.0";
const DEFAULT_COLLECTION_NAME: &str = "mirror";
const DEFAULT_MASK: &str = "**/*.qmd";

struct Args {
    root: PathBuf,
    embed: bool,
    qmd_version: String,
    collection_name: String,
    mask: String,
    models_dir: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut root: Option<PathBuf> = None;
    let mut embed = true;
    let mut qmd_version = DEFAULT_QMD_VERSION.to_string();
    let mut collection_name = DEFAULT_COLLECTION_NAME.to_string();
    let mut mask = DEFAULT_MASK.to_string();
    let mut models_dir: Option<PathBuf> = None;

    let mut it = std::env::args_os().skip(1);
    while let Some(raw) = it.next() {
        let arg = raw.to_string_lossy().into_owned();
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(next_value(&mut it, "--root")?)),
            "--no-embed" => embed = false,
            "--embed" => embed = true,
            "--qmd-version" => qmd_version = next_value(&mut it, "--qmd-version")?,
            "--collection-name" => collection_name = next_value(&mut it, "--collection-name")?,
            "--mask" => mask = next_value(&mut it, "--mask")?,
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
    let models_dir = models_dir.unwrap_or_else(default_models_dir);
    Ok(Args {
        root,
        embed,
        qmd_version,
        collection_name,
        mask,
        models_dir,
    })
}

fn default_models_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("qmd-models")
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
    let args = parse_args()?;
    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("root does not exist: {}", args.root.display()))?;

    let cache_home = root.join(".frankweiler");
    let qmd_dir = cache_home.join("qmd");
    if qmd_dir.exists() {
        // remove_dir_all does NOT traverse symlinks (good — `models` is a
        // symlink to a shared dir whose contents we want to preserve).
        std::fs::remove_dir_all(&qmd_dir)
            .with_context(|| format!("failed to clear {}", qmd_dir.display()))?;
    }
    std::fs::create_dir_all(&qmd_dir)
        .with_context(|| format!("failed to create {}", qmd_dir.display()))?;

    let models_dir = args.models_dir.clone();
    std::fs::create_dir_all(&models_dir)
        .with_context(|| format!("failed to create models dir {}", models_dir.display()))?;
    let models_link = qmd_dir.join("models");
    std::os::unix::fs::symlink(&models_dir, &models_link).with_context(|| {
        format!(
            "failed to symlink {} -> {}",
            models_link.display(),
            models_dir.display()
        )
    })?;

    let qmd_pkg = format!("@tobilu/qmd@{}", args.qmd_version);
    eprintln!("[qmd-indexer] root        = {}", root.display());
    eprintln!("[qmd-indexer] index dir   = {}", qmd_dir.display());
    eprintln!(
        "[qmd-indexer] models dir  = {} (symlinked)",
        models_dir.display()
    );
    eprintln!("[qmd-indexer] qmd package = {qmd_pkg}");
    eprintln!("[qmd-indexer] embed       = {}", args.embed);

    run_qmd(
        &cache_home,
        &qmd_pkg,
        &[
            "collection",
            "add",
            root.to_str().context("root is not valid UTF-8")?,
            "--name",
            &args.collection_name,
            "--mask",
            &args.mask,
        ],
    )?;
    run_qmd(&cache_home, &qmd_pkg, &["update"])?;
    if args.embed {
        run_qmd(&cache_home, &qmd_pkg, &["embed"])?;
    }

    let index = qmd_dir.join("index.sqlite");
    if !index.exists() {
        bail!(
            "qmd reported success but index.sqlite is missing at {}",
            index.display()
        );
    }
    eprintln!("[qmd-indexer] wrote {}", index.display());
    Ok(())
}

fn run_qmd(cache_home: &Path, qmd_pkg: &str, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("npx");
    cmd.arg("-y").arg(qmd_pkg).args(args);
    cmd.env("XDG_CACHE_HOME", cache_home);
    eprintln!("[qmd-indexer] $ npx -y {qmd_pkg} {}", args.join(" "));
    let status = cmd
        .status()
        .with_context(|| "failed to spawn npx; is Node.js installed?")?;
    if !status.success() {
        bail!("qmd {:?} failed: {status}", args);
    }
    Ok(())
}
