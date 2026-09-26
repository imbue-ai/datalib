//! Drive `qmd` to build its keyword and embedding index over the rendered
//! markdown under a data root, one collection per group, in three
//! separable operations: register the collections, keyword-index some,
//! embed some.
//!
//! All three run a small script of ours against qmd's SDK and read its
//! NDJSON back (`src/js/qmd_sdk.mjs`): the CLI cannot scope a keyword
//! update to one collection, and reports embedding progress only to a
//! terminal. Retiring a collection is the one CLI call left.
//!
//! What qmd actually does, measured — where its CLI and its SDK differ,
//! and which of its operations may overlap — is
//! `docs/dev/qmd_behaviour.md`. Read it before changing how qmd is driven.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use datalib_status_line::status_line;
use serde_json::Value;

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
/// For a caller with no config to read — the standalone CLI. The steps
/// take the graph's own list instead, which omits a directory left behind
/// by a source that has since been removed from the config.
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

/// How far along a keyword update is: files looked at, of those found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateProgress {
    pub current: u64,
    pub total: u64,
}

/// A missing number reads as zero rather than dropping the whole reading:
/// a reading with one field absent is still a reading, and losing it
/// would stall the bar until the next one.
fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

impl EmbedProgress {
    /// From the script's `progress` line, whose field names are qmd's, so
    /// a rename upstream has to fail a test here rather than silently zero
    /// the numbers.
    pub fn from_json(v: &Value) -> Self {
        Self {
            chunks_embedded: n(v, "chunksEmbedded"),
            total_chunks: n(v, "totalChunks"),
            bytes_processed: n(v, "bytesProcessed"),
            total_bytes: n(v, "totalBytes"),
            errors: n(v, "errors"),
        }
    }
}

impl UpdateProgress {
    pub fn from_json(v: &Value) -> Self {
        Self {
            current: n(v, "current"),
            total: n(v, "total"),
        }
    }
}

/// One line of the script's output. `Progress` and `Done` carry the
/// line itself: which fields they hold depends on the verb.
#[derive(Debug, Clone, PartialEq)]
pub enum SdkEvent {
    Progress(Value),
    Done(Value),
    /// Another process holds qmd's embed lock, so this pass embedded
    /// nothing.
    Busy,
    Error(String),
}

/// Parse one line of the script's output.
///
/// `None` for anything that is not one of our events — a blank line, or
/// something a dependency printed to stdout. The caller logs those
/// rather than failing on them: a chatty transitive package must not be
/// able to fail a pass that otherwise worked.
pub fn parse_event(line: &str) -> Option<SdkEvent> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    match v.get("event")?.as_str()? {
        "progress" => Some(SdkEvent::Progress(v)),
        "done" => Some(SdkEvent::Done(v)),
        "busy" => Some(SdkEvent::Busy),
        "error" => Some(SdkEvent::Error(
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or("qmd failed without a message")
                .to_string(),
        )),
        _ => None,
    }
}

/// Called with each reading as a pass reports it.
pub type OnEmbedProgress = Arc<dyn Fn(EmbedProgress) + Send + Sync>;
pub type OnUpdateProgress = Arc<dyn Fn(UpdateProgress) + Send + Sync>;

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

/// A data root's qmd index, resolved: where it is, and which qmd drives it.
///
/// qmd writes `<XDG_CACHE_HOME>/qmd/index.sqlite`, and is pointed at
/// `unified_index/qmd_index` so the index lives in that step's tree. The
/// per-source steps write into the same file, one collection each.
#[derive(Debug, Clone)]
pub struct Index {
    root: PathBuf,
    cache_home: PathBuf,
    qmd_dir: PathBuf,
    index_path: PathBuf,
    qmd_version: String,
}

impl Index {
    /// Resolve qmd (fetching the runtime on a miss) and make the index's
    /// directory. Opens nothing: the first SDK call creates the file.
    pub fn at(root: &Path, qmd_version: &str) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("root does not exist: {}", root.display()))?;
        let cache_home = datalib_runtime::qmd::qmd_cache_home(&root);
        let qmd_dir = datalib_runtime::qmd::qmd_state_dir(&root);
        std::fs::create_dir_all(&qmd_dir)
            .with_context(|| format!("failed to create {}", qmd_dir.display()))?;
        let probe = datalib_runtime::qmd::qmd_command(qmd_version)?;
        status_line!(
            "[qmd-indexer] qmd package = @tobilu/qmd@{qmd_version} ({})",
            if datalib_runtime::node_runtime::is_bundled(&probe) {
                "bundled runtime"
            } else {
                "via npx"
            }
        );
        Ok(Self {
            index_path: qmd_dir.join("index.sqlite"),
            root,
            cache_home,
            qmd_dir,
            qmd_version: qmd_version.to_string(),
        })
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// Point `<index>/models` at `models_dir`, where qmd then finds the
    /// GGUFs the caller provisioned.
    pub fn link_models(&self, models_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(models_dir)
            .with_context(|| format!("failed to create models dir {}", models_dir.display()))?;
        ensure_models_symlink(&self.qmd_dir, models_dir)
    }

    /// `index.yml`, the root, and each group's name and glob: what
    /// registering takes.
    fn collection_args(&self, groups: &[String]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            self.qmd_dir.join("index.yml").into(),
            self.root.clone().into(),
        ];
        for group in groups {
            args.push(group.into());
            args.push(mask_for_group(group).into());
        }
        args
    }

    /// Register a collection for each of `groups` and unregister each of
    /// `retire`, leaving the index's registry — and `index.yml` beside it,
    /// which `qmd mcp` reconciles that registry against — naming exactly
    /// the collections the caller wants. Indexes nothing.
    pub fn register(&self, groups: &[String], retire: &[String]) -> Result<()> {
        status_line!("[qmd-indexer] collections = {}", groups.join(", "));
        self.run_sdk("register", &self.collection_args(groups), &mut |_| {})?;

        // After registering, not before: `qmd collection remove` deletes
        // the retired collection's documents and then every content row no
        // remaining document references. Measured migrating off `mirror`,
        // with the per-group collections already indexed the content
        // survives; retired first, the index is left with no bodies.
        for name in retire {
            retire_collection(&self.cache_home, &self.qmd_version, name)?;
        }
        Ok(())
    }

    /// Register each of `groups` (a no-op for one already registered) and
    /// bring its keyword index in line with its rendered tree. qmd hashes
    /// every file and skips the unchanged.
    pub fn keyword_index(
        &self,
        groups: &[String],
        on_progress: Option<&(dyn Fn(UpdateProgress) + Send + Sync)>,
    ) -> Result<String> {
        let done = self.run_sdk("update", &self.collection_args(groups), &mut |v| {
            if let Some(f) = on_progress {
                f(UpdateProgress::from_json(v));
            }
        })?;
        Ok(format!(
            "{} new, {} updated, {} unchanged, {} removed{}",
            n(&done, "indexed"),
            n(&done, "updated"),
            n(&done, "unchanged"),
            n(&done, "removed"),
            match n(&done, "skipped") {
                0 => String::new(),
                s => format!(", {s} unreadable"),
            }
        ))
    }

    /// Embed what `group`'s collection is missing — every collection's
    /// with `None`. One process for all of them pays the model load once.
    pub fn embed(
        &self,
        group: Option<&str>,
        on_progress: Option<&(dyn Fn(EmbedProgress) + Send + Sync)>,
    ) -> Result<String> {
        // Through the link, not a models dir argument: a root whose link
        // was made earlier (the test fixture's, pointing at bazel outputs)
        // reads its models from wherever that link goes.
        let models_link = self.qmd_dir.join("models");
        if !models_present(&models_link, &embed_model_names()) {
            bail!(
                "embedding model missing from {} — expected {}; provision it \
                 (`datalib-step pull-models`) and link it (`link_models`) first",
                models_link.display(),
                embed_model_names().join(", ")
            );
        }
        let args: Vec<OsString> = group.map(OsString::from).into_iter().collect();
        let done = self.run_sdk("embed", &args, &mut |v| {
            if let Some(f) = on_progress {
                f(EmbedProgress::from_json(v));
            }
        })?;
        Ok(format!(
            "embedded {} chunks from {} documents{}",
            n(&done, "chunksEmbedded"),
            n(&done, "docsProcessed"),
            match n(&done, "errors") {
                0 => String::new(),
                e => format!(", {e} chunk(s) failed after retries"),
            }
        ))
    }

    /// `qmd status`, as text: qmd has no `--json` for it.
    pub fn status(&self) -> Result<String> {
        let mut cmd = datalib_runtime::qmd::qmd_command(&self.qmd_version)?;
        cmd.arg("status");
        cmd.env("XDG_CACHE_HOME", &self.cache_home);
        cmd.env("XDG_CONFIG_HOME", &self.cache_home);
        cmd.env("NO_COLOR", "1");
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

    /// Run one verb of the script and return its `done` line.
    fn run_sdk(
        &self,
        verb: &str,
        args: &[OsString],
        on_progress: &mut dyn FnMut(&Value),
    ) -> Result<Value> {
        let Some((node, pkg_dir)) = datalib_runtime::qmd::qmd_sdk_paths(&self.qmd_version) else {
            bail!(
                "qmd's SDK needs the staged runtime, and qmd resolved through npx, which has no \
                 package path to import from; stage it with `scripts/stage_runtime.sh`"
            );
        };
        let script = SdkScript::write()?;
        let mut cmd = std::process::Command::new(&node);
        // No node flags before the script. Anything here is inherited by
        // every process forked below us — see the script's header.
        cmd.arg(&script.0)
            .arg(&pkg_dir)
            .arg(&self.index_path)
            .arg(verb)
            .args(args);
        cmd.env("XDG_CACHE_HOME", &self.cache_home);
        cmd.env("XDG_CONFIG_HOME", &self.cache_home);
        cmd.env("NO_COLOR", "1");
        cmd.stdout(std::process::Stdio::piped());
        status_line!(
            "[qmd-indexer] $ {}",
            datalib_runtime::node_runtime::display_command(&cmd)
        );
        let done = read_events(&mut cmd, on_progress).with_context(|| format!("qmd {verb}"))?;
        status_line!("[qmd-indexer] {verb}: {done}");
        Ok(done)
    }
}

/// Options for [`run_index`], the whole index in one call. Construct with
/// `IndexOptions::new(root)` and override fields as needed.
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
    /// Collections to unregister once the rest are registered.
    pub retire_collections: Vec<String>,
    /// Where the GGUF models already are. The indexer never fetches
    /// one: `qmd pull` compares an etag against HuggingFace `main` and
    /// re-downloads on any difference, which is how a re-upload upstream
    /// would silently change every embedding. The caller provisions the
    /// pinned, sha256-verified files (`datalib_qmd_models`) before this
    /// runs, and qmd finds them in place.
    pub models_dir: PathBuf,
    /// Where to report the embedding pass's progress.
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

/// Result of a `run_index` pass. `status_output` is the raw stdout of
/// `qmd status`, and `None` if capturing it failed — indexing success
/// doesn't depend on it.
#[derive(Debug, Clone)]
pub struct IndexOutcome {
    pub index_path: PathBuf,
    pub status_output: Option<String>,
}

/// Register, keyword-index and (with `embed`) embed every group in one
/// call: what the steps do one source at a time, for a caller with no
/// runner — the standalone CLI, and through it the test fixture.
pub fn run_index(opts: &IndexOptions) -> Result<IndexOutcome> {
    let index = Index::at(&opts.root, &opts.qmd_version)?;
    index.link_models(&opts.models_dir)?;
    index.register(&opts.groups, &opts.retire_collections)?;
    if !opts.groups.is_empty() {
        index.keyword_index(&opts.groups, None)?;
    }
    if opts.embed {
        index.embed(None, opts.on_embed_progress.as_deref())?;
    }
    if !index.index_path().exists() {
        bail!(
            "qmd reported success but index.sqlite is missing at {}",
            index.index_path().display()
        );
    }
    let status_output = match index.status() {
        Ok(s) => Some(s),
        Err(e) => {
            status_line!("[qmd-indexer] qmd status capture failed (non-fatal): {e:#}");
            None
        }
    };
    Ok(IndexOutcome {
        index_path: index.index_path().to_path_buf(),
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
/// is not an option, and [`SdkScript`] for why the file is not in the
/// data root.
const QMD_SDK_MJS: &str = include_str!("js/qmd_sdk.mjs");

/// The script on disk, removed when this is dropped.
///
/// Outside the data root deliberately. The index's tree is swept whole
/// into the test fixture's overlay tar (`tests/fixtures/build_qmd_index.py`),
/// so a scratch file written beside the index would be baked into the
/// fixture.
struct SdkScript(PathBuf);

impl SdkScript {
    fn write() -> Result<Self> {
        // The pid alone is not unique enough: a crate's tests run as
        // threads of one process, so two scripts would share a path and
        // the first `Drop` would delete a file the other still needed.
        static NTH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nth = NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // `.mjs` so node reads it as a module from the extension alone,
        // with no flag that a forked grandchild could inherit.
        let path =
            std::env::temp_dir().join(format!("datalib-qmd-sdk-{}-{nth}.mjs", std::process::id()));
        std::fs::write(&path, QMD_SDK_MJS)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for SdkScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Spawn the script and drain its NDJSON until it exits; its `done` line
/// is the answer.
///
/// stderr is left inherited — it is the step's log, and node's own
/// diagnostics belong there rather than in this parser. No
/// `shared_multi().suspend(…)` either: that is for a child printing to
/// an inherited stdout, and this child's stdout is a pipe.
fn read_events(
    cmd: &mut std::process::Command,
    on_progress: &mut dyn FnMut(&Value),
) -> Result<Value> {
    use std::io::BufRead;

    let mut child = cmd
        .spawn()
        .with_context(|| "failed to spawn node; is the runtime staged?")?;
    let stdout = child.stdout.take().expect("stdout piped");

    let mut failure: Option<String> = None;
    let mut done: Option<Value> = None;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.context("read from qmd")?;
        match parse_event(&line) {
            Some(SdkEvent::Progress(v)) => on_progress(&v),
            Some(SdkEvent::Done(v)) => done = Some(v),
            // The CLI prints "Skipping." here and exits 0, which leaves
            // a half-embedded index looking like a finished one. The
            // runner keeps embeds apart, so a second one means something
            // outside it is writing this index: say so and fail.
            Some(SdkEvent::Busy) => {
                failure = Some(
                    "another process holds qmd's embed lock \
                     (.qmd-embed.lock beside the index); nothing else should be \
                     writing this index"
                        .to_string(),
                )
            }
            Some(SdkEvent::Error(msg)) => failure = Some(msg),
            // Not ours: a dependency wrote to stdout. Say so rather than
            // dropping it, and don't let it fail the pass.
            None if !line.trim().is_empty() => status_line!("[qmd-indexer] qmd: {line}"),
            None => {}
        }
    }

    let status = child.wait().context("wait for qmd")?;
    if let Some(msg) = failure {
        bail!("{msg}");
    }
    // A script that died without saying why — an OOM kill, a native
    // crash in node-llama-cpp — exits non-zero with no `error` line.
    // Leaving that as success would silently ship a half-built index.
    if !status.success() {
        bail!("exited {status}");
    }
    // Every branch of the script ends in one of the three terminal
    // events, so a clean exit with none of them means the script did
    // not run — a broken invocation, which exits 0 and does nothing.
    done.context("exited cleanly without reporting what it did")
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

    fn progress(line: &str) -> Value {
        match parse_event(line) {
            Some(SdkEvent::Progress(v)) => v,
            other => panic!("not a progress line: {other:?}"),
        }
    }

    /// The exact lines a real embed produced, pasted from a run of the
    /// script against a scratch index. The field names are qmd's
    /// (`EmbedProgress` in `third-party/qmd/src/store.ts`), so a rename
    /// upstream has to fail here rather than silently zero the numbers.
    #[test]
    fn a_real_embed_progress_line_parses_into_its_numbers() {
        let line = r#"{"event":"progress","chunksEmbedded":32,"totalChunks":60,"bytesProcessed":62171,"totalBytes":116328,"errors":0}"#;
        assert_eq!(
            EmbedProgress::from_json(&progress(line)),
            EmbedProgress {
                chunks_embedded: 32,
                total_chunks: 60,
                bytes_processed: 62171,
                total_bytes: 116328,
                errors: 0,
            }
        );
    }

    /// `update`'s `onProgress` hands over the collection, the file and a
    /// position (`ReindexProgress` in `third-party/qmd/src/store.ts`).
    #[test]
    fn an_update_progress_line_parses_into_its_position() {
        let line = r#"{"event":"progress","current":3,"total":13}"#;
        assert_eq!(
            UpdateProgress::from_json(&progress(line)),
            UpdateProgress {
                current: 3,
                total: 13
            }
        );
    }

    #[test]
    fn the_terminal_events_parse() {
        let done = r#"{"event":"done","docsProcessed":5,"chunksEmbedded":60,"errors":2}"#;
        assert!(matches!(parse_event(done), Some(SdkEvent::Done(v)) if n(&v, "errors") == 2));
        assert_eq!(parse_event(r#"{"event":"busy"}"#), Some(SdkEvent::Busy));
        assert_eq!(
            parse_event(r#"{"event":"error","message":"no such model"}"#),
            Some(SdkEvent::Error("no such model".to_string()))
        );
    }

    /// Anything that isn't one of our events is `None`, so the caller
    /// logs it instead of failing an otherwise-good pass on it. A
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
            assert_eq!(parse_event(line), None, "line: {line:?}");
        }
    }

    #[test]
    fn a_progress_line_missing_a_field_still_reports_the_rest() {
        assert_eq!(
            EmbedProgress::from_json(&progress(
                r#"{"event":"progress","bytesProcessed":10,"totalBytes":20}"#
            )),
            EmbedProgress {
                chunks_embedded: 0,
                total_chunks: 0,
                bytes_processed: 10,
                total_bytes: 20,
                errors: 0,
            }
        );
    }

    /// The script runs as a file, so its arguments start at argv[2] —
    /// argv[1] is the script itself.
    #[test]
    fn the_script_reads_argv_the_way_a_script_file_gets_it() {
        assert!(
            QMD_SDK_MJS.contains("process.argv.slice(2)"),
            "a script file's own path is argv[1], so its arguments start at 2"
        );
    }

    /// The regression #617 was: qmd's 30-minute cap ended a long first
    /// embed early and reported it finished. The SDK's `store.embed()`
    /// drops `maxDurationMs`, so the script has to call past it.
    #[test]
    fn the_embed_turns_off_qmds_time_cap() {
        assert!(
            QMD_SDK_MJS.contains("generateEmbeddings(store.internal")
                && QMD_SDK_MJS.contains("maxDurationMs: 0"),
            "the embed must go through generateEmbeddings with the cap off"
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
        let script = SdkScript::write().unwrap();
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
    /// root: the index's tree is swept whole into the fixture's overlay
    /// tar, so one written there would be baked into the fixture.
    #[test]
    fn the_script_is_cleaned_up_and_lives_outside_any_data_root() {
        let path = {
            let script = SdkScript::write().unwrap();
            assert!(script.0.is_file());
            assert!(script.0.starts_with(std::env::temp_dir()));
            script.0.clone()
        };
        assert!(!path.exists(), "the script should be gone once dropped");
    }

    /// Two live scripts must not share a path. They did while the name
    /// was the pid alone: a crate's tests run as threads of **one**
    /// process, so the first one to drop deleted a file another was
    /// still asserting on.
    #[test]
    fn two_scripts_in_one_process_get_their_own_files() {
        let a = SdkScript::write().unwrap();
        let b = SdkScript::write().unwrap();
        assert_ne!(a.0, b.0, "two scripts collided on one path");
        assert!(a.0.is_file() && b.0.is_file());
        drop(a);
        assert!(b.0.is_file(), "dropping one script deleted the other's");
    }

    /// A stand-in for the script: `sh` printing canned lines, then
    /// exiting with `code`. Lets the read loop be tested without node,
    /// qmd or a model — the loop is the part that decides whether a
    /// pass counted as success.
    fn fake_script(lines: &str, code: i32) -> std::process::Command {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("printf '%s' \"$0\"; exit {code}"))
            .arg(lines)
            .stdout(std::process::Stdio::piped());
        cmd
    }

    /// Collects the `bytesProcessed` of each progress line, in order.
    fn drain(lines: &str, code: i32) -> (Result<Value>, Vec<u64>) {
        let mut seen = Vec::new();
        let out = read_events(&mut fake_script(lines, code), &mut |v| {
            seen.push(n(v, "bytesProcessed"))
        });
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
        assert_eq!(n(&result.unwrap(), "chunksEmbedded"), 5);
        assert_eq!(seen, vec![10, 20]);
    }

    /// The regression this guards: a script that exits 0 having done
    /// nothing must not read as a finished index. The old CLI path did
    /// exactly that when the embed lock was held.
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

    /// The script's own error message has to survive to the step's
    /// failure, not be replaced by "exit status: 1" — an unregistered
    /// collection most of all, since that is how a missing fan-in shows.
    #[test]
    fn the_scripts_error_message_is_what_the_step_reports() {
        let (result, _) = drain(
            "{\"event\":\"error\",\"message\":\"collection \\\"a\\\" is not registered\"}\n",
            1,
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("is not registered"));
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
    /// not fail a pass that otherwise finished.
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
