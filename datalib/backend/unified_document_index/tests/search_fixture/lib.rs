//! The TNG fixture's data root for tests that search it: its rendered
//! documents, its grid index with the search index's tables, the vectors
//! its chunks were embedded as, and the Bazel-fetched embedding model, so
//! nothing reaches for the network. A test target that links this takes
//! `SEARCH_FIXTURE_DATA` and `SEARCH_FIXTURE_ENV` from `search_fixture.bzl`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A runfile named by an `$(rlocationpath …)` the BUILD file passed in.
fn runfile_from_env(var: &str) -> PathBuf {
    let rel = std::env::var(var)
        .unwrap_or_else(|_| panic!("{var} unset: is SEARCH_FIXTURE_ENV on the test rule?"));
    let r = runfiles::Runfiles::create().expect("runfiles");
    let p = r
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"));
    assert!(p.exists(), "{var}={rel} resolved to a missing path {p:?}");
    p
}

/// A file the fixture built, copied to `to` and made writable: the
/// runfiles tree is read-only, and SQLite opens a database read-write.
fn copy_writable(var: &str, to: &Path) {
    std::fs::create_dir_all(to.parent().expect("a file has a directory")).expect("mkdir");
    std::fs::copy(runfile_from_env(var), to).unwrap_or_else(|e| panic!("copy {var}: {e}"));
    let mut perms = std::fs::metadata(to).expect("stat").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(to, perms).expect("chmod");
}

/// The fixture's rendered documents, grid index, search index and
/// vectors, under `dst`, with the embedding model where a search of `dst`
/// looks for it (`datalib_qmd_models::models_dir_for`).
pub fn materialize_root(dst: &Path) {
    let status = Command::new("tar")
        .arg("-xf")
        .arg(runfile_from_env("SEARCH_FIXTURE_RENDERED_RLOC"))
        .arg("-C")
        .arg(dst)
        .arg("--strip-components=1")
        .status()
        .expect("spawn tar");
    assert!(
        status.success(),
        "extracting the rendered tree failed: {status}"
    );
    copy_writable(
        "SEARCH_FIXTURE_GRID_RLOC",
        &datalib_runtime::layout::grid_index_db(dst),
    );
    copy_writable(
        "SEARCH_FIXTURE_VECTORS_RLOC",
        &datalib_runtime::layout::inference_cache_db(dst),
    );
    let models = datalib_runtime::layout::unified_document_index_dir(dst).join("models");
    std::fs::create_dir_all(&models).expect("mkdir models");
    std::os::unix::fs::symlink(
        runfile_from_env("SEARCH_FIXTURE_EMBED_MODEL_RLOC"),
        models.join(datalib_runtime::qmd::EMBED_MODELS[0].cache_name()),
    )
    .expect("link the model");
}

/// The fixture's grid index as its sync left it, before any search
/// index step wrote into it: a root no `keyword_index` has run on.
pub fn copy_grid_index(dst: &Path) {
    copy_writable(
        "SEARCH_FIXTURE_PLAIN_GRID_RLOC",
        &datalib_runtime::layout::grid_index_db(dst),
    );
}
