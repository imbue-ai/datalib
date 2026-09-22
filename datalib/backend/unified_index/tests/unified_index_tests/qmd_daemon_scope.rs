//! The daemon's collection scoping, against the real qmd index the TNG
//! fixture builds and a real `qmd mcp` subprocess.
//!
//! Two claims, and neither is checkable without running qmd:
//!
//!  * an unscoped search still reaches every source, and
//!  * a search scoped to one group returns that group's documents and no
//!    others — with the scope applied *inside* retrieval, which is the
//!    whole reason per-source collections exist.
//!
//! It also pins the hit-path shape the applet joins on: what comes back
//! has to be the `<group>/render_markdown/…` string a grid row carries in
//! `qmd_path`, not qmd's collection-qualified display path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use datalib_unified_index::qmd::{CollectionScope, QmdDaemon, QmdDaemonConfig, QueryMode};

/// One of the fixture's groups, chosen because it has the most rendered
/// documents — so "scoped to it" and "everything" are clearly different
/// answers, and an accidental no-op scope would still show up.
const SCOPED_GROUP: &str = "slack";

/// A main-repo fixture, runfiles first (bazel test) then the `bazel-bin`
/// convenience symlink (plain `cargo test`) — the same two-path
/// resolution `qmd_index_state.rs` uses, and the same loud panic rather
/// than a silent skip.
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

/// A runfile named by an `$(rlocationpath …)` the BUILD file passed in.
/// Node and the qmd package store live in external repositories whose
/// names carry the host platform, so hard-coding either path here would
/// pass on one OS and fail on the other; letting bazel name them keeps
/// the mistake an analysis-time error instead.
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

fn materialize_root(dst: &Path) {
    for tar in ["ingested/qmd.tar", "ingested/qmd-index.tar"] {
        let status = Command::new("tar")
            .arg("-xf")
            .arg(fixture(&format!("tests/fixtures/{tar}")))
            .arg("-C")
            .arg(dst)
            .arg("--strip-components=1")
            .status()
            .expect("spawn tar");
        assert!(status.success(), "extracting {tar} failed: {status}");
    }
}

/// Build the `DATALIB_RUNTIME_DIR` tree `datalib_runtime::node_runtime`
/// resolves, so the daemon runs the Bazel-staged node and qmd rather
/// than reaching for `npx` and the network. Mirrors `_stage_runtime` in
/// `tests/fixtures/build_qmd_index.py`.
fn stage_runtime(work: &Path, qmd_version: &str) -> PathBuf {
    let runtime = work.join("runtime");
    let node_dir = runtime.join("node").join("bin");
    std::fs::create_dir_all(&node_dir).expect("mkdir node");
    let node = runfile_from_env("QMD_DAEMON_TEST_NODE_RLOC");
    std::os::unix::fs::symlink(node, node_dir.join("node")).expect("link node");

    // `qmd_package_dir` points inside the pnpm virtual store; the store
    // root is everything before the FIRST `/node_modules/`.
    let pkg = runfile_from_env("QMD_DAEMON_TEST_QMD_DIR_RLOC");
    let pkg_str = pkg.to_string_lossy().into_owned();
    let store = PathBuf::from(pkg_str.split("/node_modules/").next().expect("store root"))
        .join("node_modules");
    let staged = runtime.join("qmd").join(qmd_version);
    std::fs::create_dir_all(&staged).expect("mkdir qmd");
    std::os::unix::fs::symlink(store, staged.join("node_modules")).expect("link store");
    runtime
}

/// Point the index's `qmd/models` at the Bazel-fetched embedding model.
///
/// The daemon's hybrid search sends a `vec` sub-query, which has to embed
/// the query text. Left unstaged, qmd finds a model in the developer's
/// own `~/.cache/qmd/models` — and on a machine without one (every CI
/// container) reaches for the network instead. Measured: staged 2.1s,
/// unstaged 8.6s here, where the host cache is warm. The test does still
/// pass on the lexical half alone, so this is about being hermetic and
/// quick rather than about the assertions.
fn stage_models(root: &Path, work: &Path) {
    let models = work.join("models");
    std::fs::create_dir_all(&models).expect("mkdir models");
    let gguf = runfile_from_env("QMD_DAEMON_TEST_EMBED_MODEL_RLOC");
    std::os::unix::fs::symlink(
        gguf,
        models.join("hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"),
    )
    .expect("link model");
    let link = datalib_runtime::qmd::qmd_state_dir(root).join("models");
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&link);
    std::os::unix::fs::symlink(&models, &link).expect("link models dir");
}

/// The group a hit belongs to: the first segment of the path the daemon
/// resolved, which is what `grid_rows.qmd_path` is keyed on.
fn group_of(path: &str) -> &str {
    path.split('/').next().unwrap_or_default()
}

fn hits(daemon: &QmdDaemon, q: &str, scope: &CollectionScope) -> Vec<String> {
    daemon
        .search(QueryMode::Hybrid, q, 50, scope)
        .unwrap_or_else(|e| panic!("daemon search {q:?} scope {scope:?} failed: {e:#}"))
        .into_iter()
        .map(|h| h.path)
        .collect()
}

#[test]
fn daemon_search_is_unscoped_by_default_and_scopes_on_request() {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path();
    materialize_root(root);
    let work = td.path().join("_work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    stage_models(root, &work);
    let runtime = stage_runtime(&work, datalib_runtime::qmd::DEFAULT_QMD_VERSION);
    // SAFETY: the daemon reads this when it spawns, on this thread, and
    // no other test in this binary touches the environment.
    unsafe { std::env::set_var("DATALIB_RUNTIME_DIR", &runtime) };

    let daemon = QmdDaemon::new(QmdDaemonConfig::new(root.to_path_buf()));

    // A word the fixture's corpus uses across several sources, so the
    // unscoped answer genuinely spans collections.
    let query = "the enterprise";

    let all = hits(&daemon, query, &CollectionScope::All);
    assert!(!all.is_empty(), "unscoped search returned nothing");
    let all_groups: BTreeSet<&str> = all.iter().map(|p| group_of(p)).collect();
    assert!(
        all_groups.len() > 1,
        "unscoped search reached only {all_groups:?} — scoping has leaked into the default path"
    );

    // Every hit resolves to the shape `grid_rows.qmd_path` holds. A
    // collection-qualified path (`slack/slack/render_markdown/…`) would
    // pass the group check above and still join to no rows.
    for p in &all {
        assert!(
            p.contains("/render_markdown/"),
            "hit path {p:?} is not a `<group>/render_markdown/…` path"
        );
        assert_eq!(
            p.matches("/render_markdown/").count(),
            1,
            "hit path {p:?} looks collection-qualified — the prefix strip is wrong"
        );
    }

    let scoped = hits(
        &daemon,
        query,
        &CollectionScope::Only(vec![SCOPED_GROUP.to_string()]),
    );
    assert!(
        !scoped.is_empty(),
        "scoped search returned nothing; unscoped saw {all_groups:?}"
    );
    let scoped_groups: BTreeSet<&str> = scoped.iter().map(|p| group_of(p)).collect();
    assert_eq!(
        scoped_groups,
        BTreeSet::from([SCOPED_GROUP]),
        "scoped search leaked other sources"
    );
}

/// An empty scope means "no collection can match". qmd reads an empty
/// `collections` array as unscoped and would answer with the whole
/// corpus, so the daemon has to short-circuit instead of asking.
#[test]
fn an_empty_scope_asks_qmd_nothing() {
    // No runtime staged and no index needed: reaching qmd at all would
    // fail, so an Ok(empty) proves the request was never made.
    let td = tempfile::tempdir().expect("tempdir");
    let daemon = QmdDaemon::new(QmdDaemonConfig::new(td.path().to_path_buf()));
    let got = daemon
        .search(
            QueryMode::Hybrid,
            "anything",
            10,
            &CollectionScope::Only(Vec::new()),
        )
        .expect("an empty scope is answerable without qmd");
    assert!(got.is_empty());
}
