//! `Index` against the real qmd: the pinned package, run by the pinned
//! node, over a few documents a test writes. Everything but the embed
//! tests runs without a model and takes well under a second.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use datalib_qmd_indexer::{Collection, Embedded, Index, Qmd, UpdateProgress, Updated};

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

/// The point of a per-source step: one source's keyword update fills its
/// own collection and leaves every other one as it was.
#[test]
fn a_keyword_update_fills_its_own_collection_and_no_other() {
    let root = root_with(LOGS);
    let index = open(root.path());
    index.register(&["bridge", "sickbay"]).unwrap();

    let updated = index.keyword_index(&["bridge"], &quiet).unwrap();
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
        .join("unified_index/qmd_aggregator/qmd/index.yml");
    let yml = std::fs::read_to_string(&yml).unwrap();
    assert!(yml.contains("sickbay:"), "{yml}");
    assert!(yml.contains("sickbay/render_markdown/**/*.md"), "{yml}");
}

#[test]
fn an_unchanged_tree_is_left_alone() {
    let root = root_with(LOGS);
    let index = open(root.path());
    index.keyword_index(&["bridge"], &quiet).unwrap();

    let again = index.keyword_index(&["bridge"], &quiet).unwrap();
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

    let updated = index.keyword_index(&["bridge"], &quiet).unwrap();
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
        .join("unified_index/qmd_aggregator/qmd/index.yml");
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

    let embedded = index.embed(&["bridge"], &|_| {}).unwrap();
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

    let embedded = index.embed(&["bridge", "sickbay"], &|_| {}).unwrap();
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
        index.embed(&["bridge", "sickbay"], &|_| {}).unwrap(),
        Embedded::default()
    );
}
