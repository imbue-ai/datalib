//! Drive the `qmd` CLI to (re)build a BM25 + embedding index over the
//! rendered conversation markdown tree at a given root.
//!
//! QMD (https://github.com/tobi/qmd) is an npm package. We invoke it via
//! `npx -y @tobilu/qmd@<version>` so callers don't need a global install.
//!
//! QMD stores its index under `$XDG_CACHE_HOME/qmd/index.sqlite`. We pin
//! it inside the data root by setting `XDG_CACHE_HOME=<root>/.frankweiler`,
//! so the resulting index lives at `<root>/.frankweiler/qmd/index.sqlite`
//! alongside everything else the backend owns.
//!
//! The run is **incremental** — qmd's `update` only re-indexes changed
//! files. The first run lazily creates the collection via `collection add`
//! (detected by the absence of `index.sqlite`); subsequent runs skip
//! straight to `update` + optional `embed`.
//!
//! qmd stores its ~300MB embedding model under
//! `<XDG_CACHE_HOME>/qmd/models/`, which would otherwise land inside the
//! data root and bloat any archive of it. The models cache is independent
//! of the index, so we pre-create `<root>/.frankweiler/qmd/models` as a
//! symlink to a shared `models_dir` (default `~/.cache/qmd-models`). qmd
//! treats the symlink transparently and models stay outside the data root.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub const DEFAULT_QMD_VERSION: &str = "2.1.0";
pub const DEFAULT_COLLECTION_NAME: &str = "mirror";
pub const DEFAULT_MASK: &str = "**/*.md";

/// Options for an indexer run. Construct with `IndexOptions::new(root)` and
/// override fields as needed.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub root: PathBuf,
    pub embed: bool,
    pub qmd_version: String,
    pub collection_name: String,
    pub mask: String,
    pub models_dir: PathBuf,
}

impl IndexOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            embed: true,
            qmd_version: DEFAULT_QMD_VERSION.to_string(),
            collection_name: DEFAULT_COLLECTION_NAME.to_string(),
            mask: DEFAULT_MASK.to_string(),
            models_dir: default_models_dir(),
        }
    }
}

pub fn default_models_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("qmd-models")
}

/// Run an incremental qmd index pass over `<root>/rendered_md/*.md` (and
/// every other `.md` under root). Creates the collection lazily on first
/// run; subsequent runs only `update` + optional `embed`.
pub fn run_index(opts: &IndexOptions) -> Result<PathBuf> {
    let root = opts
        .root
        .canonicalize()
        .with_context(|| format!("root does not exist: {}", opts.root.display()))?;

    let cache_home = root.join(".frankweiler");
    let qmd_dir = cache_home.join("qmd");
    std::fs::create_dir_all(&qmd_dir)
        .with_context(|| format!("failed to create {}", qmd_dir.display()))?;

    std::fs::create_dir_all(&opts.models_dir).with_context(|| {
        format!("failed to create models dir {}", opts.models_dir.display())
    })?;
    ensure_models_symlink(&qmd_dir, &opts.models_dir)?;

    let qmd_pkg = format!("@tobilu/qmd@{}", opts.qmd_version);
    let index_path = qmd_dir.join("index.sqlite");
    let first_run = !index_path.exists();

    eprintln!("[qmd-indexer] root        = {}", root.display());
    eprintln!("[qmd-indexer] index dir   = {}", qmd_dir.display());
    eprintln!(
        "[qmd-indexer] models dir  = {} (symlinked)",
        opts.models_dir.display()
    );
    eprintln!("[qmd-indexer] qmd package = {qmd_pkg}");
    eprintln!("[qmd-indexer] embed       = {}", opts.embed);
    eprintln!("[qmd-indexer] mode        = {}", if first_run { "create" } else { "incremental" });

    if first_run {
        run_qmd(
            &cache_home,
            &qmd_pkg,
            &[
                "collection",
                "add",
                root.to_str().context("root is not valid UTF-8")?,
                "--name",
                &opts.collection_name,
                "--mask",
                &opts.mask,
            ],
        )?;
    }
    run_qmd(&cache_home, &qmd_pkg, &["update"])?;
    if opts.embed {
        run_qmd(&cache_home, &qmd_pkg, &["embed"])?;
    }

    if !index_path.exists() {
        bail!(
            "qmd reported success but index.sqlite is missing at {}",
            index_path.display()
        );
    }
    eprintln!("[qmd-indexer] wrote {}", index_path.display());
    Ok(index_path)
}

fn ensure_models_symlink(qmd_dir: &Path, models_dir: &Path) -> Result<()> {
    let models_link = qmd_dir.join("models");
    match std::fs::symlink_metadata(&models_link) {
        Ok(meta) if meta.file_type().is_symlink() => return Ok(()),
        Ok(_) => bail!(
            "{} exists and is not a symlink — remove it to let the indexer manage it",
            models_link.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {}", models_link.display())),
    }
    std::os::unix::fs::symlink(models_dir, &models_link).with_context(|| {
        format!(
            "failed to symlink {} -> {}",
            models_link.display(),
            models_dir.display()
        )
    })?;
    Ok(())
}

fn run_qmd(cache_home: &Path, qmd_pkg: &str, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("npx");
    cmd.arg("-y").arg(qmd_pkg).args(args);
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    eprintln!("[qmd-indexer] $ npx -y {qmd_pkg} {}", args.join(" "));
    let status = cmd
        .status()
        .with_context(|| "failed to spawn npx; is Node.js installed?")?;
    if !status.success() {
        bail!("qmd {:?} failed: {status}", args);
    }
    Ok(())
}
