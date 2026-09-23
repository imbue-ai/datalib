//! Drive `qmd` to (re)build a BM25 + embedding index over the rendered
//! conversation markdown tree at a given root.
//!
//! Indexing goes through the qmd CLI. The embedding pass does not: it is
//! the long one, and the CLI reports its progress only to a terminal, so
//! that pass runs a small script of ours against qmd's SDK instead and
//! reads progress back as NDJSON. See [`EmbedEvent`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use datalib_status_line::status_line;

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

/// How far along the embedding pass is, as qmd's own `EmbedProgress`
/// reports it (`third-party/qmd/src/store.ts`).
///
/// Progress is measured in **input bytes**, not chunks: qmd discovers
/// the chunk count batch by batch, so `total_chunks` climbs during the
/// run and a chunk ratio reads wrong while large documents remain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmbedProgress {
    pub chunks_embedded: u64,
    pub total_chunks: u64,
    pub bytes_processed: u64,
    pub total_bytes: u64,
    /// Failed chunks still awaiting a successful retry.
    pub errors: u64,
}

/// One line of the embed wrapper's NDJSON (`src/js/embed_ndjson.mjs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedEvent {
    Progress(EmbedProgress),
    Done {
        docs_processed: u64,
        chunks_embedded: u64,
        errors: u64,
    },
    /// Another process holds qmd's embed lock, so this pass embedded
    /// nothing.
    Busy,
    Error(String),
}

/// Parse one line of the wrapper's output.
///
/// `None` for anything that is not one of our events — a blank line, or
/// something a dependency printed to stdout. The caller logs those
/// rather than failing on them: a chatty transitive package must not be
/// able to fail an embed that otherwise worked.
pub fn parse_embed_event(line: &str) -> Option<EmbedEvent> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let n = |key: &str| v.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    match v.get("event")?.as_str()? {
        "progress" => Some(EmbedEvent::Progress(EmbedProgress {
            chunks_embedded: n("chunksEmbedded"),
            total_chunks: n("totalChunks"),
            bytes_processed: n("bytesProcessed"),
            total_bytes: n("totalBytes"),
            errors: n("errors"),
        })),
        "done" => Some(EmbedEvent::Done {
            docs_processed: n("docsProcessed"),
            chunks_embedded: n("chunksEmbedded"),
            errors: n("errors"),
        }),
        "busy" => Some(EmbedEvent::Busy),
        "error" => Some(EmbedEvent::Error(
            v.get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("qmd embed failed without a message")
                .to_string(),
        )),
        _ => None,
    }
}

/// Called with each [`EmbedProgress`] as the embedding pass reports it.
pub type OnEmbedProgress = Arc<dyn Fn(EmbedProgress) + Send + Sync>;

/// Options for an indexer run. Construct with `IndexOptions::new(root)` and
/// override fields as needed.
#[derive(Clone)]
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
    /// Where the GGUF models already are. The indexer never fetches
    /// one: `qmd pull` compares an etag against HuggingFace `main` and
    /// re-downloads on any difference, which is how a re-upload upstream
    /// would silently change every embedding. The caller provisions the
    /// pinned, sha256-verified files (`datalib_qmd_models`) before this
    /// runs, and qmd finds them in place.
    pub models_dir: PathBuf,
    /// Where to report the embedding pass's progress. `None` runs it
    /// exactly the same way and drops the numbers.
    pub on_embed_progress: Option<OnEmbedProgress>,
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
            on_embed_progress: None,
        }
    }
}

impl std::fmt::Debug for IndexOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexOptions")
            .field("root", &self.root)
            .field("embed", &self.embed)
            .field("qmd_version", &self.qmd_version)
            .field("groups", &self.groups)
            .field("retire_collections", &self.retire_collections)
            .field("models_dir", &self.models_dir)
            .field("on_embed_progress", &self.on_embed_progress.is_some())
            .finish()
    }
}

/// Default location of the shared qmd model cache. Matches qmd's own
/// default (`$XDG_CACHE_HOME/qmd/models`, falling back to
/// `~/.cache/qmd/models` — see `third-party/qmd/src/llm.ts`'s
/// `MODEL_CACHE_DIR`), so a standalone `qmd` run and a build-driven run
/// share one cache instead of each downloading their own copy.
pub fn default_models_dir() -> PathBuf {
    models_dir_under(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

/// Split out from the environment so the choice between the two roots
/// can be tested as what it is — a decision over two values — rather
/// than by setting variables the whole test process shares.
fn models_dir_under(xdg_cache_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(xdg) = xdg_cache_home {
        return PathBuf::from(xdg).join("qmd").join("models");
    }
    PathBuf::from(home.unwrap_or_else(|| ".".into()))
        .join(".cache")
        .join("qmd")
        .join("models")
}

/// The pinned models `qmd embed` needs, by the on-disk names
/// node-llama-cpp looks for. Just the embedding model: the fixture's
/// embed action stages only that one, and query expansion / reranking
/// are the applet's concern.
pub fn embed_model_names() -> Vec<String> {
    datalib_runtime::qmd::PINNED_MODELS
        .iter()
        .take(1)
        .map(|m| m.cache_name())
        .collect()
}

/// True when every file in `names` exists and is non-empty under
/// `models_dir` (symlinks are followed, so passing the per-root
/// `<root>/qmd/models` link resolves out to the shared cache).
pub fn models_present(models_dir: &Path, names: &[String]) -> bool {
    names.iter().all(|name| {
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
    let probe = datalib_runtime::qmd::qmd_command(&opts.qmd_version)?;
    status_line!(
        "[qmd-indexer] qmd package = @tobilu/qmd@{} ({})",
        opts.qmd_version,
        if datalib_runtime::node_runtime::is_bundled(&probe) {
            "bundled runtime"
        } else {
            "via npx"
        }
    );
    // Through the link, not `opts.models_dir`: a root whose link was
    // made earlier (the test fixture's, pointing at bazel outputs) reads
    // its models from wherever that link goes.
    let models_link = qmd_dir.join("models");
    if !models_present(&models_link, &embed_model_names()) {
        bail!(
            "embedding model missing from {} — expected {}; the caller provisions \
             it (`datalib-step pull-models`) before indexing",
            models_link.display(),
            embed_model_names().join(", ")
        );
    }
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

    if opts.embed {
        run_embed(&cache_home, &qmd_dir, &index_path, opts)?;
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
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version)?;
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
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version)?;
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
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version)?;
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

/// Our SDK driver. Written to a temp file per run and deleted after,
/// so what runs is always the copy this build carries.
///
/// It has to be a *file*: see the script's own header for why `node -e`
/// is not an option, and [`embed_script`] for why the file is not in the
/// data root.
const EMBED_NDJSON_MJS: &str = include_str!("js/embed_ndjson.mjs");

/// The wrapper on disk, removed when this is dropped.
///
/// Outside the data root deliberately. The step's tree is swept whole
/// into the test fixture's overlay tar
/// (`tests/fixtures/build_qmd_index.py`), so a scratch file written
/// beside the index would be baked into the fixture.
struct EmbedScript(PathBuf);

impl EmbedScript {
    fn write() -> Result<Self> {
        // The pid alone is not unique enough: a crate's tests run as
        // threads of one process, so two scripts would share a path and
        // the first `Drop` would delete a file the other still needed.
        static NTH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nth = NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // `.mjs` so node reads it as a module from the extension alone,
        // with no flag that a forked grandchild could inherit.
        let path = std::env::temp_dir().join(format!(
            "datalib-qmd-embed-{}-{nth}.mjs",
            std::process::id()
        ));
        std::fs::write(&path, EMBED_NDJSON_MJS)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for EmbedScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The embedding pass.
///
/// Through qmd's SDK when the runtime is staged, so progress comes back
/// as NDJSON; through `qmd embed` when it is not. The `npx` fallback has
/// no importable package path (see
/// `datalib_runtime::node_runtime::staged_package`), and that is the
/// only way to be here without one.
fn run_embed(
    cache_home: &Path,
    qmd_dir: &Path,
    index_path: &Path,
    opts: &IndexOptions,
) -> Result<()> {
    let Some((node, pkg_dir)) = datalib_runtime::qmd::qmd_sdk_paths(&opts.qmd_version) else {
        status_line!(
            "[qmd-indexer] no staged qmd package — embedding through the CLI, \
             which reports no progress until it is done"
        );
        return run_qmd(cache_home, &opts.qmd_version, &["embed"]);
    };

    let script = EmbedScript::write()?;
    let mut cmd = std::process::Command::new(&node);
    // No node flags before the script. Anything here is inherited by
    // every process forked below us — see the script's header.
    cmd.arg(&script.0).arg(&pkg_dir).arg(index_path);
    // qmd writes this beside the index during `update`; it is where the
    // embedding model is pinned. Passing it keeps the SDK resolving the
    // same model the CLI would rather than falling back to qmd's default
    // and agreeing with us only by coincidence.
    let config = qmd_dir.join("index.yml");
    if config.is_file() {
        cmd.arg(&config);
    }
    cmd.env("XDG_CACHE_HOME", cache_home);
    cmd.env("XDG_CONFIG_HOME", cache_home);
    cmd.env("NO_COLOR", "1");
    cmd.stdout(std::process::Stdio::piped());
    status_line!(
        "[qmd-indexer] $ {}",
        datalib_runtime::node_runtime::display_command(&cmd)
    );

    // No `shared_multi().suspend(…)` here, unlike `run_qmd`. That
    // exists so a child printing to an inherited stdout doesn't scribble
    // over live bars — but this child's stdout is a pipe, and suspending
    // for the length of the embed would hide the bars for exactly the
    // stretch this progress is for.
    read_embed_events(&mut cmd, opts.on_embed_progress.as_deref())
}

/// Spawn the wrapper and drain its NDJSON until it exits.
///
/// stderr is left inherited — it is the step's log, and node's own
/// diagnostics belong there rather than in this parser.
fn read_embed_events(
    cmd: &mut std::process::Command,
    on_progress: Option<&(dyn Fn(EmbedProgress) + Send + Sync)>,
) -> Result<()> {
    use std::io::BufRead;

    let mut child = cmd
        .spawn()
        .with_context(|| "failed to spawn node; is the runtime staged?")?;
    let stdout = child.stdout.take().expect("stdout piped");

    let mut failure: Option<String> = None;
    let mut done: Option<String> = None;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.context("read from qmd embed")?;
        match parse_embed_event(&line) {
            Some(EmbedEvent::Progress(p)) => {
                if let Some(f) = on_progress {
                    f(p);
                }
            }
            Some(EmbedEvent::Done {
                docs_processed,
                chunks_embedded,
                errors,
            }) => {
                done = Some(format!(
                    "embedded {chunks_embedded} chunks from {docs_processed} documents\
                     {}",
                    if errors > 0 {
                        format!(", {errors} chunk(s) failed after retries")
                    } else {
                        String::new()
                    }
                ));
            }
            // The CLI prints "Skipping." here and exits 0, which leaves
            // a half-embedded index looking like a finished one. This
            // step owns the index, so a second embed means something
            // unexpected is writing it: say so and fail.
            Some(EmbedEvent::Busy) => {
                failure = Some(
                    "another process holds qmd's embed lock \
                     (.qmd-embed.lock beside the index); nothing else should be \
                     writing this index"
                        .to_string(),
                )
            }
            Some(EmbedEvent::Error(msg)) => failure = Some(msg),
            // Not ours: a dependency wrote to stdout. Say so rather than
            // dropping it, and don't let it fail the pass.
            None if !line.trim().is_empty() => status_line!("[qmd-indexer] qmd: {line}"),
            None => {}
        }
    }

    let status = child.wait().context("wait for qmd embed")?;
    if let Some(msg) = failure {
        bail!("qmd embed failed: {msg}");
    }
    // A wrapper that died without saying why — an OOM kill, a native
    // crash in node-llama-cpp — exits non-zero with no `error` line.
    // Leaving that as success would silently ship a half-embedded index.
    if !status.success() {
        bail!("qmd embed failed: {status}");
    }
    // Every branch of the wrapper ends in one of the three terminal
    // events, so a clean exit with none of them means the script did
    // not run — a broken invocation, which exits 0 and embeds nothing.
    let Some(done) = done else {
        bail!("qmd embed exited cleanly without reporting what it did");
    };
    status_line!("[qmd-indexer] {done}");
    Ok(())
}

fn run_qmd(cache_home: &Path, qmd_version: &str, args: &[&str]) -> Result<()> {
    let mut cmd = datalib_runtime::qmd::qmd_command(qmd_version)?;
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
    let status = match datalib_status_line::shared_multi() {
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
        // XDG_CACHE_HOME wins where it is set.
        assert_eq!(
            models_dir_under(Some("/x/cache".into()), Some("/home/u".into())),
            PathBuf::from("/x/cache/qmd/models")
        );

        // HOME fallback: $HOME/.cache/qmd/models.
        assert_eq!(
            models_dir_under(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.cache/qmd/models")
        );

        // Neither set — relative to wherever this is running.
        assert_eq!(
            models_dir_under(None, None),
            PathBuf::from("./.cache/qmd/models")
        );
    }

    /// The exact lines a real embed produced, pasted from a run of the
    /// wrapper against a scratch index. The field names are qmd's
    /// (`EmbedProgress` in `third-party/qmd/src/store.ts`), so a rename
    /// upstream has to fail here rather than silently zero the numbers.
    #[test]
    fn a_real_progress_line_parses_into_its_numbers() {
        let line = r#"{"event":"progress","chunksEmbedded":32,"totalChunks":60,"bytesProcessed":62171,"totalBytes":116328,"errors":0}"#;
        assert_eq!(
            parse_embed_event(line),
            Some(EmbedEvent::Progress(EmbedProgress {
                chunks_embedded: 32,
                total_chunks: 60,
                bytes_processed: 62171,
                total_bytes: 116328,
                errors: 0,
            }))
        );
    }

    #[test]
    fn the_terminal_events_parse() {
        let done = r#"{"event":"done","docsProcessed":5,"chunksEmbedded":60,"errors":2,"failures":[],"durationMs":25365}"#;
        assert_eq!(
            parse_embed_event(done),
            Some(EmbedEvent::Done {
                docs_processed: 5,
                chunks_embedded: 60,
                errors: 2,
            })
        );
        assert_eq!(
            parse_embed_event(r#"{"event":"busy"}"#),
            Some(EmbedEvent::Busy)
        );
        assert_eq!(
            parse_embed_event(r#"{"event":"error","message":"no such model"}"#),
            Some(EmbedEvent::Error("no such model".to_string()))
        );
    }

    /// Anything that isn't one of our events is `None`, so the caller
    /// logs it instead of failing an otherwise-good embed on it. A
    /// dependency writing a banner to stdout must not break indexing.
    #[test]
    fn foreign_output_is_not_an_event() {
        for line in [
            "",
            "   ",
            "Loading model...",
            r#"{"level":"warn","msg":"something else entirely"}"#,
            r#"{"event":"some_future_event","n":1}"#,
            "{not json at all",
        ] {
            assert_eq!(parse_embed_event(line), None, "line: {line:?}");
        }
    }

    /// A missing number reads as zero rather than dropping the whole
    /// event: a reading with one field absent is still a reading, and
    /// losing it would stall the bar until the next one.
    #[test]
    fn a_progress_line_missing_a_field_still_reports_the_rest() {
        assert_eq!(
            parse_embed_event(r#"{"event":"progress","bytesProcessed":10,"totalBytes":20}"#),
            Some(EmbedEvent::Progress(EmbedProgress {
                chunks_embedded: 0,
                total_chunks: 0,
                bytes_processed: 10,
                total_bytes: 20,
                errors: 0,
            }))
        );
    }

    /// The wrapper runs as a file, so its arguments start at argv[2] —
    /// argv[1] is the script itself.
    #[test]
    fn the_wrapper_reads_argv_the_way_a_script_file_gets_it() {
        assert!(
            EMBED_NDJSON_MJS.contains("process.argv.slice(2)"),
            "a script file's own path is argv[1], so its arguments start at 2"
        );
    }

    /// **No node flags before the script path.** node passes its own
    /// flags down to anything forked beneath it, dropping `-e` but
    /// keeping `--input-type`; node-llama-cpp probes its prebuilt by
    /// forking such a child, and on linux-x64 that child then fails to
    /// start. The whole embed comes back `NoBinaryFoundError` — green
    /// on a mac, red on CI, which is how this was found.
    #[test]
    fn nothing_we_pass_node_can_be_inherited_by_a_forked_grandchild() {
        let script = EmbedScript::write().unwrap();
        let mut cmd = std::process::Command::new("node");
        cmd.arg(&script.0).arg("pkg").arg("db");
        let first = cmd.get_args().next().unwrap();
        assert_eq!(
            first,
            script.0.as_os_str(),
            "the script must be node's first argument, with no flags in front of it"
        );
        assert!(
            script.0.extension().is_some_and(|e| e == "mjs"),
            "the file is read as a module by its extension, not by a flag"
        );
    }

    /// The script is a scratch file, and it does not belong in the data
    /// root: the step's tree is swept whole into the fixture's overlay
    /// tar, so one written there would be baked into the fixture.
    #[test]
    fn the_script_is_cleaned_up_and_lives_outside_any_data_root() {
        let path = {
            let script = EmbedScript::write().unwrap();
            assert!(script.0.is_file());
            assert!(script.0.starts_with(std::env::temp_dir()));
            script.0.clone()
        };
        assert!(!path.exists(), "the script should be gone once dropped");
    }

    /// Two live scripts must not share a path. They did while the name
    /// was the pid alone: `cargo`/bazel run a crate's tests as threads
    /// of **one** process, so every test that wrote a script wrote the
    /// same file, and the first one to drop deleted a file another was
    /// still asserting on. It fails as a flake somewhere else, which is
    /// the expensive kind.
    #[test]
    fn two_scripts_in_one_process_get_their_own_files() {
        let a = EmbedScript::write().unwrap();
        let b = EmbedScript::write().unwrap();
        assert_ne!(a.0, b.0, "two scripts collided on one path");
        assert!(a.0.is_file() && b.0.is_file());
        drop(a);
        assert!(b.0.is_file(), "dropping one script deleted the other's");
    }

    /// A stand-in for the wrapper: `sh` printing canned lines, then
    /// exiting with `code`. Lets the read loop be tested without node,
    /// qmd or a model — the loop is the part that decides whether a
    /// pass counted as success.
    fn fake_wrapper(lines: &str, code: i32) -> std::process::Command {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("printf '%s' \"$0\"; exit {code}"))
            .arg(lines)
            .stdout(std::process::Stdio::piped());
        cmd
    }

    /// Collects what the callback was handed, in order.
    fn drain(lines: &str, code: i32) -> (Result<()>, Vec<EmbedProgress>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = {
            let seen = seen.clone();
            move |p: EmbedProgress| seen.lock().unwrap().push(p)
        };
        let out = read_embed_events(&mut fake_wrapper(lines, code), Some(&sink));
        let seen = seen.lock().unwrap().clone();
        (out, seen)
    }

    #[test]
    fn every_progress_line_reaches_the_callback_in_order() {
        let (result, seen) = drain(
            "{\"event\":\"progress\",\"bytesProcessed\":10,\"totalBytes\":30}\n\
             {\"event\":\"progress\",\"bytesProcessed\":20,\"totalBytes\":30}\n\
             {\"event\":\"done\",\"docsProcessed\":2,\"chunksEmbedded\":5,\"errors\":0}\n",
            0,
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            seen.iter().map(|p| p.bytes_processed).collect::<Vec<_>>(),
            vec![10, 20]
        );
    }

    /// The regression this guards: a wrapper that exits 0 having
    /// embedded nothing must not read as a finished index. The old CLI
    /// path did exactly that when the embed lock was held.
    #[test]
    fn a_clean_exit_that_reported_nothing_is_a_failure() {
        let (result, _) = drain("", 0);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("without reporting what it did"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_busy_lock_fails_rather_than_claiming_the_index_is_built() {
        let (result, _) = drain("{\"event\":\"busy\"}\n", 75);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("embed lock"), "unexpected error: {err}");
    }

    /// The wrapper's own error message has to survive to the step's
    /// failure, not be replaced by "exit status: 1".
    #[test]
    fn the_wrappers_error_message_is_what_the_step_reports() {
        let (result, _) = drain("{\"event\":\"error\",\"message\":\"no such model\"}\n", 1);
        assert!(result.unwrap_err().to_string().contains("no such model"));
    }

    /// A crash with no error line — an OOM kill, a native fault in
    /// node-llama-cpp — still has to fail, on the exit code alone.
    #[test]
    fn a_silent_crash_after_progress_still_fails() {
        let (result, seen) = drain(
            "{\"event\":\"progress\",\"bytesProcessed\":10,\"totalBytes\":30}\n",
            1,
        );
        assert_eq!(seen.len(), 1, "the progress before the crash still counts");
        assert!(result.is_err());
    }

    /// Foreign stdout is logged, not fatal: a dependency's banner must
    /// not fail an embed that otherwise finished.
    #[test]
    fn chatter_on_stdout_does_not_fail_the_pass() {
        let (result, _) = drain(
            "Loading model...\n\
             {\"event\":\"done\",\"docsProcessed\":1,\"chunksEmbedded\":1,\"errors\":0}\n",
            0,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn models_present_requires_every_named_model_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let names = embed_model_names();
        assert_eq!(names, ["hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"]);

        // Nothing there yet → absent.
        assert!(!models_present(base, &names));

        // Every named model present + non-empty → present.
        for name in &names {
            std::fs::write(base.join(name), b"gguf").unwrap();
        }
        assert!(models_present(base, &names));

        // A zero-byte (partial/truncated) model doesn't count.
        std::fs::write(base.join(&names[0]), b"").unwrap();
        assert!(!models_present(base, &names));
    }
}
