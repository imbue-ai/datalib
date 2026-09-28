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

use datalib_qmd_fixture::{materialize_root, stage_models, stage_runtime};
use datalib_unified_index::qmd::{CollectionScope, QmdDaemon, QmdDaemonConfig, QueryMode};

/// One of the fixture's groups, chosen because it has the most rendered
/// documents — so "scoped to it" and "everything" are clearly different
/// answers, and an accidental no-op scope would still show up.
const SCOPED_GROUP: &str = "slack";

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
    let runtime = stage_runtime(&work);
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
