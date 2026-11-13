//! What SQLite's `vec1` vector-search extension does inside doltlite, one
//! test per fact in docs/dev/doltlite.md § "Vector search: vec1". vec1 is
//! compiled only into the test-only `//third-party/doltlite:doltlite_vec1`
//! shell, so every fact here is driven through that shell, on a plain
//! SQLite file unless the test says otherwise.
//!
//! The corpus is made up: 2,000 vectors of source `a` near the origin and
//! 10 of source `b` far away along the first axis, so the nearest
//! neighbours of the origin are all `a` and a query asking for `b` shows at
//! once whether its filter ran before or after the K nearest were chosen.

use std::fmt::Debug;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;

const QUERY: &str = "vec1_from_json('[0,0,0,0,0,0,0,0]')";

const CORPUS: &str = "
CREATE TABLE docs(doc_id INTEGER PRIMARY KEY, source_id TEXT NOT NULL);
CREATE INDEX docs_source ON docs(source_id);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2010)
INSERT INTO docs SELECT i, CASE WHEN i<=2000 THEN 'a' ELSE 'b' END FROM n;
CREATE VIRTUAL TABLE v USING vec1(embedding, source_id);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2010)
INSERT INTO v(rowid, embedding, source_id)
SELECT d.doc_id,
  vec1_from_json(json_array(
    (i*37%101)/100.0 + CASE WHEN d.source_id='b' THEN 1000 ELSE 0 END,
    (i*53%103)/100.0, (i*71%107)/100.0, (i*29%109)/100.0,
    (i*17%113)/100.0, (i*13%127)/100.0, (i*11%131)/100.0, (i*7%137)/100.0)),
  d.source_id
FROM n JOIN docs d ON d.doc_id = n.i;
";

#[derive(Clone, Copy, Debug)]
enum Index {
    /// No `rebuild` yet: vec1 answers with a full scan of its base table.
    Untrained,
    /// `rebuild` with `{"index":"flat"}`: raw vectors, one bucket.
    Flat,
    /// `rebuild` with a `vec1_train` model: 8 IVF buckets, PQ codes.
    Trained,
}

impl Index {
    fn build_sql(self) -> &'static str {
        match self {
            Index::Untrained => "",
            Index::Flat => "INSERT INTO v(cmd, arg) VALUES ('rebuild', '{\"index\":\"flat\"}');",
            Index::Trained => {
                "CREATE TEMP TABLE m AS \
                   SELECT vec1_train(embedding, '{\"nbucket\":8, \"codesize\":2}') AS model FROM v;
                 INSERT INTO v(cmd, arg) SELECT 'rebuild', model FROM m;"
            }
        }
    }
}

fn vec1_cli() -> PathBuf {
    bin("DOLTLITE_VEC1_BIN")
}

fn bin(var: &str) -> PathBuf {
    PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("{var} is set by the BUILD rule")))
        .canonicalize()
        .expect("the shell is in runfiles")
}

/// Runs `sql` through `cli` on `db` with `-bail`; returns success and the
/// shell's stdout and stderr together.
fn shell_on(cli: PathBuf, db: &str, sql: &str) -> (bool, String) {
    let mut child = Command::new(cli)
        .args(["-bail", "-list", db])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the shell");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .expect("write sql");
    let out = child.wait_with_output().expect("wait for the shell");
    let mut both = String::from_utf8_lossy(&out.stdout).into_owned();
    both.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), both.trim().to_string())
}

struct Db {
    _dir: tempfile::TempDir,
    target: String,
}

impl Db {
    fn plain_sqlite() -> Db {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.sqlite");
        let target = format!("file:{}?doltlite_engine=sqlite", path.display());
        Db { _dir: dir, target }
    }

    fn doltlite_store() -> Db {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("index.doltlite_db").display().to_string();
        Db { _dir: dir, target }
    }

    fn with_corpus(self, index: Index) -> Db {
        self.run(&format!("{CORPUS}{}", index.build_sql()));
        self
    }

    fn run(&self, sql: &str) -> String {
        let (ok, out) = shell_on(vec1_cli(), &self.target, sql);
        assert!(ok, "{sql}\n{out}");
        out
    }

    fn lines(&self, sql: &str) -> Vec<String> {
        self.run(sql).lines().map(str::to_string).collect()
    }

    fn plan(&self, sql: &str) -> String {
        self.run(&format!("EXPLAIN QUERY PLAN {sql}"))
    }
}

/// Two-column `x|y` lines, parsed.
fn pairs<A: FromStr, B: FromStr>(lines: &[String]) -> Vec<(A, B)>
where
    A::Err: Debug,
    B::Err: Debug,
{
    lines
        .iter()
        .map(|l| {
            let (a, b) = l.split_once('|').unwrap_or_else(|| panic!("row: {l}"));
            (a.parse().unwrap(), b.parse().unwrap())
        })
        .collect()
}

fn ids(lines: &[String]) -> Vec<i64> {
    lines.iter().map(|l| l.parse().unwrap()).collect()
}

fn b_ids() -> Vec<i64> {
    (2001..=2010).collect()
}

fn knn_where_source_is_b(k: &str) -> String {
    format!("SELECT rowid FROM v WHERE cmd = {QUERY} AND arg = '{k}' AND source_id = 'b';")
}

#[test]
fn the_shipped_build_has_no_vec1() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("s.doltlite_db").display().to_string();
    let (ok, out) = shell_on(bin("DOLTLITE_BIN"), &db, "SELECT vec1_info();");
    assert!(!ok && out.contains("no such function: vec1_info"), "{out}");
}

#[test]
fn the_vec1_shell_builds_vec1_with_simd_on_this_platform() {
    let out = Db::plain_sqlite().run("SELECT vec1_info();");
    assert!(out.starts_with("version 0.7"), "{out}");
}

#[test]
fn a_filter_on_a_vec1_column_is_applied_before_the_k_nearest_are_chosen() {
    for index in [Index::Untrained, Index::Flat] {
        let db = Db::plain_sqlite().with_corpus(index);
        let got = ids(&db.lines(&knn_where_source_is_b("5")));
        assert_eq!(got.len(), 5, "{index:?}: {got:?}");
        assert!(
            got.iter().all(|id| b_ids().contains(id)),
            "{index:?}: {got:?}"
        );
        let in_list = db.lines(&format!(
            "SELECT count(*) FROM v WHERE cmd = {QUERY} AND arg = 5 AND source_id IN ('b', 'c');"
        ));
        assert_eq!(in_list, ["5"], "{index:?}: IN on a vec1 column");
    }
}

#[test]
fn a_filter_on_a_vec1_column_is_handed_to_vec1_in_the_plan() {
    let db = Db::plain_sqlite().with_corpus(Index::Flat);
    let plan = db.plan(&knn_where_source_is_b("5"));
    assert!(plan.contains("SCAN v VIRTUAL TABLE INDEX 5:PA00"), "{plan}");
}

#[test]
fn a_filter_on_a_vec1_column_works_in_a_doltlite_store_too() {
    let db = Db::doltlite_store().with_corpus(Index::Flat);
    assert_eq!(ids(&db.lines(&knn_where_source_is_b("5"))).len(), 5);
}

#[test]
fn a_rowid_filter_is_applied_after_the_k_nearest_are_chosen() {
    let db = Db::plain_sqlite().with_corpus(Index::Flat);
    let sql = format!(
        "SELECT count(*) FROM v WHERE cmd = {QUERY} AND arg = 5 \
         AND rowid IN (SELECT doc_id FROM docs WHERE source_id = 'b');"
    );
    assert_eq!(db.lines(&sql), ["0"]);
    let plan = db.plan(&sql);
    assert!(plan.contains("SCAN v VIRTUAL TABLE INDEX 5:P\n"), "{plan}");
}

#[test]
fn a_join_to_a_filtered_table_is_applied_after_the_k_nearest_are_chosen() {
    let db = Db::plain_sqlite().with_corpus(Index::Flat);
    let sql = format!(
        "SELECT count(*) FROM docs d JOIN v ON v.rowid = d.doc_id \
         WHERE v.cmd = {QUERY} AND v.arg = 5 AND d.source_id = 'b';"
    );
    assert_eq!(db.lines(&sql), ["0"]);
}

#[test]
fn ordering_by_vec1_l2_distance_over_an_indexed_filter_is_exact() {
    let db = Db::plain_sqlite().with_corpus(Index::Trained);
    let sql = format!(
        "SELECT d.doc_id FROM docs d JOIN v ON v.rowid = d.doc_id \
         WHERE d.source_id = 'b' ORDER BY vec1_l2_distance({QUERY}, v.embedding) LIMIT 5;"
    );
    let got = ids(&db.lines(&sql));
    let exact = ids(&db.lines(&format!(
        "SELECT rowid FROM v WHERE source_id = 'b' \
         ORDER BY vec1_l2_distance({QUERY}, embedding) LIMIT 5;"
    )));
    assert_eq!(got, exact);
    let plan = db.plan(&sql);
    assert!(
        plan.contains("SEARCH d USING COVERING INDEX docs_source (source_id=?)")
            && plan.contains("SCAN v VIRTUAL TABLE INDEX 3:"),
        "{plan}"
    );
}

#[test]
fn vec1_l2_distance_is_the_squared_distance() {
    let out = Db::plain_sqlite()
        .run("SELECT vec1_l2_distance(vec1_from_json('[0,0]'), vec1_from_json('[3,4]'));");
    assert_eq!(out, "25.0");
}

#[test]
fn an_untrained_or_flat_table_ranks_by_exact_distance() {
    for index in [Index::Untrained, Index::Flat] {
        let db = Db::plain_sqlite().with_corpus(index);
        let knn: Vec<(i64, f64)> = pairs(&db.lines(&format!(
            "SELECT rowid, distance FROM v WHERE cmd = {QUERY} AND arg = 10;"
        )));
        let exact: Vec<(i64, f64)> = pairs(&db.lines(&format!(
            "SELECT rowid, vec1_l2_distance({QUERY}, embedding) AS d FROM v ORDER BY d LIMIT 10;"
        )));
        assert_eq!(knn, exact, "{index:?}");
    }
}

#[test]
fn a_trained_table_ranks_by_approximate_distance_and_does_not_rerank() {
    let db = Db::plain_sqlite().with_corpus(Index::Trained);
    let rows = db.lines(&format!(
        "SELECT distance, vec1_l2_distance({QUERY}, embedding) FROM v \
         WHERE cmd = {QUERY} AND arg = '{{\"K\":20,\"nprobe\":8}}';"
    ));
    assert_eq!(rows.len(), 20);
    let off = pairs::<f64, f64>(&rows)
        .iter()
        .filter(|(approx, exact)| (approx - exact).abs() > 1e-4)
        .count();
    assert!(off > 0, "every distance was exact: {rows:?}");
}

#[test]
fn a_trained_table_filters_only_within_the_buckets_it_probes() {
    let db = Db::plain_sqlite().with_corpus(Index::Trained);
    let one_bucket = db.lines(&knn_where_source_is_b("{\"K\":5,\"nprobe\":1}"));
    assert!(one_bucket.is_empty(), "{one_bucket:?}");
    let default_nprobe = db.lines(&knn_where_source_is_b("5"));
    assert!(default_nprobe.is_empty(), "{default_nprobe:?}");
    let every_bucket = ids(&db.lines(&knn_where_source_is_b("{\"K\":5,\"nprobe\":8}")));
    assert_eq!(every_bucket.len(), 5);
    assert!(every_bucket.iter().all(|id| b_ids().contains(id)));
}

#[test]
fn a_streaming_query_probes_further_buckets_until_the_limit_is_met() {
    let db = Db::plain_sqlite().with_corpus(Index::Trained);
    let got = ids(&db.lines(&format!(
        "SELECT rowid FROM v WHERE cmd = {QUERY} \
         AND arg = '{{\"K\":5,\"nprobe\":1,\"streaming\":1}}' AND source_id = 'b' LIMIT 5;"
    )));
    assert_eq!(got.len(), 5, "{got:?}");
    assert!(got.iter().all(|id| b_ids().contains(id)));
}

#[test]
fn a_row_inserted_after_training_is_found_without_retraining() {
    let db = Db::plain_sqlite().with_corpus(Index::Trained);
    db.run(
        "INSERT INTO v(rowid, embedding, source_id) \
         VALUES (3000, vec1_from_json('[0,0,0,0,0,0,0,0.001]'), 'c');",
    );
    let got = db.lines(&format!(
        "SELECT rowid FROM v WHERE cmd = {QUERY} AND arg = 3 AND source_id = 'c';"
    ));
    assert_eq!(got, ["3000"]);
}

/// Trains on 511 two-element vectors with `config`; the shell's output.
fn train_on_511(config: &str) -> (bool, String) {
    let db = Db::plain_sqlite();
    db.run(
        "CREATE VIRTUAL TABLE v USING vec1(embedding);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<511)
         INSERT INTO v(embedding) SELECT vec1_from_json(json_array(i, i % 7)) FROM n;",
    );
    shell_on(
        vec1_cli(),
        &db.target,
        &format!("SELECT length(vec1_train(embedding, '{config}')) > 0 FROM v;"),
    )
}

#[test]
fn product_quantization_needs_512_training_vectors() {
    let (ok, out) = train_on_511("{\"nbucket\":2, \"codesize\":1}");
    assert!(
        !ok && out.contains("too few training vectors (require 512, have 511)"),
        "{out}"
    );
}

#[test]
fn buckets_alone_need_4_training_vectors_each() {
    assert_eq!(train_on_511("{\"nbucket\":127}"), (true, "1".to_string()));
    let (ok, out) = train_on_511("{\"nbucket\":128}");
    assert!(
        !ok && out.contains("too few training vectors (require 512, have 511)"),
        "{out}"
    );
}

#[test]
fn a_query_with_neither_k_nor_a_limit_is_refused() {
    let db = Db::plain_sqlite().with_corpus(Index::Untrained);
    let (ok, out) = shell_on(
        vec1_cli(),
        &db.target,
        &format!("SELECT rowid FROM v WHERE cmd = {QUERY};"),
    );
    assert!(
        !ok && out.contains("no K value or visible LIMIT clause"),
        "{out}"
    );
}
