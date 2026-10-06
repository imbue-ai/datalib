//! The TNG fixture's data root for tests that search it with a real
//! `qmd mcp`: its rendered markdown, qmd index and grid index, and the
//! Bazel-staged node, qmd package and embedding model to run qmd with, so
//! nothing reaches for `npx` or the network. A test target that links this
//! takes `QMD_FIXTURE_DATA` and `QMD_FIXTURE_ENV` from `qmd_fixture.bzl`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `tests/fixtures/<rel>`, from runfiles.
pub fn fixture(rel: &str) -> PathBuf {
    let r = runfiles::Runfiles::create().expect("runfiles");
    let p = r
        .rlocation(format!("_main/tests/fixtures/{rel}"))
        .unwrap_or_else(|| panic!("fixture {rel} is not in runfiles"));
    assert!(p.exists(), "fixture {rel} resolved to a missing path {p:?}");
    p
}

/// A runfile named by an `$(rlocationpath …)` the BUILD file passed in.
/// Node and the qmd package store live in external repositories whose
/// names carry the host platform, so hard-coding either path here would
/// pass on one OS and fail on the other; letting bazel name them keeps
/// the mistake an analysis-time error instead.
fn runfile_from_env(var: &str) -> PathBuf {
    let rel = std::env::var(var)
        .unwrap_or_else(|_| panic!("{var} unset: is QMD_FIXTURE_ENV on the test rule?"));
    let r = runfiles::Runfiles::create().expect("runfiles");
    let p = r
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"));
    assert!(p.exists(), "{var}={rel} resolved to a missing path {p:?}");
    p
}

/// The fixture's rendered markdown and qmd index, under `dst`.
pub fn materialize_root(dst: &Path) {
    for tar in ["ingested/qmd.tar", "ingested/qmd-index.tar"] {
        let status = Command::new("tar")
            .arg("-xf")
            .arg(fixture(tar))
            .arg("-C")
            .arg(dst)
            .arg("--strip-components=1")
            .status()
            .expect("spawn tar");
        assert!(status.success(), "extracting {tar} failed: {status}");
    }
}

/// [`materialize_root`], plus the grid index the same sync built.
pub fn materialize_root_with_grid(dst: &Path) {
    materialize_root(dst);
    copy_grid_index(dst);
}

/// The fixture's grid index alone, where a root keeps it.
pub fn copy_grid_index(dst: &Path) {
    let db = datalib_runtime::layout::grid_index_db(dst);
    std::fs::create_dir_all(db.parent().expect("a grid index has a directory"))
        .expect("create grid dir");
    std::fs::copy(fixture("ingested/backend_index.doltlite_db"), &db).expect("copy grid index");
    // The fixture output is read-only in the runfiles tree; doltlite
    // wants to open it writable even though we only read.
    let mut perms = std::fs::metadata(&db).expect("stat").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&db, perms).expect("chmod");
}

/// The `DATALIB_RUNTIME_DIR` tree `datalib_runtime::node_runtime`
/// resolves, built under `work`, so qmd runs on the Bazel-staged node and
/// package. Mirrors `_stage_runtime` in `tests/fixtures/build_qmd_index.py`.
pub fn stage_runtime(work: &Path) -> PathBuf {
    let runtime = work.join("runtime");
    let node_dir = runtime.join("node").join("bin");
    std::fs::create_dir_all(&node_dir).expect("mkdir node");
    let node = runfile_from_env("QMD_FIXTURE_NODE_RLOC");
    std::os::unix::fs::symlink(node, node_dir.join("node")).expect("link node");

    // `qmd_package_dir` points inside the pnpm virtual store; the store
    // root is everything before the FIRST `/node_modules/`.
    let pkg = runfile_from_env("QMD_FIXTURE_QMD_DIR_RLOC");
    let pkg_str = pkg.to_string_lossy().into_owned();
    let store = PathBuf::from(pkg_str.split("/node_modules/").next().expect("store root"))
        .join("node_modules");
    let staged = runtime
        .join("qmd")
        .join(datalib_runtime::qmd::DEFAULT_QMD_VERSION);
    std::fs::create_dir_all(&staged).expect("mkdir qmd");
    std::os::unix::fs::symlink(store, staged.join("node_modules")).expect("link store");
    runtime
}

/// Point `root`'s `qmd/models` at the Bazel-fetched embedding model.
///
/// A hybrid search embeds the query text. Left unstaged, qmd finds a model
/// in the developer's own `~/.cache/qmd/models`, and on a machine without
/// one (every CI container) reaches for the network instead. Measured:
/// staged 2.1 s, unstaged 8.6 s with the host cache warm.
pub fn stage_models(root: &Path, work: &Path) {
    let models = work.join("models");
    std::fs::create_dir_all(&models).expect("mkdir models");
    let gguf = runfile_from_env("QMD_FIXTURE_EMBED_MODEL_RLOC");
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
