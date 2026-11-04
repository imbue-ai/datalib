//! `Index` against the real qmd: the pinned package, run by the pinned
//! node, over a few documents a test writes. Everything but the embed
//! tests runs without a model and takes well under a second.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use datalib_qmd_indexer::{
    Collection, Embedded, Index, Qmd, QueryEmbedder, UpdateProgress, Updated, VerifiedModels,
};

fn runfile(var: &str) -> PathBuf {
    let rel = std::env::var(var).unwrap_or_else(|_| panic!("{var} unset: is it on the test rule?"));
    let p = runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"));
    assert!(p.exists(), "{var}={rel} resolved to a missing path {p:?}");
    p
}

fn qmd() -> Qmd {
    Qmd::new(
        runfile("QMD_TEST_NODE_RLOC"),
        runfile("QMD_TEST_PACKAGE_RLOC"),
    )
}

/// A data root holding `docs`, each `(group, file name, text)` under that
/// group's `render_markdown`.
fn root_with(docs: &[(&str, &str, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (group, name, text) in docs {
        write(root.path(), group, name, text);
    }
    root
}

fn write(root: &Path, group: &str, name: &str, text: &str) {
    let dir = root.join(group).join("render_markdown");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), text).unwrap();
}

const LOGS: &[(&str, &str, &str)] = &[
    (
        "bridge",
        "log-41153.md",
        "# Captain's log, stardate 41153.7\n\nThe Enterprise departs Farpoint Station.\n",
    ),
    (
        "bridge",
        "log-41209.md",
        "# Captain's log, stardate 41209.2\n\nQ delays the survey of Deneb IV.\n",
    ),
    (
        "sickbay",
        "report-41174.md",
        "# Medical report, stardate 41174.2\n\nPolywater intoxication in three crew.\n",
    ),
];

fn open(root: &Path) -> Index {
    Index::open(root, qmd()).unwrap()
}

fn by_name(index: &Index) -> Vec<(String, u64, u64)> {
    let mut all: Vec<_> = index
        .collections()
        .unwrap()
        .into_iter()
        .map(
            |Collection {
                 name,
                 documents,
                 needs_embedding,
             }| (name, documents, needs_embedding),
        )
        .collect();
    all.sort();
    all
}

fn quiet(_: UpdateProgress) {}

/// The regression: a step that exited before its script left node
/// writing to a pipe nobody read, and node died on the unhandled EPIPE.
/// Its stack trace became the step's recorded error.
#[test]
fn a_script_whose_reader_is_gone_says_so_in_one_line() {
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(runfile("QMD_TEST_NODE_RLOC"))
        .arg(runfile("QMD_TEST_SDK_RLOC"))
        .arg(runfile("QMD_TEST_PACKAGE_RLOC"))
        .arg(root.path().join("index.sqlite"))
        .arg("status")
        .arg("{}")
        .env("XDG_CACHE_HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Closed before node has loaded anything, so its one line hits it.
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "nobody got the answer: {stderr}");
    assert!(!stderr.contains("Unhandled 'error' event"), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("stdout is closed"), "{stderr}");
}

/// The point of a per-source step: one source's keyword update fills its
/// own collection and leaves every other one as it was.
#[test]
fn a_keyword_update_fills_its_own_collection_and_no_other() {
    let root = root_with(LOGS);
    let index = open(root.path());
    index.register(&["bridge", "sickbay"]).unwrap();

    let updated = index.keyword_index(&["bridge"], &quiet).unwrap().done;
    assert_eq!(
        updated,
        Updated {
            indexed: 2,
            ..Updated::default()
        }
    );
    assert_eq!(
        by_name(&index),
        [("bridge".into(), 2, 2), ("sickbay".into(), 0, 0)]
    );
}

/// A keyword step's log said nothing between its start and its end, so
/// a 27 s floor on a two-document source could not be placed.
#[test]
fn a_keyword_update_says_where_its_time_went() {
    let root = root_with(LOGS);
    let index = open(root.path());
    let ran = index.keyword_index(&["bridge"], &quiet).unwrap();
    let names: Vec<&str> = ran.phases.0.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["open_ms", "files_ms", "finish_ms"]);
    assert!(ran.phases.get("open_ms").unwrap() > 0);
}

/// The runner may start a source's keyword step before the step that
/// registers every source has run, so it registers its own collection —
/// in `index.yml` too, or `qmd mcp` would drop it from the registry the
/// next time it starts.
#[test]
fn a_keyword_update_registers_its_own_collection() {
    let root = root_with(LOGS);
    let index = open(root.path());

    index.keyword_index(&["sickbay"], &quiet).unwrap();
    assert_eq!(by_name(&index), [("sickbay".into(), 1, 1)]);
    let yml = root
        .path()
        .join("unified_document_index/qmd_aggregator/qmd/index.yml");
    let yml = std::fs::read_to_string(&yml).unwrap();
    assert!(yml.contains("sickbay:"), "{yml}");
    assert!(yml.contains("sickbay/render_markdown/**/*.md"), "{yml}");
}

#[test]
fn an_unchanged_tree_is_left_alone() {
    let root = root_with(LOGS);
    let index = open(root.path());
    index.keyword_index(&["bridge"], &quiet).unwrap();

    let again = index.keyword_index(&["bridge"], &quiet).unwrap().done;
    assert_eq!(
        again,
        Updated {
            unchanged: 2,
            ..Updated::default()
        }
    );
}

#[test]
fn a_new_an_edited_and_a_deleted_file_all_reach_the_index() {
    let root = root_with(LOGS);
    let index = open(root.path());
    index.keyword_index(&["bridge"], &quiet).unwrap();

    write(
        root.path(),
        "bridge",
        "log-41153.md",
        "# Captain's log\n\nA new entry.\n",
    );
    std::fs::remove_file(root.path().join("bridge/render_markdown/log-41209.md")).unwrap();
    write(
        root.path(),
        "bridge",
        "log-41986.md",
        "# Captain's log\n\nThe Borg.\n",
    );

    let updated = index.keyword_index(&["bridge"], &quiet).unwrap().done;
    assert_eq!(
        updated,
        Updated {
            indexed: 1,
            updated: 1,
            removed: 1,
            ..Updated::default()
        }
    );
    assert_eq!(by_name(&index), [("bridge".into(), 2, 2)]);
}

/// Registering is the whole collection set: a source no longer named is
/// retired with its documents, not just unregistered, since unregistered
/// documents would still be searched.
#[test]
fn registering_retires_every_other_collection_with_its_documents() {
    let root = root_with(LOGS);
    let index = open(root.path());
    index.keyword_index(&["bridge", "sickbay"], &quiet).unwrap();

    assert_eq!(index.register(&["bridge"]).unwrap(), ["sickbay"]);
    assert_eq!(by_name(&index), [("bridge".into(), 2, 2)]);
    let yml = root
        .path()
        .join("unified_document_index/qmd_aggregator/qmd/index.yml");
    assert!(!std::fs::read_to_string(yml).unwrap().contains("sickbay"));

    // Registered again, it starts from nothing rather than finding its
    // old rows still there.
    index.register(&["bridge", "sickbay"]).unwrap();
    assert_eq!(
        by_name(&index),
        [("bridge".into(), 2, 2), ("sickbay".into(), 0, 0)]
    );
}

/// The Manage row's bar: every file counts, and the last reading is the
/// whole tree.
#[test]
fn keyword_progress_counts_every_file() {
    let root = root_with(LOGS);
    let index = open(root.path());
    let seen = Mutex::new(Vec::new());
    index
        .keyword_index(&["bridge"], &|p| seen.lock().unwrap().push(p))
        .unwrap();
    let seen = seen.into_inner().unwrap();
    assert_eq!(
        seen.last(),
        Some(&UpdateProgress {
            current: 2,
            total: 2
        }),
        "{seen:?}"
    );
}

/// Holding qmd to one embedding context rests on a variable only qmd reads.
/// A qmd that stopped reading it would go back to eight contexts, about
/// 3.6 GB more, with nothing failing (`docs/dev/qmd_behaviour.md`, finding
/// 16).
#[test]
fn the_pinned_qmd_still_reads_the_variable_that_holds_it_to_one_context() {
    let llm = runfile("QMD_TEST_PACKAGE_RLOC").join("dist/llm.js");
    let source = std::fs::read_to_string(&llm).unwrap();
    assert!(
        source.contains(datalib_qmd_indexer::EMBED_PARALLELISM_VAR),
        "{} no longer reads {}",
        llm.display(),
        datalib_qmd_indexer::EMBED_PARALLELISM_VAR
    );
}

/// The embedding model, linked where qmd looks for it, under the name
/// node-llama-cpp gives a model it downloaded itself.
fn link_model(index: &Index, work: &Path) {
    let models = work.join("models");
    std::fs::create_dir_all(&models).unwrap();
    let name = &datalib_qmd_indexer::embed_model_names()[0];
    std::os::unix::fs::symlink(runfile("QMD_TEST_EMBED_MODEL_RLOC"), models.join(name)).unwrap();
    index.link_models(&models).unwrap();
}

#[test]
fn an_embed_without_the_model_linked_says_so() {
    let root = root_with(LOGS);
    let index = open(root.path());
    index.keyword_index(&["bridge"], &quiet).unwrap();
    let err = index.embed(&["bridge"], &|_| {}).unwrap_err();
    assert!(
        format!("{err:#}").contains("embedding model missing"),
        "{err:#}"
    );
}

/// A scoped embed over a name the registry lacks would match nothing and
/// report success; it has to be an error.
#[test]
fn an_embed_of_an_unregistered_collection_fails() {
    let root = root_with(LOGS);
    let index = open(root.path());
    let work = tempfile::tempdir().unwrap();
    link_model(&index, work.path());
    let err = index.embed(&["holodeck"], &|_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("is not registered"), "{err:#}");
}

/// One source's embed embeds its own documents and nobody else's; one
/// call can cover several sources, in one process and so one model load,
/// which is what a whole root's first embed wants; and an embed with
/// nothing left finds nothing to do. Loads the model twice.
#[test]
fn an_embed_fills_only_the_collections_it_names() {
    let root = root_with(LOGS);
    let index = open(root.path());
    let work = tempfile::tempdir().unwrap();
    link_model(&index, work.path());
    index.keyword_index(&["bridge", "sickbay"], &quiet).unwrap();

    let embedded = index.embed(&["bridge"], &|_| {}).unwrap().done;
    assert_eq!(
        (embedded.documents, embedded.errors),
        (2, 0),
        "{embedded:?}"
    );
    assert!(embedded.chunks >= 2, "{embedded:?}");
    assert_eq!(
        by_name(&index),
        [("bridge".into(), 2, 0), ("sickbay".into(), 1, 1)]
    );

    let embedded = index.embed(&["bridge", "sickbay"], &|_| {}).unwrap().done;
    assert_eq!(
        (embedded.documents, embedded.errors),
        (1, 0),
        "{embedded:?}"
    );
    assert_eq!(
        by_name(&index),
        [("bridge".into(), 2, 0), ("sickbay".into(), 1, 0)]
    );

    assert_eq!(
        index.embed(&["bridge", "sickbay"], &|_| {}).unwrap().done,
        Embedded::default()
    );
}

/// Models already verified, at `0`.
struct VerifiedAt(PathBuf);

impl VerifiedModels for VerifiedAt {
    fn dir(&self, _: Instant) -> anyhow::Result<PathBuf> {
        Ok(self.0.clone())
    }
}

fn query_embedder(root: &Path, models: &Path) -> QueryEmbedder {
    QueryEmbedder::new(
        root,
        Some(qmd()),
        Arc::new(VerifiedAt(models.to_path_buf())),
        Duration::from_secs(120),
    )
}

/// A query embeds with the file it was given, and under the name qmd
/// records beside the index's vectors, so the two compare.
#[test]
fn a_query_embeds_with_the_model_it_was_given() {
    let root = tempfile::tempdir().unwrap();
    let models = tempfile::tempdir().unwrap();
    let name = &datalib_qmd_indexer::embed_model_names()[0];
    std::os::unix::fs::symlink(
        runfile("QMD_TEST_EMBED_MODEL_RLOC"),
        models.path().join(name),
    )
    .unwrap();
    let embedded = query_embedder(root.path(), models.path())
        .embed("warp core breach")
        .unwrap();
    assert_eq!(
        embedded.model,
        "hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf"
    );
    assert!(!embedded.vector.is_empty());
}

/// The regression: with the model absent, qmd fetched it from
/// HuggingFace's `resolve/main`, unpinned, into the root. Now it is an
/// error naming where the model should be, and nothing is fetched.
#[test]
fn a_query_without_its_model_fetches_nothing() {
    let root = tempfile::tempdir().unwrap();
    let models = tempfile::tempdir().unwrap();
    let err = query_embedder(root.path(), models.path())
        .embed("warp core breach")
        .unwrap_err();
    assert!(format!("{err:#}").contains("is not at"), "{err:#}");
    let qmd_models = datalib_runtime::qmd::qmd_state_dir(root.path()).join("models");
    assert!(!qmd_models.exists(), "qmd made {}", qmd_models.display());
    assert_eq!(std::fs::read_dir(models.path()).unwrap().count(), 0);
}

/// An embed loads only the models its caller names, through the same
/// store qmd's own `generateEmbeddings` uses: with none named, it refuses
/// the embedding model even though qmd would find it in its cache.
#[test]
fn an_embed_loads_no_model_it_was_not_given() {
    let root = root_with(LOGS);
    let index = open(root.path());
    let work = tempfile::tempdir().unwrap();
    link_model(&index, work.path());
    index.keyword_index(&["bridge"], &quiet).unwrap();
    let qmd_dir = datalib_runtime::qmd::qmd_state_dir(&root.path().canonicalize().unwrap());
    let cache_home = datalib_runtime::qmd::qmd_cache_home(&root.path().canonicalize().unwrap());
    let out = std::process::Command::new(runfile("QMD_TEST_NODE_RLOC"))
        .arg(runfile("QMD_TEST_SDK_RLOC"))
        .arg(runfile("QMD_TEST_PACKAGE_RLOC"))
        .arg(qmd_dir.join("index.sqlite"))
        .arg("embed")
        .arg(r#"{"collections":["bridge"],"models":{}}"#)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_CONFIG_HOME", &cache_home)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("which it was not given"), "{stdout}");
}
