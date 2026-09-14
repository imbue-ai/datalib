//! The render framework driven without a provider, the way the runner is
//! tested without a step: a synthetic raw store, a reference renderer, and
//! random histories of mutations cut into random runs. After every run the
//! render store must equal one cold render of the raw store as it stands
//! (`docs/dev/plans/one_mode.md`, §"Testing it without a provider").
//!
//! The only fake here is the provider. The store, the diff scan, the
//! driver (`render_source`) and the index (`build_grid_index`) are the
//! real ones.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use datalib_etl::doltlite_raw::{self, DiffScanSpec};
use datalib_etl::pin::{self, Reads};
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::{build_grid_index, init_schema, RenderedMarkdown};
use datalib_etl_render::indexed_markdown::{blocking, IndexedMarkdownStore};
use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
use datalib_schema::edges::EdgeRow;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::providers::Provider;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::render::{render_source, RenderSource};

const SOURCE: &str = "synth";
const NOW: &str = "2026-09-14T12:00:00+00:00";

// ── the synthetic raw store and its reference renderer ──────────────

/// Three tables, chosen for the three shapes every real bucket query has
/// to handle: the bucket entity, rows that reach a bucket through a
/// foreign key, and a table every document reads but none owns.
const RAW_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS parents (id TEXT PRIMARY KEY, title TEXT NOT NULL, author_id TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS children (id TEXT PRIMARY KEY, parent_id TEXT NOT NULL, body TEXT NOT NULL, seq INT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS authors (id TEXT PRIMARY KEY, name TEXT NOT NULL)",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Parent {
    title: String,
    author_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Child {
    parent_id: String,
    body: String,
    seq: i64,
}

/// The raw store as the test believes it to be. The doltlite file is
/// kept equal to this by applying every mutation to both.
#[derive(Debug, Clone, Default)]
struct Model {
    parents: BTreeMap<String, Parent>,
    children: BTreeMap<String, Child>,
    authors: BTreeMap<String, String>,
}

/// Render knobs. `upper` changes every document, so a param change is
/// only right if every bucket renders again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Params {
    upper: bool,
}

impl Params {
    fn json(self) -> serde_json::Value {
        serde_json::json!({ "upper": self.upper })
    }
}

/// One document as both sides of the comparison see it: what the
/// reference renderer produces, and what a store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Doc {
    fingerprint: String,
    /// `(row uuid, text)`, sorted by uuid.
    rows: Vec<(String, String)>,
    /// `(edge uuid, destination)`, sorted.
    edges: Vec<(String, String)>,
}

/// The reference renderer, shared by the model (over memory) and the
/// synthetic provider (over the pinned store). Sharing it is the point:
/// the property under test is the framework's incrementality, not this.
fn render_parent(
    id: &str,
    parent: &Parent,
    children: &[(&String, &Child)],
    authors: &BTreeMap<String, String>,
    params: Params,
) -> Doc {
    let case = |s: &str| {
        if params.upper {
            s.to_uppercase()
        } else {
            s.to_string()
        }
    };
    let author = authors
        .get(&parent.author_id)
        .cloned()
        .unwrap_or_else(|| format!("unknown:{}", parent.author_id));
    let mut rows = vec![(id.to_string(), case(&format!("{author}: {}", parent.title)))];
    let mut ordered: Vec<&(&String, &Child)> = children.iter().collect();
    ordered.sort_by_key(|(cid, c)| (c.seq, (*cid).clone()));
    let mut edges = Vec::new();
    for (cid, c) in ordered {
        rows.push(((*cid).clone(), case(&c.body)));
        if let Some(target) = c.body.strip_prefix("->") {
            edges.push((format!("{cid}->{target}"), target.to_string()));
        }
    }
    rows.sort();
    edges.sort();
    let mut h = blake3::Hasher::new();
    for (u, t) in &rows {
        h.update(u.as_bytes());
        h.update(b"\0");
        h.update(t.as_bytes());
        h.update(b"\n");
    }
    for (e, d) in &edges {
        h.update(e.as_bytes());
        h.update(b"\0");
        h.update(d.as_bytes());
        h.update(b"\n");
    }
    Doc {
        fingerprint: h.finalize().to_hex().to_string(),
        rows,
        edges,
    }
}

/// One cold render of the model: what every incremental path must reach.
fn expected(model: &Model, params: Params) -> BTreeMap<String, Doc> {
    model
        .parents
        .iter()
        .map(|(id, p)| {
            let children: Vec<(&String, &Child)> = model
                .children
                .iter()
                .filter(|(_, c)| &c.parent_id == id)
                .collect();
            (
                id.clone(),
                render_parent(id, p, &children, &model.authors, params),
            )
        })
        .collect()
}

// ── the synthetic provider: the reference renderer behind the framework ──

/// What a real provider is: a scan, a load of the buckets the scan
/// named, a removal for each named bucket that is gone, a document per
/// bucket, and a report of the commit it read. Plus the knobs the test
/// turns: the version and params it declares, and a point to fail at.
struct SynthRender {
    raw_db: PathBuf,
    version: AtomicU32,
    params: Mutex<Params>,
    fail_after: AtomicUsize,
}

const NEVER: usize = usize::MAX;

impl SynthRender {
    fn new(raw_db: PathBuf) -> Self {
        Self {
            raw_db,
            version: AtomicU32::new(1),
            params: Mutex::new(Params { upper: false }),
            fail_after: AtomicUsize::new(NEVER),
        }
    }
    fn params(&self) -> Params {
        *self.params.lock().unwrap()
    }
}

#[async_trait]
impl RenderProcessor for SynthRender {
    fn id(&self) -> &str {
        "synth/synth/render"
    }
    fn render_version(&self) -> Option<u32> {
        Some(self.version.load(Ordering::SeqCst))
    }
    fn render_params(&self) -> serde_json::Value {
        self.params().json()
    }

    async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
        let pool = blocking(doltlite_raw::open_reader(&self.raw_db)).context("open raw")?;
        let Some(pin) = blocking(pin::head(&pool))? else {
            blocking(pool.close());
            return Ok("nothing committed".into());
        };
        blocking(pin::install_views(&pool, &pin))?;
        let scan = blocking(doltlite_raw::scan_buckets(
            &pool,
            ctx.raw_cursor,
            &pin,
            &DiffScanSpec {
                global_fanout_tables: &["authors"],
                bucket_query: "
                    SELECT DISTINCT bucket FROM (
                        SELECT coalesce(to_id, from_id) AS bucket FROM dolt_diff_parents
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_parent_id, from_parent_id) FROM dolt_diff_children
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    ) WHERE bucket IS NOT NULL
                ",
            },
        ))?;
        let model = blocking(load_model(&pool, Reads::At(&pin)))?;
        if let Some(changed) = scan.changed_buckets.as_ref() {
            let gone = blocking(doltlite_raw::buckets_without_rows(
                &pool,
                Reads::At(&pin),
                changed,
                &[("parents", "id")],
            ))?;
            for bucket in gone {
                ctx.remove_conversation(&bucket)?;
            }
        }
        blocking(pool.close());

        let params = self.params();
        let version = self.version.load(Ordering::SeqCst);
        let fail_after = self.fail_after.load(Ordering::SeqCst);
        let mut rendered = 0usize;
        for (id, doc) in expected(&model, params) {
            if scan.render.as_ref().is_some_and(|c| !c.contains(&id)) {
                continue;
            }
            if rendered == fail_after {
                bail!("synthetic failure after {rendered} document(s)");
            }
            rendered += 1;
            if ctx.prior_fingerprints.get(&id) == Some(&doc.fingerprint) {
                continue;
            }
            let root = datalib_etl::layout::render_markdown_root(ctx.root, ctx.name);
            let md_path = root.join(format!("{id}.md"));
            std::fs::create_dir_all(&root)?;
            std::fs::write(&md_path, &doc.fingerprint)?;
            ctx.emit_doc(to_rendered(&id, &doc, md_path, version))?;
        }
        ctx.consumed(pin.commit());
        Ok(format!("rendered {rendered}"))
    }
}

async fn load_model(pool: &SqlitePool, reads: Reads<'_>) -> Result<Model> {
    let mut model = Model::default();
    let sql = format!(
        "SELECT id, title, author_id FROM {}",
        reads.table("parents")
    );
    // Audited: the table name is a literal through `Reads::table`.
    for r in sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?
    {
        model.parents.insert(
            r.try_get(0)?,
            Parent {
                title: r.try_get(1)?,
                author_id: r.try_get(2)?,
            },
        );
    }
    let sql = format!(
        "SELECT id, parent_id, body, seq FROM {}",
        reads.table("children")
    );
    for r in sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?
    {
        model.children.insert(
            r.try_get(0)?,
            Child {
                parent_id: r.try_get(1)?,
                body: r.try_get(2)?,
                seq: r.try_get(3)?,
            },
        );
    }
    let sql = format!("SELECT id, name FROM {}", reads.table("authors"));
    for r in sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?
    {
        model.authors.insert(r.try_get(0)?, r.try_get(1)?);
    }
    Ok(model)
}

fn to_rendered(id: &str, doc: &Doc, md_path: PathBuf, version: u32) -> RenderedMarkdown {
    let rows = doc
        .rows
        .iter()
        .map(|(uuid, text)| {
            GridRow::builder()
                .uuid(uuid)
                .provider(Provider::Test)
                .kind("Synth")
                .source_label("Synth")
                .conversation_uuid(id)
                .entire_chat(format!("/chat/{id}"))
                .text(text)
                .markdown_uuid(Some(id.to_string()))
                .build()
                .expect("row")
        })
        .collect();
    let edges = doc
        .edges
        .iter()
        .map(|(edge_uuid, dst)| EdgeRow {
            edge_uuid: edge_uuid.clone(),
            src_markdown_uuid: id.to_string(),
            src_anchor_uuid: None,
            dst_markdown_uuid: dst.clone(),
            dst_anchor_uuid: None,
            label: Some("mentions".into()),
        })
        .collect();
    RenderedMarkdown {
        markdown_uuid: id.to_string(),
        source_id: SOURCE.into(),
        source_fingerprint: doc.fingerprint.clone(),
        upstream_cursor: None,
        md_path,
        render_version: version,
        rows,
        edges,
        problems: Vec::new(),
    }
}

// ── the raw store the test writes ───────────────────────────────────

#[derive(Debug, Clone)]
enum Mutation {
    InsertParent(String, Parent),
    Retitle(String, String),
    DeleteParent(String),
    InsertChild(String, Child),
    Rebody(String, String),
    DeleteChild(String),
    RenameAuthor(String, String),
}

fn apply_to_model(model: &mut Model, m: &Mutation) {
    match m {
        Mutation::InsertParent(id, p) => {
            model.parents.insert(id.clone(), p.clone());
        }
        Mutation::Retitle(id, title) => {
            model.parents.get_mut(id).unwrap().title = title.clone();
        }
        Mutation::DeleteParent(id) => {
            model.parents.remove(id);
            model.children.retain(|_, c| &c.parent_id != id);
        }
        Mutation::InsertChild(id, c) => {
            model.children.insert(id.clone(), c.clone());
        }
        Mutation::Rebody(id, body) => {
            model.children.get_mut(id).unwrap().body = body.clone();
        }
        Mutation::DeleteChild(id) => {
            model.children.remove(id);
        }
        Mutation::RenameAuthor(id, name) => {
            model.authors.insert(id.clone(), name.clone());
        }
    }
}

async fn apply_to_store(pool: &SqlitePool, m: &Mutation) -> Result<()> {
    match m {
        Mutation::InsertParent(id, p) => {
            sqlx::query("INSERT INTO parents (id, title, author_id) VALUES (?, ?, ?)")
                .bind(id)
                .bind(&p.title)
                .bind(&p.author_id)
                .execute(pool)
                .await?;
        }
        Mutation::Retitle(id, title) => {
            sqlx::query("UPDATE parents SET title = ? WHERE id = ?")
                .bind(title)
                .bind(id)
                .execute(pool)
                .await?;
        }
        Mutation::DeleteParent(id) => {
            sqlx::query("DELETE FROM children WHERE parent_id = ?")
                .bind(id)
                .execute(pool)
                .await?;
            sqlx::query("DELETE FROM parents WHERE id = ?")
                .bind(id)
                .execute(pool)
                .await?;
        }
        Mutation::InsertChild(id, c) => {
            sqlx::query("INSERT INTO children (id, parent_id, body, seq) VALUES (?, ?, ?, ?)")
                .bind(id)
                .bind(&c.parent_id)
                .bind(&c.body)
                .bind(c.seq)
                .execute(pool)
                .await?;
        }
        Mutation::Rebody(id, body) => {
            sqlx::query("UPDATE children SET body = ? WHERE id = ?")
                .bind(body)
                .bind(id)
                .execute(pool)
                .await?;
        }
        Mutation::DeleteChild(id) => {
            sqlx::query("DELETE FROM children WHERE id = ?")
                .bind(id)
                .execute(pool)
                .await?;
        }
        Mutation::RenameAuthor(id, name) => {
            sqlx::query("INSERT OR REPLACE INTO authors (id, name) VALUES (?, ?)")
                .bind(id)
                .bind(name)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// A small deterministic generator, so a failure prints a seed that
/// reproduces it and the test needs no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.below(items.len())])
        }
    }
}

fn mutation(rng: &mut Rng, model: &Model, counter: &mut usize) -> Mutation {
    let parents: Vec<String> = model.parents.keys().cloned().collect();
    let children: Vec<String> = model.children.keys().cloned().collect();
    let authors: Vec<String> = model.authors.keys().cloned().collect();
    *counter += 1;
    let n = *counter;
    // Weighted towards growth so histories have something to delete.
    match rng.below(10) {
        0 | 1 => Mutation::InsertParent(
            format!("p{n}"),
            Parent {
                title: format!("title {n}"),
                author_id: format!("a{}", rng.below(3)),
            },
        ),
        2 if !parents.is_empty() => {
            Mutation::Retitle(rng.pick(&parents).unwrap().clone(), format!("title {n}"))
        }
        3 if !parents.is_empty() => Mutation::DeleteParent(rng.pick(&parents).unwrap().clone()),
        4..=6 if !parents.is_empty() => {
            let parent = rng.pick(&parents).unwrap().clone();
            let body = match rng.pick(&parents) {
                Some(target) if rng.chance(3) => format!("->{target}"),
                _ => format!("body {n}"),
            };
            Mutation::InsertChild(
                format!("c{n}"),
                Child {
                    parent_id: parent,
                    body,
                    seq: n as i64,
                },
            )
        }
        7 if !children.is_empty() => {
            Mutation::Rebody(rng.pick(&children).unwrap().clone(), format!("body {n}"))
        }
        8 if !children.is_empty() => Mutation::DeleteChild(rng.pick(&children).unwrap().clone()),
        _ => {
            let id = match rng.pick(&authors) {
                Some(a) if rng.chance(2) => a.clone(),
                _ => format!("a{}", rng.below(3)),
            };
            Mutation::RenameAuthor(id, format!("name {n}"))
        }
    }
}

// ── reading the stores back ─────────────────────────────────────────

fn docs_of(rendered: &[RenderedMarkdown]) -> BTreeMap<String, Doc> {
    rendered
        .iter()
        .map(|md| {
            let mut rows: Vec<(String, String)> = md
                .rows
                .iter()
                .map(|r| (r.uuid.clone(), r.text.clone()))
                .collect();
            rows.sort();
            let mut edges: Vec<(String, String)> = md
                .edges
                .iter()
                .map(|e| (e.edge_uuid.clone(), e.dst_markdown_uuid.clone()))
                .collect();
            edges.sort();
            (
                md.markdown_uuid.clone(),
                Doc {
                    fingerprint: md.source_fingerprint.clone(),
                    rows,
                    edges,
                },
            )
        })
        .collect()
}

/// The render store at one commit, as documents, plus the `.md` files
/// on disk, which must be exactly the documents' — a deleted document
/// left on disk stays searchable (`remove_document`'s reason to exist).
fn render_store_at(
    data_root: &Path,
    commit: Option<&str>,
) -> (BTreeMap<String, Doc>, BTreeSet<String>, Vec<u32>) {
    let root = datalib_etl::layout::render_markdown_root(data_root, SOURCE);
    let store = IndexedMarkdownStore::open_for_reading(&root).expect("open for reading");
    let pin = match commit {
        Some(c) => store.pin_at(c).expect("pin"),
        None => store.pin_for_reading().expect("pin").expect("a commit"),
    };
    let rendered = store.documents(data_root, &pin).expect("documents");
    let versions = rendered.iter().map(|d| d.render_version).collect();
    store.close();
    let files = std::fs::read_dir(&root)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".md").map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    (docs_of(&rendered), files, versions)
}

async fn index_docs(pool: &SqlitePool) -> BTreeMap<String, Doc> {
    let mds =
        sqlx::query("SELECT markdown_uuid, source_fingerprint FROM markdowns WHERE source_id = ?")
            .bind(SOURCE)
            .fetch_all(pool)
            .await
            .unwrap();
    let mut out = BTreeMap::new();
    for md in mds {
        let uuid: String = md.try_get(0).unwrap();
        let fingerprint: Option<String> = md.try_get(1).unwrap();
        let mut rows: Vec<(String, String)> =
            sqlx::query("SELECT uuid, text FROM grid_rows WHERE markdown_uuid = ?")
                .bind(&uuid)
                .fetch_all(pool)
                .await
                .unwrap()
                .into_iter()
                .map(|r| (r.try_get(0).unwrap(), r.try_get(1).unwrap()))
                .collect();
        rows.sort();
        let mut edges: Vec<(String, String)> = sqlx::query(
            "SELECT edge_uuid, dst_markdown_uuid FROM edges WHERE src_markdown_uuid = ?",
        )
        .bind(&uuid)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.try_get(0).unwrap(), r.try_get(1).unwrap()))
        .collect();
        edges.sort();
        out.insert(
            uuid,
            Doc {
                fingerprint: fingerprint.unwrap_or_default(),
                rows,
                edges,
            },
        );
    }
    out
}

fn diff(want: &BTreeMap<String, Doc>, got: &BTreeMap<String, Doc>) -> String {
    let mut out = String::new();
    for (k, v) in want {
        match got.get(k) {
            None => out.push_str(&format!("missing {k}\n")),
            Some(g) if g != v => {
                out.push_str(&format!("differs {k}:\n  want {v:?}\n  got  {g:?}\n"))
            }
            _ => {}
        }
    }
    for k in got.keys() {
        if !want.contains_key(k) {
            out.push_str(&format!("extra {k}\n"));
        }
    }
    out
}

// ── the model test ──────────────────────────────────────────────────

struct World {
    data_root: PathBuf,
    raw_db: PathBuf,
    model: Model,
    counter: usize,
    index: SqlitePool,
    dolt: bool,
    /// What each run saw, for the failure message.
    history: Vec<String>,
}

impl World {
    async fn new(data_root: &Path) -> World {
        let raw_dir = data_root.join(SOURCE).join("raw");
        std::fs::create_dir_all(&raw_dir).unwrap();
        let raw_db = doltlite_raw::db_path_for(&raw_dir);
        let db = data_root.join("unified_index/grid/db.doltlite_db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let index = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&db)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        init_schema(&index).await.unwrap();
        // Create the raw store's schema now, and ask it whether this
        // build links doltlite at all.
        let pool = doltlite_raw::open(&raw_db, RAW_DDL).await.unwrap();
        let dolt = doltlite_raw::has_dolt_extensions(&pool).await;
        pool.close().await;
        World {
            data_root: data_root.to_path_buf(),
            raw_db,
            model: Model::default(),
            counter: 0,
            index,
            dolt,
            history: Vec::new(),
        }
    }

    /// Apply a wave of mutations to the model and the store, and commit.
    /// One writer per open, closed before the render opens its reader.
    async fn commit_wave(&mut self, rng: &mut Rng, mutations: usize) -> Option<String> {
        let pool = doltlite_raw::open(&self.raw_db, RAW_DDL).await.unwrap();
        for _ in 0..mutations {
            let m = mutation(rng, &self.model, &mut self.counter);
            apply_to_store(&pool, &m).await.unwrap();
            apply_to_model(&mut self.model, &m);
            self.history.push(format!("  {m:?}"));
        }
        self.history.push("  -- commit".into());
        let hash = doltlite_raw::commit_run(&pool, "wave").await.unwrap();
        pool.close().await;
        hash
    }

    async fn render(&self, synth: &SynthRender, seal_often: bool) -> Result<()> {
        let processors: Vec<Box<dyn RenderProcessor>> =
            vec![Box::new(SynthRender::clone_of(synth))];
        let source = RenderSource {
            name: SOURCE.into(),
            data_root: self.data_root.clone(),
            rendered_root: datalib_etl::layout::render_markdown_root(&self.data_root, SOURCE),
            now: NOW.into(),
            cadence: if seal_often {
                datalib_etl::checkpointer::Cadence {
                    at_most_every: std::time::Duration::ZERO,
                }
            } else {
                Default::default()
            },
            storage: None,
            progress: Progress::noop(),
        };
        tokio::task::spawn_blocking(move || render_source(&processors, source).map(|_| ()))
            .await
            .unwrap()
    }

    async fn index(&self) {
        build_grid_index(&self.index, &self.data_root, |_| {}, Some(NOW))
            .await
            .unwrap();
    }
}

impl SynthRender {
    fn clone_of(other: &SynthRender) -> SynthRender {
        SynthRender {
            raw_db: other.raw_db.clone(),
            version: AtomicU32::new(other.version.load(Ordering::SeqCst)),
            params: Mutex::new(other.params()),
            fail_after: AtomicUsize::new(other.fail_after.load(Ordering::SeqCst)),
        }
    }
}

fn assert_store_is(world: &World, want: &BTreeMap<String, Doc>, ctx: &str) {
    let (got, files, _) = render_store_at(&world.data_root, None);
    let d = diff(want, &got);
    assert!(
        d.is_empty(),
        "{ctx}: render store ≠ cold render:\n{d}\nhistory:\n{}",
        world.history.join("\n")
    );
    let want_files: BTreeSet<String> = want.keys().cloned().collect();
    assert_eq!(
        files, want_files,
        "{ctx}: the .md files on disk are not the documents'"
    );
}

/// Every commit the run made is one a consumer may read: each document
/// in it is either the run's answer or the previous run's, and nothing
/// unchanged between the two ever went missing.
fn assert_every_commit_is_truthful(
    world: &World,
    commits: &[String],
    before: &BTreeMap<String, Doc>,
    after: &BTreeMap<String, Doc>,
    ctx: &str,
) {
    for c in commits {
        let (got, _, _) = render_store_at(&world.data_root, Some(c));
        for (uuid, doc) in &got {
            let ok = before.get(uuid) == Some(doc) || after.get(uuid) == Some(doc);
            assert!(
                ok,
                "{ctx}: commit {c} holds a torn or invented document {uuid}: {doc:?}"
            );
        }
        for (uuid, doc) in before {
            if after.get(uuid) == Some(doc) {
                assert!(
                    got.contains_key(uuid),
                    "{ctx}: commit {c} lost {uuid}, which neither run changed"
                );
            }
        }
    }
}

/// `(hash, message)` of every commit in the render store, newest first.
fn log_of(world: &World) -> Vec<(String, String)> {
    let root = datalib_etl::layout::render_markdown_root(&world.data_root, SOURCE);
    if !datalib_etl_render::indexed_markdown::path_for(&root).exists() {
        return Vec::new();
    }
    let store = IndexedMarkdownStore::open_for_reading(&root).unwrap();
    let c = store.log().unwrap();
    store.close();
    c
}

fn commits_of(world: &World) -> Vec<String> {
    log_of(world).into_iter().map(|(h, _)| h).collect()
}

/// Random histories, random run boundaries, random checkpoint cadence,
/// occasional version bumps, param changes and mid-run failures; the
/// real index reading the result. Seeds are fixed so a failure names one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incremental_render_equals_cold_render_under_random_histories() {
    for seed in 1..=12u64 {
        let td = tempfile::tempdir().unwrap();
        let mut world = World::new(td.path()).await;
        if !world.dolt {
            return;
        }
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let synth = SynthRender::new(world.raw_db.clone());
        let mut params = Params { upper: false };
        let mut before = BTreeMap::new();

        for run in 0..10 {
            let ctx = format!("seed {seed} run {run}");
            for _ in 0..(1 + rng.below(3)) {
                let mutations = 1 + rng.below(5);
                world.commit_wave(&mut rng, mutations).await;
            }
            if rng.chance(6) {
                synth.version.fetch_add(1, Ordering::SeqCst);
                world.history.push("  -- version bump".into());
            }
            if rng.chance(6) {
                params = Params {
                    upper: !params.upper,
                };
                *synth.params.lock().unwrap() = params;
                world.history.push("  -- params changed".into());
            }
            world.history.push(format!("== run {run}"));
            let after = expected(&world.model, params);
            let seal_often = rng.chance(2);
            let commits_before = commits_of(&world);

            // Sometimes fail partway, then recover. The trip-wire counts
            // documents the run actually renders, so an incremental run
            // with fewer than that simply succeeds — it was then an
            // ordinary run, and is checked as one below.
            let mut succeeded = false;
            if rng.chance(4) && !after.is_empty() {
                synth
                    .fail_after
                    .store(rng.below(after.len()), Ordering::SeqCst);
                let res = world.render(&synth, seal_often).await;
                synth.fail_after.store(NEVER, Ordering::SeqCst);
                succeeded = res.is_ok();
                if !succeeded && !commits_of(&world).is_empty() {
                    let (got, _, _) = render_store_at(&world.data_root, None);
                    for (uuid, doc) in &got {
                        let ok = before.get(uuid) == Some(doc) || after.get(uuid) == Some(doc);
                        assert!(ok, "{ctx}: after a failed run, {uuid} is torn: {doc:?}");
                    }
                }
            }

            if !succeeded {
                world
                    .render(&synth, seal_often)
                    .await
                    .unwrap_or_else(|e| panic!("{ctx}: {e:#}"));
            }
            assert_store_is(&world, &after, &ctx);
            let new_commits: Vec<(String, String)> = log_of(&world)
                .into_iter()
                .filter(|(c, _)| !commits_before.contains(c))
                .collect();
            // A batch is SQL-committed only on its way into a dolt
            // commit, so nothing is ever left for the next open to
            // rescue — not even after the failed run above.
            for (c, msg) in &new_commits {
                assert!(
                    !msg.starts_with("rescue:"),
                    "{ctx}: commit {c} is a rescue — a batch was SQL-committed and never sealed"
                );
            }
            let new_commits: Vec<String> = new_commits.into_iter().map(|(c, _)| c).collect();
            assert_every_commit_is_truthful(&world, &new_commits, &before, &after, &ctx);
            let (_, _, versions) = render_store_at(&world.data_root, None);
            let v = synth.version.load(Ordering::SeqCst);
            assert!(
                versions.iter().all(|x| *x == v),
                "{ctx}: documents at a version the renderer no longer declares: {versions:?}"
            );

            if rng.chance(2) {
                world.index().await;
                let got = index_docs(&world.index).await;
                let d = diff(&after, &got);
                assert!(d.is_empty(), "{ctx}: index ≠ render store:\n{d}");
            }
            before = after;
        }
        world.index().await;
        let got = index_docs(&world.index).await;
        let want = expected(&world.model, params);
        let d = diff(&want, &got);
        assert!(d.is_empty(), "seed {seed}: final index ≠ cold render:\n{d}");
        world.index.close().await;
    }
}

/// A steady-state run writes nothing: the diff names no bucket, the
/// store's HEAD does not move, and the index reads nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_over_an_unchanged_store_moves_nothing() {
    let td = tempfile::tempdir().unwrap();
    let mut world = World::new(td.path()).await;
    if !world.dolt {
        return;
    }
    let mut rng = Rng(7);
    let synth = SynthRender::new(world.raw_db.clone());
    world.commit_wave(&mut rng, 12).await;
    world.render(&synth, false).await.unwrap();
    let head = commits_of(&world);
    world.render(&synth, false).await.unwrap();
    assert_eq!(
        commits_of(&world),
        head,
        "a second run over the same commit committed something"
    );
    world.index.close().await;
}
