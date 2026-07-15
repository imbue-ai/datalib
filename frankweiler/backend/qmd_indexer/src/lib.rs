//! Drive the `qmd` CLI to (re)build a BM25 + embedding index over the
//! rendered conversation markdown tree at a given root.
//!
//! QMD (https://github.com/tobi/qmd) is an npm package. We invoke it via
//! [`frankweiler_core::qmd::qmd_command`] — the app-bundled Node runtime
//! when staged, else `npx -y @tobilu/qmd@<version>` — so callers don't
//! need a global install.
//!
//! QMD stores its index under `$XDG_CACHE_HOME/qmd/index.sqlite`. We pin
//! it inside the data root by setting `XDG_CACHE_HOME=<root>/system`, so
//! the resulting index lives at `<root>/system/qmd/index.sqlite` alongside
//! the other cross-stanza aggregates (`backend_index/db.doltlite_db`).
//!
//! The run is **incremental** — qmd's `update` only re-indexes changed
//! files. The first run lazily creates the collection via `collection add`
//! (detected by the absence of `index.sqlite`); subsequent runs skip
//! straight to `update` + optional `embed`.
//!
//! qmd stores its ~300MB embedding model under
//! `<XDG_CACHE_HOME>/qmd/models/`, which would otherwise land inside the
//! data root and bloat any archive of it. The models cache is independent
//! of the index, so we pre-create `<root>/qmd/models` as a symlink to a
//! shared `models_dir` (default `~/.cache/qmd/models` — the same path a
//! standalone `qmd` run uses, so the two share one cache). qmd treats
//! the symlink transparently and models stay outside the data root.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use frankweiler_core::sync_phase::SyncPhase;
use frankweiler_obs::status_line;

/// Re-export of the ONE canonical qmd pin (`frankweiler_core::qmd`) —
/// a re-export rather than a literal so this crate *cannot* drift from
/// the search runner/daemon the way two same-named constants once did.
pub use frankweiler_core::qmd::DEFAULT_QMD_VERSION;

pub const DEFAULT_COLLECTION_NAME: &str = "mirror";
/// Only index per-stanza rendered markdown — `<root>/<stanza>/rendered_md/**`.
/// The leading `*/` is exactly one stanza segment, so this never descends into
/// `<root>/system/` (where the qmd index itself and other aggregates live).
pub const DEFAULT_MASK: &str = "*/rendered_md/**/*.md";

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
///
/// These mirror `tests/fixtures/materialize_tng_root.sh`'s
/// `REQUIRED_MODELS` — keep the two lists in sync when bumping
/// `DEFAULT_QMD_VERSION`. We intentionally list only the embedding +
/// query-expansion models (not the reranker): they're the gate the
/// fixture + README guarantee, and qmd lazily fetches any other model
/// (e.g. the reranker) on first use, so a missing one degrades to a
/// one-time on-demand download rather than a hard failure.
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

/// Run an incremental qmd index pass over `<root>/rendered_md/*.md` (and
/// every other `.md` under root). Creates the collection lazily on first
/// run; subsequent runs only `update` + optional `embed`.
pub fn run_index(opts: &IndexOptions) -> Result<IndexOutcome> {
    let root = opts
        .root
        .canonicalize()
        .with_context(|| format!("root does not exist: {}", opts.root.display()))?;

    // qmd writes `<XDG_CACHE_HOME>/qmd/index.sqlite`; point it at `<root>/system`
    // so the index lands at `<root>/system/qmd/`. The collection-add scan root
    // below stays `<root>` so qmd still sees every stanza's `rendered_md/`.
    let cache_home = frankweiler_core::qmd::qmd_cache_home(&root);
    let qmd_dir = cache_home.join("qmd");
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
        if frankweiler_core::node_runtime::is_bundled(&frankweiler_core::qmd::qmd_command(
            &opts.qmd_version
        )) {
            "bundled runtime"
        } else {
            "via npx"
        }
    );
    status_line!("[qmd-indexer] embed       = {}", opts.embed);
    status_line!(
        "[qmd-indexer] mode        = {}",
        if first_run { "create" } else { "incremental" }
    );

    // Phase markers for the http worker's progress display (see
    // `frankweiler_core::sync_phase`): collection add/update is the
    // Index stage, the embed subcommand is the Embed stage.
    status_line!("{}", SyncPhase::Index.marker());
    if first_run {
        ensure_collection(
            &cache_home,
            &opts.qmd_version,
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
    run_qmd(&cache_home, &opts.qmd_version, &["update"])?;
    if opts.embed {
        status_line!("{}", SyncPhase::Embed.marker());
        run_qmd(&cache_home, &opts.qmd_version, &["embed"])?;
    }

    // Eagerly pull the query-expansion and reranker models so the first
    // user query (via the UI or `qmd query` directly) doesn't pay the
    // multi-hundred-MB download cost on the interactive path. The embed
    // step above already pulled the embedding model; `qmd pull` is
    // idempotent so re-pulling it is free (cache-checked).
    //
    // Best-effort: a failure here doesn't fail the index build — the
    // index is on disk and queries still work, just with the first one
    // paying the download cost. Most likely failure mode is a network
    // hiccup pulling from huggingface; we don't want that to mark an
    // otherwise-fine sync as errored.
    if let Err(e) = run_qmd(&cache_home, &opts.qmd_version, &["pull"]) {
        status_line!("[qmd-indexer] qmd pull failed (non-fatal): {e:#}");
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
    let mut cmd = frankweiler_core::qmd::qmd_command(qmd_version);
    cmd.arg("status");
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    // Make sure ANSI color codes stay out of the captured text — qmd
    // disables color when stdout isn't a TTY (which it isn't here), but
    // belt-and-braces.
    cmd.env("NO_COLOR", "1");
    status_line!(
        "[qmd-indexer] $ {}",
        frankweiler_core::node_runtime::display_command(&cmd)
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
///
/// Why this can't just lean on the `first_run` (`!index.sqlite`) gate:
/// qmd records the collection in its config the moment `collection add`
/// runs, but `index.sqlite` only appears after a successful `update`. So
/// a run that registers the collection and then dies before `update`
/// finishes (e.g. the `embed` step fails on a native-module/ABI error)
/// leaves the collection registered with no index file. Every later run
/// then sees `first_run == true`, re-runs `collection add`, and aborts.
/// Swallowing "already exists" makes the step re-entrant.
fn ensure_collection(cache_home: &Path, qmd_version: &str, args: &[&str]) -> Result<()> {
    let mut cmd = frankweiler_core::qmd::qmd_command(qmd_version);
    cmd.args(args);
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    cmd.env("NO_COLOR", "1");
    status_line!(
        "[qmd-indexer] $ {}",
        frankweiler_core::node_runtime::display_command(&cmd)
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

fn run_qmd(cache_home: &Path, qmd_version: &str, args: &[&str]) -> Result<()> {
    // Resolution (bundled runtime vs npx, `$NPX_BIN` override) lives in
    // `frankweiler_core::qmd::qmd_command`. Bazel actions don't get
    // `$NPX_BIN` forwarded (would bust action cache keys) and instead
    // rely on `PATH` (pinned in `.bazelrc`).
    let mut cmd = frankweiler_core::qmd::qmd_command(qmd_version);
    cmd.args(args);
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    status_line!(
        "[qmd-indexer] $ {}",
        frankweiler_core::node_runtime::display_command(&cmd)
    );
    // `.status()` lets the child inherit our stdout/stderr, so qmd's own
    // output lands on the same terminal as the orchestrator's live
    // progress bars. Suspend the shared `MultiProgress` across the run
    // so the two don't interleave — bars are hidden while qmd prints,
    // then redrawn. No-op (plain run) when no bars are live, e.g. the
    // standalone CLI or tests, where `shared_multi()` returns `None`.
    let mut run = || cmd.status();
    let status = match frankweiler_obs::shared_multi() {
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
