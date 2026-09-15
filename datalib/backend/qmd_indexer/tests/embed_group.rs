//! The two qmd behaviors `embed_group` reads off qmd's output, checked
//! against a real qmd: `qmd embed -c` is scoped to the collection, and
//! a second concurrent embed is refused with the exact wording the loop
//! looks for — and exits 0, which is why the loop looks.
//!
//! Runs the fixture's rendered markdown (`qmd_md.tar`) through the
//! Bazel-staged node, qmd tree and embedding model, so it needs neither
//! a host node nor the network.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use datalib_qmd_indexer::store::{embed_gauge, index_version, open_ro};
use datalib_qmd_indexer::{
    embed_group, prepare_store, run_index, EmbedGauge, EmbedOptions, EmbedProgress, IndexOptions,
    DEFAULT_QMD_VERSION,
};

fn fixture(rel: &str) -> PathBuf {
    if let Ok(r) = runfiles::Runfiles::create() {
        if let Some(c) = r.rlocation(format!("_main/{rel}")) {
            if c.exists() {
                return c;
            }
        }
    }
    let cargo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    cargo_root
        .join(format!("../../../bazel-bin/{rel}"))
        .canonicalize()
        .unwrap_or_else(|_| panic!("fixture {rel} not found; build it with bazel first"))
}

fn runfile_from_env(var: &str) -> PathBuf {
    let rel = std::env::var(var)
        .unwrap_or_else(|_| panic!("{var} unset — did the data dep drop out of the BUILD rule?"));
    let r = runfiles::Runfiles::create().expect("runfiles");
    let p = r
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"));
    assert!(p.exists(), "{var}={rel} resolved to a missing path {p:?}");
    p
}

/// A data root with every group's rendered markdown and nothing else,
/// the qmd runtime staged beside it, and the store's `models` link at
/// the Bazel-fetched embedding model.
struct Root {
    _dir: tempfile::TempDir,
    root: PathBuf,
    models: PathBuf,
}

impl Root {
    fn materialize() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let status = Command::new("tar")
            .arg("-xf")
            .arg(fixture("tests/fixtures/ingested/qmd_md.tar"))
            .arg("-C")
            .arg(&root)
            .arg("--strip-components=1")
            .status()
            .expect("spawn tar");
        assert!(status.success(), "extracting qmd_md.tar failed: {status}");

        stage_runtime_once();

        let models = dir.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        std::os::unix::fs::symlink(
            runfile_from_env("QMD_TEST_EMBED_MODEL_RLOC"),
            models.join("hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"),
        )
        .expect("link model");
        prepare_store(&root, &models).expect("prepare store");
        Self {
            _dir: dir,
            root,
            models,
        }
    }

    fn qmd(&self, args: &[&str]) -> String {
        let mut cmd = datalib_runtime::qmd::qmd_command(DEFAULT_QMD_VERSION);
        cmd.args(args);
        let cache_home = datalib_runtime::qmd::qmd_cache_home(&self.root);
        cmd.env("XDG_CACHE_HOME", &cache_home);
        cmd.env("XDG_CONFIG_HOME", &cache_home);
        cmd.env("NO_COLOR", "1");
        let out = cmd.output().expect("spawn qmd");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "qmd {args:?} failed: {text}");
        text
    }

    /// `qmd collection add` + `qmd update` over these groups, embedding
    /// nothing.
    fn index(&self, groups: &[&str]) {
        let mut opts = IndexOptions::new(&self.root);
        opts.groups = groups.iter().map(|g| g.to_string()).collect();
        opts.embed = false;
        opts.pull = false;
        opts.models_dir = self.models.clone();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(run_index(&opts))
            .unwrap_or_else(|e| panic!("run_index({groups:?}) failed: {e:#}"));
    }

    fn gauge(&self, group: &str) -> EmbedGauge {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = open_ro(&datalib_runtime::qmd::qmd_index_path(&self.root))
                .await
                .unwrap();
            let g = embed_gauge(&pool, group).await.unwrap();
            pool.close().await;
            g
        })
    }

    fn version(&self) -> String {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = open_ro(&datalib_runtime::qmd::qmd_index_path(&self.root))
                .await
                .unwrap();
            let v = index_version(&pool, DEFAULT_QMD_VERSION).await.unwrap();
            pool.close().await;
            v
        })
    }

    fn embed_opts(&self, group: &str) -> EmbedOptions {
        EmbedOptions {
            root: self.root.clone(),
            group: group.to_string(),
            qmd_version: DEFAULT_QMD_VERSION.to_string(),
            budget: None,
            pull_if_missing: false,
            models_dir: self.models.clone(),
        }
    }
}

/// The staged runtime is process-wide (`DATALIB_RUNTIME_DIR` is one
/// environment variable), so it is built once, in a directory that
/// lives as long as the process, however many tests share it.
fn stage_runtime_once() {
    static STAGED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    STAGED.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = stage_runtime(dir.path(), DEFAULT_QMD_VERSION);
        std::mem::forget(dir);
        // SAFETY: set once, before any test spawns a child; later
        // callers only read the initialized value.
        unsafe { std::env::set_var("DATALIB_RUNTIME_DIR", &runtime) };
        runtime
    });
}

/// Mirrors `_stage_runtime` in `tests/fixtures/build_qmd_index.py`.
fn stage_runtime(work: &Path, qmd_version: &str) -> PathBuf {
    let runtime = work.join("runtime");
    let node_dir = runtime.join("node").join("bin");
    std::fs::create_dir_all(&node_dir).expect("mkdir node");
    std::os::unix::fs::symlink(
        runfile_from_env("QMD_TEST_NODE_RLOC"),
        node_dir.join("node"),
    )
    .expect("link node");
    let pkg = runfile_from_env("QMD_TEST_QMD_DIR_RLOC");
    let pkg_str = pkg.to_string_lossy().into_owned();
    let store = PathBuf::from(pkg_str.split("/node_modules/").next().expect("store root"))
        .join("node_modules");
    let staged = runtime.join("qmd").join(qmd_version);
    std::fs::create_dir_all(&staged).expect("mkdir qmd");
    std::os::unix::fs::symlink(store, staged.join("node_modules")).expect("link store");
    runtime
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
    out
}

/// The index's content version is a function of what is indexed: the
/// same after a second pass over unchanged trees, different after an
/// edit — which is what makes a `qmd_embed` step stale exactly when
/// there could be something to embed.
#[test]
fn the_index_version_moves_with_the_documents_and_only_then() {
    let r = Root::materialize();
    r.index(&["yolink", "slack"]);
    let before = r.version();
    r.index(&["yolink", "slack"]);
    assert_eq!(
        r.version(),
        before,
        "a pass over unchanged trees must not move the version"
    );

    let file = walk(&r.root.join("yolink/render_markdown"))[0].clone();
    let mut body = std::fs::read_to_string(&file).unwrap();
    body.push_str("\n\nQuiddleworth zebrafish, appended by the test.\n");
    std::fs::write(&file, body).unwrap();
    r.index(&["yolink", "slack"]);
    let after = r.version();
    assert_ne!(after, before, "an edited document must move the version");
    assert_ne!(
        index_version_with_pin(&r, "9.9.9"),
        after,
        "a qmd bump must move the version even with nothing edited"
    );
}

fn index_version_with_pin(r: &Root, pin: &str) -> String {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let pool = open_ro(&datalib_runtime::qmd::qmd_index_path(&r.root))
            .await
            .unwrap();
        let v = index_version(&pool, pin).await.unwrap();
        pool.close().await;
        v
    })
}

struct Gauges(Mutex<Vec<EmbedGauge>>);
impl EmbedProgress for Gauges {
    fn gauge(&self, g: &EmbedGauge) {
        self.0.lock().unwrap().push(*g);
    }
}

/// `embed_group` embeds one collection and no other, reports the
/// collection complete, and is a no-op the second time. Two
/// simultaneous `qmd embed`s make the loser print the exact line the
/// loop reads — pinned here so a qmd bump that rewords it fails a test
/// instead of turning a skipped embed into a reported success.
#[test]
fn embed_is_scoped_serialized_and_driven_to_completion() {
    let r = Root::materialize();
    r.index(&["slack", "yolink"]);
    assert_eq!(r.gauge("slack").embedded(), 0);

    let gauges = Gauges(Mutex::new(Vec::new()));
    let out = embed_group(&r.embed_opts("yolink"), &gauges).expect("embed yolink");
    assert!(out.complete, "{out:?}");
    assert_eq!(out.sessions, 1, "{out:?}");
    assert_eq!(out.gauge.pending, 0, "{out:?}");
    assert_eq!(out.gauge.embedded(), out.gauge.active, "{out:?}");
    assert!(out.gauge.active >= 2, "{out:?}");
    let seen = gauges.0.lock().unwrap();
    assert!(
        seen.first().is_some_and(|g| g.pending == g.active)
            && seen.last().is_some_and(|g| g.pending == 0),
        "gauge readings should run from all-pending to none: {seen:?}"
    );
    drop(seen);

    // Scoped: slack was not touched.
    let slack = r.gauge("slack");
    assert_eq!(slack.embedded(), 0, "{slack:?}");
    assert_eq!(slack.pending, slack.active);

    // Idempotent, and cheap: no session runs when nothing is pending.
    let again = embed_group(
        &r.embed_opts("yolink"),
        &datalib_qmd_indexer::NoEmbedProgress,
    )
    .expect("embed yolink again");
    assert!(again.complete && again.sessions == 0, "{again:?}");

    // qmd's own refusal, verbatim. qmd honours its lock file only while
    // the pid in it is alive and looks like a qmd command line, so the
    // holder is a real process whose argv ends in `qmd`.
    let lock = datalib_runtime::qmd::qmd_state_dir(&r.root).join(".qmd-embed.lock");
    let holder = LockHolder::start(&lock);
    let refused = r.qmd(&["embed", "-c", "slack"]);
    assert!(
        refused.contains(datalib_qmd_indexer::embed::QMD_EMBED_BUSY),
        "qmd no longer prints the busy line embed_group reads:\n{refused}"
    );
    assert_eq!(
        r.gauge("slack").embedded(),
        0,
        "the refused embed must have done nothing"
    );

    // And the loop sees through that exit 0.
    let err = embed_group(
        &r.embed_opts("slack"),
        &datalib_qmd_indexer::NoEmbedProgress,
    )
    .expect_err("a refused embed must not read as success");
    assert!(format!("{err:#}").contains("did nothing"), "{err:#}");
    drop(holder);

    // With the holder gone the same call embeds the collection.
    let out = embed_group(
        &r.embed_opts("slack"),
        &datalib_qmd_indexer::NoEmbedProgress,
    )
    .expect("embed slack");
    assert!(
        out.complete && out.sessions == 1 && out.gauge.pending == 0,
        "{out:?}"
    );
}

/// A process qmd takes for a live embed: the staged node idling with an
/// argv that ends in `qmd`, its pid written where qmd looks.
struct LockHolder {
    child: std::process::Child,
    lock: PathBuf,
}

impl LockHolder {
    fn start(lock: &Path) -> Self {
        let child = Command::new(runfile_from_env("QMD_TEST_NODE_RLOC"))
            .args(["-e", "setTimeout(() => {}, 120000)", "qmd"])
            .spawn()
            .expect("spawn node");
        std::fs::write(lock, format!("{}\n", child.id())).unwrap();
        Self {
            child,
            lock: lock.to_path_buf(),
        }
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.lock);
    }
}
