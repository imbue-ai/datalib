//! The `qmd mcp` behaviour the search depends on, one test per fact,
//! grouped by the section of `docs/dev/qmd_behaviour.md` that states it.
//! A qmd bump that moves a fact fails here by name: fix the fact in
//! qmd_behaviour.md, then the test, then whatever in the tree leaned on
//! it. What the indexing side relies on is held by
//! `//datalib/backend/qmd_indexer:qmd_indexer_tests`.
//!
//! Each test builds a small index through `Index` (the code the steps
//! run) and talks to the pinned `qmd mcp` itself, line-delimited JSON-RPC
//! on its stdin and stdout, so a fact is about qmd and not about
//! `QmdDaemon`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use datalib_qmd_indexer::{Index, Qmd, UpdateProgress};
use serde_json::{json, Value};

// ── plumbing ────────────────────────────────────────────────────────

fn runfile(var: &str) -> PathBuf {
    let rel = std::env::var(var).unwrap_or_else(|_| panic!("{var} unset: is it on the test rule?"));
    let p = runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"));
    assert!(p.exists(), "{var}={rel} resolved to a missing path {p:?}");
    p
}

fn node() -> PathBuf {
    runfile("QMD_TEST_NODE_RLOC")
}

fn package() -> PathBuf {
    runfile("QMD_TEST_PACKAGE_RLOC")
}

/// A data root, its index, and the file qmd keeps it in.
struct Root {
    dir: tempfile::TempDir,
    index: Index,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(dir.path(), Qmd::new(node(), package())).unwrap();
        Root { dir, index }
    }

    fn write(&self, group: &str, name: &str, text: &str) {
        let at = self.dir.path().join(group).join("render_markdown");
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join(name), text).unwrap();
    }

    fn keyword_index(&self, groups: &[&str]) {
        self.index
            .keyword_index(groups, &|_: UpdateProgress| {})
            .unwrap();
    }

    /// The embedding model, linked where qmd looks for it.
    fn link_model(&self) {
        let models = self.dir.path().join("_models");
        std::fs::create_dir_all(&models).unwrap();
        let name = &datalib_qmd_indexer::embed_model_names()[0];
        std::os::unix::fs::symlink(runfile("QMD_TEST_EMBED_MODEL_RLOC"), models.join(name))
            .unwrap();
        self.index.link_models(&models).unwrap();
    }

    fn embed(&self, groups: &[&str]) {
        self.index.embed(groups, &|_| {}).unwrap();
    }

    fn index_file(&self) -> PathBuf {
        datalib_runtime::qmd::qmd_index_path(self.dir.path())
    }

    fn serve(&self) -> Server {
        Server::start(self.dir.path())
    }
}

/// A running `qmd mcp`, as `QmdDaemon` runs it: the cache and config
/// homes both at the data root's qmd directory.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(root: &Path) -> Server {
        let home = datalib_runtime::qmd::qmd_cache_home(root);
        let mut child = Command::new(node())
            .arg(package().join("dist/cli/qmd.js"))
            .arg("mcp")
            .env("XDG_CACHE_HOME", &home)
            .env("XDG_CONFIG_HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn qmd mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut server = Server {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        server.call(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "qmd_facts", "version": "0"},
            }),
        );
        server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        server
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "qmd mcp closed its stdout before answering {method}");
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == id {
                return msg;
            }
        }
    }

    /// The files a `query` returns, best first, as qmd names them:
    /// `<collection>/<path under the collection's root>`.
    fn query(
        &mut self,
        searches: Value,
        limit: usize,
        collections: Option<&[&str]>,
    ) -> Vec<String> {
        let mut arguments = json!({"searches": searches, "limit": limit, "rerank": false});
        if let Some(c) = collections {
            arguments["collections"] = json!(c);
        }
        self.query_args(arguments)
    }

    /// [`Server::query`] with the `query` tool's arguments as given.
    fn query_args(&mut self, arguments: Value) -> Vec<String> {
        let resp = self.call(
            "tools/call",
            json!({"name": "query", "arguments": arguments}),
        );
        let results = resp["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap_or_else(|| panic!("no results in {resp}"));
        results
            .iter()
            .map(|r| r["file"].as_str().unwrap().to_string())
            .collect()
    }

    fn lex(&mut self, q: &str, limit: usize, collections: Option<&[&str]>) -> Vec<String> {
        self.query(json!([{"type": "lex", "query": q}]), limit, collections)
    }

    fn vec(&mut self, q: &str, limit: usize) -> Vec<String> {
        self.query(json!([{"type": "vec", "query": q}]), limit, None)
    }

    /// Close its stdin, which is how `QmdDaemon` lets it go, and wait.
    fn stop(self) {
        let Server {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        child.wait().unwrap();
    }
}

fn collection_of(file: &str) -> &str {
    file.split('/').next().unwrap()
}

fn stamp(path: &Path) -> (std::time::SystemTime, u64) {
    let m = std::fs::metadata(path).unwrap();
    (m.modified().unwrap(), m.len())
}

// ── A running server reads the index live ───────────────────────────

/// Why the daemon need not restart when the keyword index moves: a
/// document indexed after the server started is found by it.
#[test]
fn a_running_server_finds_a_document_keyword_indexed_after_it_started() {
    let root = Root::new();
    root.write(
        "bridge",
        "log-1.md",
        "# Log\n\nThe Enterprise departs Farpoint.\n",
    );
    root.keyword_index(&["bridge"]);
    let mut server = root.serve();
    assert_eq!(
        server.lex("Farpoint", 10, None),
        ["bridge/bridge/render_markdown/log-1.md"]
    );
    assert!(server.lex("Kobayashi", 10, None).is_empty());

    root.write(
        "bridge",
        "log-2.md",
        "# Log\n\nThe Kobayashi Maru scenario.\n",
    );
    root.keyword_index(&["bridge"]);
    assert_eq!(
        server.lex("Kobayashi", 10, None),
        ["bridge/bridge/render_markdown/log-2.md"]
    );
    server.stop();
}

/// The same for vectors: a document embedded after the server started
/// is found by meaning. Loads the model twice.
#[test]
fn a_running_server_finds_a_document_embedded_after_it_started() {
    let root = Root::new();
    root.link_model();
    root.write(
        "bridge",
        "log-1.md",
        "# Log\n\nThe starship enters orbit around a gas giant.\n",
    );
    root.keyword_index(&["bridge"]);
    root.embed(&["bridge"]);
    let mut server = root.serve();
    assert_eq!(
        server.vec("a spaceship circling a planet", 1),
        ["bridge/bridge/render_markdown/log-1.md"]
    );

    root.write(
        "bridge",
        "log-2.md",
        "# Log\n\nA puppy chases a red ball across the park lawn.\n",
    );
    root.keyword_index(&["bridge"]);
    root.embed(&["bridge"]);
    assert_eq!(
        server.vec("a dog playing fetch outside", 1),
        ["bridge/bridge/render_markdown/log-2.md"]
    );
    server.stop();
}

/// A server writes the index once, and then never: at startup it
/// reconciles the registry with `index.yml` unless the index already
/// holds that file's hash (`store_config.config_hash`), and a keyword
/// update leaves the hash stale. So the first server after an update
/// writes, its write reaching `index.sqlite` when it stops, and the next
/// one starts, searches and stops without touching the file.
#[test]
fn only_the_first_server_after_a_keyword_update_writes_the_index() {
    let root = Root::new();
    root.write(
        "bridge",
        "log-1.md",
        "# Log\n\nThe Enterprise departs Farpoint.\n",
    );
    root.keyword_index(&["bridge"]);

    let before = stamp(&root.index_file());
    root.serve().stop();
    let after_first = stamp(&root.index_file());
    assert_ne!(after_first, before);

    let mut server = root.serve();
    assert!(!server.lex("Farpoint", 10, None).is_empty());
    server.stop();
    assert_eq!(stamp(&root.index_file()), after_first);
}

// ── How much a query returns ────────────────────────────────────────

/// Each sub-query takes the best 20 documents of each collection it
/// searches, however deep `limit` and `candidateLimit` ask: one source
/// alone never answers with more.
#[test]
fn a_sub_query_takes_twenty_documents_from_a_collection() {
    let root = Root::new();
    for i in 0..50 {
        root.write("bridge", &format!("log-{i}.md"), &warp_log(i));
    }
    root.keyword_index(&["bridge"]);
    let mut server = root.serve();
    let args = json!({
        "searches": [{"type": "lex", "query": "warp"}],
        "limit": 100,
        "candidateLimit": 100,
        "rerank": false,
    });
    assert_eq!(server.query_args(args).len(), 20);
    server.stop();
}

/// The sub-queries' lists are merged, then cut to `candidateLimit`, 40
/// unless asked, rerank or not: why the daemon sends one.
#[test]
fn the_merged_answer_is_cut_to_its_candidate_limit() {
    let root = Root::new();
    let decks = ["bridge", "engineering", "sickbay"];
    for deck in decks {
        for i in 0..20 {
            root.write(deck, &format!("log-{i}.md"), &warp_log(i));
        }
    }
    root.keyword_index(&decks);
    let mut server = root.serve();
    let asked = |candidates: Option<usize>| {
        let mut args = json!({
            "searches": [{"type": "lex", "query": "warp"}],
            "limit": 100,
            "collections": decks,
            "rerank": false,
        });
        if let Some(n) = candidates {
            args["candidateLimit"] = json!(n);
        }
        args
    };
    assert_eq!(server.query_args(asked(None)).len(), 40);
    assert_eq!(server.query_args(asked(Some(100))).len(), 60);
    server.stop();
}

fn warp_log(i: usize) -> String {
    format!("# Log {i}\n\nThe Enterprise holds at warp six.\n")
}

// ── Scoping a query to collections ──────────────────────────────────

/// Why the daemon has to name the collections even for an unscoped
/// search: with no list, qmd searches the ones it read at startup.
#[test]
fn an_unscoped_query_searches_the_collections_registered_when_the_server_started() {
    let root = Root::new();
    root.write(
        "bridge",
        "log-1.md",
        "# Log\n\nThe Enterprise departs Farpoint.\n",
    );
    root.keyword_index(&["bridge"]);
    let mut server = root.serve();

    root.write(
        "sickbay",
        "report-1.md",
        "# Report\n\nPolywater intoxication.\n",
    );
    root.keyword_index(&["sickbay"]);
    assert!(server.lex("Polywater", 10, None).is_empty());
    assert_eq!(
        server.lex("Polywater", 10, Some(&["sickbay"])),
        ["sickbay/sickbay/render_markdown/report-1.md"]
    );
    server.stop();
}

/// Why the daemon answers an empty scope itself: qmd reads `[]` as no
/// scope at all.
#[test]
fn an_empty_collection_list_is_unscoped() {
    let root = Root::new();
    root.write(
        "bridge",
        "log-1.md",
        "# Log\n\nThe Enterprise at stardate 41153.\n",
    );
    root.write(
        "sickbay",
        "report-1.md",
        "# Report\n\nSickbay at stardate 41174.\n",
    );
    root.keyword_index(&["bridge", "sickbay"]);
    let mut server = root.serve();
    let hits = server.lex("stardate", 10, Some(&[]));
    let mut collections: Vec<&str> = hits.iter().map(|f| collection_of(f)).collect();
    collections.sort();
    assert_eq!(collections, ["bridge", "sickbay"]);
    server.stop();
}

/// Why an unscoped search sends `[]` rather than every collection's name:
/// an empty list reaches a collection registered after the server
/// started, which no list at all does not.
#[test]
fn an_empty_collection_list_reaches_a_collection_added_after_start() {
    let root = Root::new();
    root.write(
        "bridge",
        "log-1.md",
        "# Log\n\nThe Enterprise departs Farpoint.\n",
    );
    root.keyword_index(&["bridge"]);
    let mut server = root.serve();
    root.write(
        "sickbay",
        "report-1.md",
        "# Report\n\nPolywater intoxication.\n",
    );
    root.keyword_index(&["sickbay"]);
    assert_eq!(
        server.lex("Polywater", 10, Some(&[])),
        ["sickbay/sickbay/render_markdown/report-1.md"]
    );
    server.stop();
}

/// Why an unscoped search never names several collections: qmd ranks each
/// named collection apart and merges the lists by rank alone, the first
/// named list counting double, so a poor match in the first collection
/// outranks the best match in the next. With `[]` it is one list, best
/// match first.
#[test]
fn naming_several_collections_ranks_each_apart_and_merges_by_rank() {
    let root = Root::new();
    root.write(
        "aft",
        "log-1.md",
        "# Cargo\n\nThe cargo manifest lists forty crates of grain, two crates of \
         spare parts, medical supplies, and one note about the warp schedule.\n",
    );
    root.write(
        "bridge",
        "log-1.md",
        "# Warp\n\nWarp drive, warp core, warp field.\n",
    );
    root.keyword_index(&["aft", "bridge"]);
    let mut server = root.serve();
    let first = |hits: Vec<String>| collection_of(&hits[0]).to_string();
    assert_eq!(
        first(server.lex("warp", 10, Some(&["aft", "bridge"]))),
        "aft"
    );
    assert_eq!(
        first(server.lex("warp", 10, Some(&["bridge", "aft"]))),
        "bridge"
    );
    assert_eq!(first(server.lex("warp", 10, Some(&[]))), "bridge");
    server.stop();
}

/// Why `source_id:` scopes qmd rather than filtering its answer: the
/// scope applies before the limit, so a collection whose hits rank below
/// another's still fills the answer.
#[test]
fn a_scope_applies_before_the_limit() {
    let root = Root::new();
    root.write(
        "bridge",
        "log-1.md",
        "# Warp\n\nWarp drive, warp core, warp field.\n",
    );
    root.write(
        "sickbay",
        "report-1.md",
        "# Report\n\nThe patient recovered after the warp core breach on deck twelve, \
         once the radiation burns and the broken arm had both been treated.\n",
    );
    root.keyword_index(&["bridge", "sickbay"]);
    let mut server = root.serve();
    assert_eq!(
        server.lex("warp", 1, None),
        ["bridge/bridge/render_markdown/log-1.md"]
    );
    assert_eq!(
        server.lex("warp", 1, Some(&["sickbay"])),
        ["sickbay/sickbay/render_markdown/report-1.md"]
    );
    server.stop();
}

// ── What needs a model ──────────────────────────────────────────────

/// Why a keyword-only pass can be fast: a `lex` query answers with no
/// embedding model anywhere qmd could load one from.
#[test]
fn a_keyword_query_needs_no_model() {
    let root = Root::new();
    root.write(
        "bridge",
        "log-1.md",
        "# Log\n\nThe Enterprise departs Farpoint.\n",
    );
    root.keyword_index(&["bridge"]);
    let mut server = root.serve();
    assert_eq!(
        server.lex("Farpoint", 10, None),
        ["bridge/bridge/render_markdown/log-1.md"]
    );
    server.stop();
}
