//! Drive the `qmd` CLI to (re)build a BM25 + embedding index over the
//! rendered conversation markdown tree at a given root.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_obs::status_line;

/// Re-export of the ONE canonical qmd pin (`datalib_runtime::qmd`) — a
/// re-export rather than a literal so this crate *cannot* drift from the
/// search runner/daemon the way two same-named constants once did.
pub use datalib_runtime::qmd::DEFAULT_QMD_VERSION;

/// The single collection every rendered document used to live in.
/// Nothing indexes into it any more; the name survives so a data root
/// built before per-source collections can be migrated off it.
pub const LEGACY_COLLECTION_NAME: &str = "mirror";

/// The glob one group's collection covers, relative to the data root.
///
/// Every collection is rooted at the **data root**, not at the group's own
/// tree, so a hit's path stays `<group>/render_markdown/…` — the exact
/// string `grid_rows.qmd_path` holds, which is what maps a hit back to its
/// rows. Rooting a collection at `<root>/<group>/render_markdown` instead
/// would shorten every stored path by that prefix and break the join.
pub fn mask_for_group(group: &str) -> String {
    format!("{group}/render_markdown/**/*.md")
}

/// The groups under `root` that have a rendered-markdown tree.
///
/// For a caller with no config to read — the standalone CLI. The step
/// passes the graph's own list instead, which is the better source: it
/// omits a directory left behind by a source that has since been removed
/// from the config.
pub fn discover_groups(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("read dir {}", root.display()))?
        .flatten()
    {
        if !entry.path().join("render_markdown").is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Options for an indexer run. Construct with `IndexOptions::new(root)` and
/// override fields as needed.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub root: PathBuf,
    pub embed: bool,
    pub qmd_version: String,
    /// One qmd collection per group, named after the group. Scoping a
    /// search to one source is then a `collections` argument qmd applies
    /// *inside* retrieval, instead of a filter over a global top-N —
    /// which drops a source's hits entirely whenever a larger source
    /// fills that global list.
    pub groups: Vec<String>,
    /// Collections to unregister once this run's indexing pass is done.
    /// See [`run_index`] for why the removal cannot come earlier.
    pub retire_collections: Vec<String>,
    pub models_dir: PathBuf,
    /// Whether to run `qmd pull` before embedding. On by default,
    /// because it is what puts the query-expansion and reranker models
    /// in place for the first interactive query.
    pub pull: bool,
}

impl IndexOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            embed: true,
            qmd_version: DEFAULT_QMD_VERSION.to_string(),
            groups: Vec::new(),
            retire_collections: Vec::new(),
            models_dir: default_models_dir(),
            pull: true,
        }
    }
}

/// Default location of the shared qmd model cache. Matches qmd's own
/// default (`$XDG_CACHE_HOME/qmd/models`, falling back to
/// `~/.cache/qmd/models` — see `third-party/qmd/src/llm.ts`'s
/// `MODEL_CACHE_DIR`), so a standalone `qmd` run and a build-driven run
/// share one cache instead of each downloading their own copy.
pub fn default_models_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("qmd").join("models");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("qmd").join("models")
}

/// The GGUF model files `npx -y @tobilu/qmd@<DEFAULT_QMD_VERSION> pull`
/// lands in the cache dir, by their on-disk filenames (qmd derives these
/// from the HF URIs). Used by [`models_present`] to detect a cold cache
/// (the backend logs a first-search-will-download heads-up).
pub const REQUIRED_MODELS: &[&str] = &[
    "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf",
    "hf_tobil_qmd-query-expansion-1.7B-q4_k_m.gguf",
];

/// True when every [`REQUIRED_MODELS`] file exists and is non-empty
/// under `models_dir` (symlinks are followed, so passing the per-root
/// `<root>/qmd/models` link resolves out to the shared cache). Lets a
/// caller skip the network round-trip of `qmd pull` when the cache is
/// already warm.
pub fn models_present(models_dir: &Path) -> bool {
    REQUIRED_MODELS.iter().all(|name| {
        std::fs::metadata(models_dir.join(name))
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false)
    })
}

/// Result of a `run_index` pass. `status_output` is the raw stdout of
/// `qmd status` (qmd has no `--json` flag, so this is the human-readable
/// text) and is `None` if the status capture failed for any reason —
/// indexing success doesn't depend on it.
#[derive(Debug, Clone)]
pub struct IndexOutcome {
    pub index_path: PathBuf,
    pub status_output: Option<String>,
}

/// Run an incremental qmd index pass over every group's `render_markdown/`
/// tree under `<root>`, one collection per group. Registering a
/// collection is idempotent, so this reconciles rather than assuming a
/// first run: a source added after the index was built gets its
/// collection here.
pub fn run_index(opts: &IndexOptions) -> Result<IndexOutcome> {
    let root = opts
        .root
        .canonicalize()
        .with_context(|| format!("root does not exist: {}", opts.root.display()))?;

    // qmd writes `<XDG_CACHE_HOME>/qmd/index.sqlite`; point it at the
    // `qmd_index` step's own tree so that is the only tree the step writes.
    // The collection-add scan root below stays `<root>` so qmd still sees
    // every group's `render_markdown/`.
    let cache_home = datalib_runtime::qmd::qmd_cache_home(&root);
    let qmd_dir = datalib_runtime::qmd::qmd_state_dir(&root);
    std::fs::create_dir_all(&qmd_dir)
        .with_context(|| format!("failed to create {}", qmd_dir.display()))?;

    std::fs::create_dir_all(&opts.models_dir)
        .with_context(|| format!("failed to create models dir {}", opts.models_dir.display()))?;
    ensure_models_symlink(&qmd_dir, &opts.models_dir)?;

    let index_path = qmd_dir.join("index.sqlite");
    let first_run = !index_path.exists();

    status_line!("[qmd-indexer] root        = {}", root.display());
    status_line!("[qmd-indexer] index dir   = {}", qmd_dir.display());
    status_line!(
        "[qmd-indexer] models dir  = {} (symlinked)",
        opts.models_dir.display()
    );
    status_line!(
        "[qmd-indexer] qmd package = @tobilu/qmd@{} ({})",
        opts.qmd_version,
        if datalib_runtime::node_runtime::is_bundled(&datalib_runtime::qmd::qmd_command(
            &opts.qmd_version
        )) {
            "bundled runtime"
        } else {
            "via npx"
        }
    );
    status_line!("[qmd-indexer] embed       = {}", opts.embed);
    status_line!("[qmd-indexer] collections = {}", opts.groups.join(", "));
    status_line!(
        "[qmd-indexer] mode        = {}",
        if first_run { "create" } else { "incremental" }
    );

    let root_arg = root.to_str().context("root is not valid UTF-8")?;
    for group in &opts.groups {
        let mask = mask_for_group(group);
        ensure_collection(
            &cache_home,
            &opts.qmd_version,
            &[
                "collection",
                "add",
                root_arg,
                "--name",
                group,
                "--mask",
                &mask,
            ],
        )?;
    }
    run_qmd(&cache_home, &opts.qmd_version, &["update"])?;

    // Retiring a collection is destructive and has to come *after* the
    // indexing pass above. `qmd collection remove` deletes that
    // collection's `documents` rows and then every `content` row whose
    // hash no longer has an active document row anywhere.
    //
    // Measured on the TNG fixture (76 documents), migrating off `mirror`:
    // in this order qmd reports "Deleted 76 documents" and cleans up no
    // content, because the per-group collections already reference those
    // hashes — content, vectors and every `embedded_at` come through
    // untouched. Retire first and it reports "Cleaned up 76 orphaned
    // content hashes" instead, emptying the index of document bodies.
    // The vectors themselves survive that (nothing cascades to
    // `content_vectors`), so a later re-index can re-insert the same
    // hashes and reuse them — but only if qmd's `cleanupOrphanedVectors`
    // has not run in the window, and it is not worth finding out.
    for name in &opts.retire_collections {
        retire_collection(&cache_home, &opts.qmd_version, name)?;
    }

    // Pull BEFORE embed, and the order is the whole point.
    if opts.pull {
        if let Err(e) = run_qmd(&cache_home, &opts.qmd_version, &["pull"]) {
            status_line!("[qmd-indexer] qmd pull failed (non-fatal): {e:#}");
        }
    } else {
        status_line!("[qmd-indexer] pull        = skipped (models pre-staged)");
    }

    if opts.embed {
        run_qmd(&cache_home, &opts.qmd_version, &["embed"])?;
    }

    if !index_path.exists() {
        bail!(
            "qmd reported success but index.sqlite is missing at {}",
            index_path.display()
        );
    }
    status_line!("[qmd-indexer] wrote {}", index_path.display());

    // Capture `qmd status` for the run summary. Best-effort: a failure
    // here doesn't fail the index build — the index is already on disk
    // and usable.
    let status_output = match capture_qmd_status(&cache_home, &opts.qmd_version) {
        Ok(s) => Some(s),
        Err(e) => {
            status_line!("[qmd-indexer] qmd status capture failed (non-fatal): {e:#}");
            None
        }
    };

    Ok(IndexOutcome {
        index_path,
        status_output,
    })
}

/// Ensure `<qmd_dir>/models` is a symlink to `models_dir` (the shared
/// cache), so qmd — run with `XDG_CACHE_HOME=<root>` — resolves model
/// lookups out to one shared copy instead of downloading into the data
/// root. Idempotent: a no-op when the link already exists; errors if the
/// path exists as a real (non-symlink) entry so the caller can decide
/// whether to surface or tolerate that.
pub fn ensure_models_symlink(qmd_dir: &Path, models_dir: &Path) -> Result<()> {
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

fn capture_qmd_status(cache_home: &Path, qmd_version: &str) -> Result<String> {
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version);
    cmd.arg("status");
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    // Make sure ANSI color codes stay out of the captured text — qmd
    // disables color when stdout isn't a TTY (which it isn't here), but
    // belt-and-braces.
    cmd.env("NO_COLOR", "1");
    status_line!(
        "[qmd-indexer] $ {}",
        datalib_runtime::node_runtime::display_command(&cmd)
    );
    let out = cmd
        .output()
        .with_context(|| "failed to spawn qmd; is Node.js installed?")?;
    if !out.status.success() {
        bail!(
            "qmd status failed: {}: stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Register the qmd collection, tolerating the case where a previous
/// (possibly *failed*) run already registered it. qmd's `collection add`
/// aborts with "Collection '<name>' already exists" — which for our
/// idempotent re-runs is success, not failure.
fn ensure_collection(cache_home: &Path, qmd_version: &str, args: &[&str]) -> Result<()> {
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version);
    cmd.args(args);
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    cmd.env("NO_COLOR", "1");
    status_line!(
        "[qmd-indexer] $ {}",
        datalib_runtime::node_runtime::display_command(&cmd)
    );
    // Capture output so we can inspect it for the benign "already exists"
    // case; on the happy path qmd is quiet here anyway.
    let out = cmd
        .output()
        .with_context(|| "failed to spawn qmd; is Node.js installed?")?;
    if out.status.success() {
        return Ok(());
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if combined.contains("already exists") {
        status_line!("[qmd-indexer] collection already registered — continuing");
        return Ok(());
    }
    bail!("qmd {:?} failed: {}: {}", args, out.status, combined.trim());
}

/// Unregister a collection, tolerating one that is already gone.
/// `qmd collection remove` exits non-zero with "Collection not found"
/// for a name it doesn't have, which for a re-run of a migration that
/// already happened is success.
fn retire_collection(cache_home: &Path, qmd_version: &str, name: &str) -> Result<()> {
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version);
    cmd.args(["collection", "remove", name]);
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    cmd.env("NO_COLOR", "1");
    status_line!(
        "[qmd-indexer] $ {}",
        datalib_runtime::node_runtime::display_command(&cmd)
    );
    let out = cmd
        .output()
        .with_context(|| "failed to spawn qmd; is Node.js installed?")?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        status_line!(
            "[qmd-indexer] retired collection {name:?}: {}",
            combined.trim()
        );
        return Ok(());
    }
    if combined.contains("Collection not found") {
        return Ok(());
    }
    bail!(
        "qmd collection remove {name:?} failed: {}: {}",
        out.status,
        combined.trim()
    );
}

fn run_qmd(cache_home: &Path, qmd_version: &str, args: &[&str]) -> Result<()> {
    // Resolution (bundled runtime vs npx, `$NPX_BIN` override) lives in
    // `datalib_runtime::qmd::qmd_command`. Bazel actions don't get
    // `$NPX_BIN` forwarded (would bust action cache keys) and instead
    // rely on `PATH` (pinned in `.bazelrc`).
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version);
    cmd.args(args);
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    status_line!(
        "[qmd-indexer] $ {}",
        datalib_runtime::node_runtime::display_command(&cmd)
    );
    // `.status()` lets the child inherit our stdout/stderr, so qmd's own
    // output lands on the same terminal as the orchestrator's live
    // progress bars. Suspend the shared `MultiProgress` across the run
    // so the two don't interleave — bars are hidden while qmd prints,
    // then redrawn. No-op (plain run) when no bars are live, e.g. the
    // standalone CLI or tests, where `shared_multi()` returns `None`.
    let mut run = || cmd.status();
    let status = match datalib_obs::shared_multi() {
        Some(mp) => mp.suspend(run),
        None => run(),
    }
    .with_context(|| "failed to spawn qmd; is Node.js installed?")?;
    if !status.success() {
        bail!("qmd {:?} failed: {status}", args);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `default_models_dir()` must agree with qmd's `MODEL_CACHE_DIR`
    /// (`third-party/qmd/src/llm.ts`) so a standalone `qmd` run and a
    /// build-driven run share one cache. Upstream-side drift is caught
    /// by `//tools:check_qmd_model_cache_path_test`; this end checks
    /// our half of the contract.
    #[test]
    fn default_models_dir_matches_qmd_default() {
        // XDG_CACHE_HOME branch: $XDG/qmd/models.
        // Use the temp dir as a stand-in so we don't depend on the
        // host's actual XDG_CACHE_HOME value (which CI may or may not
        // set). `set_var` here is fine — Rust tests in a crate share a
        // process, but no other test in this file touches the env.
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::set_var("XDG_CACHE_HOME", "/tmp/qmd-test-xdg") };
        let dir = default_models_dir();
        assert_eq!(dir, PathBuf::from("/tmp/qmd-test-xdg/qmd/models"));

        // HOME fallback: $HOME/.cache/qmd/models.
        // SAFETY: single-threaded test, no concurrent env access.
        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
        unsafe { std::env::set_var("HOME", "/tmp/qmd-test-home") };
        let dir = default_models_dir();
        assert_eq!(dir, PathBuf::from("/tmp/qmd-test-home/.cache/qmd/models"));
    }

    #[test]
    fn models_present_requires_every_required_model_nonempty() {
        let base = std::env::temp_dir().join(format!("qmd-models-present-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // Nothing there yet → absent.
        assert!(!models_present(&base));

        // All required models present + non-empty → present.
        for name in REQUIRED_MODELS {
            std::fs::write(base.join(name), b"gguf").unwrap();
        }
        assert!(models_present(&base));

        // A zero-byte (partial/truncated) model doesn't count.
        std::fs::write(base.join(REQUIRED_MODELS[0]), b"").unwrap();
        assert!(!models_present(&base));

        let _ = std::fs::remove_dir_all(&base);
    }
}
