//! What a source's mirror weighs, as rows the grid can show.
//!
//! Every source's render wave ends here, including the ones that render
//! no documents of their own — which is how `fsindex` and `media` get a
//! place in the UI at all.
//!
//! **Scope is the raw store, not the whole tree.** `<name>/rendered_md`
//! is datalib's own output, `system/usage.doltlite_db` already tracks it
//! per step, and measuring it from inside the thing that writes it is a
//! ratchet: every run would find a bigger tree, write a bigger number,
//! and commit — growing the store it just measured, forever, on a
//! pipeline where nothing upstream changed.
//!
//! **A `Table` row carries no byte size.** doltlite is a
//! content-addressed chunk store with no page layout — `dbstat` refuses
//! outright — and chunks are shared between tables and between commits,
//! so no honest per-table number exists to report. Row counts are exact
//! and cheap; file sizes are exact and free. Those are what this emits.
//!
//! **What re-renders the report is a count, never a byte.** Both of the
//! numbers that move on their own live on the storage side rather than
//! in the data. `sync_runs` gains a row per run, so its table is left
//! out of the report. And a doltlite store's size is not merely
//! growing but *not reproducible*: rebuilding the TNG fixture from
//! byte-identical inputs moves six of its sixteen sources by 1-22
//! bytes, in a different direction each time — so bytes stay out of
//! the fingerprint here and out of `compute_row_set_hash` too.
//! Otherwise every run rewrites a report nothing asked for and
//! `grid_index` never gets to skip a source.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_id::{entity_id_str, IdNamespace, Scope};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::measurements::{MeasurementKind, SourceMeasurementRow};
use datalib_schema::providers::Provider;

/// The `grid_rows.source_label` — the grid's Source column, and what
/// `source:` filters on. One label for every source's measurements, so
/// `source:Storage` is "show me what everything weighs"; `source_name:`
/// still narrows to one source, since these rows live under that
/// source's `rendered_md/`.
pub const SOURCE_LABEL: &str = "Storage";

/// Where the report lands inside the source's render output.
const REPORT_REL: &str = "_datalib/storage.md";

/// Bumped when the shape of what this emits changes, so an older
/// report is re-rendered rather than left to disagree with a newer one.
pub const RENDER_VERSION: u32 = 1;

/// One measured thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    /// Data-root-relative, and stable across runs — this is what joins
    /// one run's sample to the next one's. A table inside a store is
    /// `<store path>#<table>`.
    pub path: String,
    pub kind: MeasurementKind,
    pub bytes: Option<i64>,
    pub items: Option<i64>,
}

impl Subject {
    fn uuid(&self, source_name: &str) -> String {
        entity_id_str(
            IdNamespace::Datalib,
            Scope::SourceInstance(source_name),
            self.kind.as_str(),
            &self.path,
        )
    }

    /// The one-line human summary that becomes `grid_rows.text` and the
    /// report's body line.
    /// `grid_rows.text`. **Carries no byte figure**, on purpose.
    ///
    /// `text` is hashed into `compute_row_set_hash`, the markdown cache
    /// key — and a doltlite store's size is not reproducible. It drifts
    /// 1-22 bytes between rebuilds on one machine, and differs outright
    /// between machines: CI's Linux runner and a developer's Mac
    /// produce different sizes for byte-identical inputs, which made
    /// the fixture golden unable to pass on both at once.
    ///
    /// The size is not lost — it is in `byte_size`, which the grid
    /// renders in its own column, and in the report's Size cell. It is
    /// only kept out of the string that a staleness decision hashes.
    fn summary(&self) -> String {
        match self.items.map(|n| plural(n, self.counts())) {
            Some(n) => format!("{} — {n}", self.path),
            None => self.path.clone(),
        }
    }

    /// What `item_count` counts for this kind. The column is
    /// deliberately unitless, so the unit has to come from somewhere and
    /// this is it.
    fn counts(&self) -> &'static str {
        match self.kind {
            MeasurementKind::Tree => "file",
            MeasurementKind::Table => "row",
            MeasurementKind::Store => "item",
        }
    }
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Binary units, because the thing being described is a file on disk.
fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Total bytes and file count under `dir`, following no symlinks — a
/// cycle would never return and a shared target would be counted twice.
fn walk(dir: &Path) -> (i64, i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut bytes = 0i64;
    let mut files = 0i64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            let (b, f) = walk(&entry.path());
            bytes += b;
            files += f;
        } else {
            bytes += meta.len() as i64;
            files += 1;
        }
    }
    (bytes, files)
}

/// Open a store without claiming it. Read-only matters: doltlite's
/// working set is per *file* and shared across processes, so a
/// read-write handle here could sweep another writer's in-flight rows
/// into a commit.
async fn open_read_only(db_path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("open {}", db_path.display()))?
        .read_only(true)
        .create_if_missing(false);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("connect {}", db_path.display()))
}

/// The suffix `doltlite_raw::bookkeeping_ddl_for` gives a sidecar
/// table. Pinned against that generator by a test rather than trusted.
const BOOKKEEPING_SUFFIX: &str = "_bookkeeping";

/// The table `ddl` creates, for a `CREATE TABLE IF NOT EXISTS <name> (`.
/// `None` for anything else, so a future statement this cannot read is
/// skipped rather than silently mis-parsed into a name that excludes
/// the wrong table.
fn table_name_in(ddl: &str) -> Option<&str> {
    ddl.trim()
        .strip_prefix("CREATE TABLE IF NOT EXISTS ")?
        .split(|c: char| c.is_whitespace() || c == '(')
        .find(|s| !s.is_empty())
}

/// Datalib's own run bookkeeping, as opposed to the source's data.
///
/// Read out of `doltlite_raw::SHARED_DDL` rather than listed here, so a
/// table added to the framework is excluded without anyone remembering
/// to come back — a hardcoded list would agree with itself forever
/// while the framework moved underneath it.
///
/// Two reasons to leave these out. They are not content —
/// `doltlite_raw`'s own words for `sync_runs` and `sync_scope_state`
/// are "audit log and resume cursor, not content" — and a
/// `<table>_bookkeeping` sidecar holds exactly one row per row of the
/// table it shadows, so reporting it doubles every count with no new
/// information.
///
/// The load-bearing half is `sync_runs`: it gains a row on every run,
/// so counting it would move the report's fingerprint on a pipeline
/// where nothing changed and hand `grid_index` work forever.
fn is_datalib_bookkeeping(table: &str) -> bool {
    table.ends_with(BOOKKEEPING_SUFFIX)
        || datalib_etl::doltlite_raw::SHARED_DDL
            .iter()
            .filter_map(|ddl| table_name_in(ddl))
            .any(|shared| shared == table)
}

async fn table_names(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("list tables")?;
    rows.into_iter()
        .map(|r| r.try_get::<String, _>(0).context("read table name"))
        .filter(|name| !matches!(name, Ok(n) if is_datalib_bookkeeping(n)))
        .collect()
}

async fn row_count(pool: &SqlitePool, table: &str) -> Result<i64> {
    // Audited: `table` came from `sqlite_master` on this same file, so
    // it is an identifier the engine itself just handed us; there is no
    // caller-supplied text in the string.
    let sql = format!("SELECT COUNT(*) FROM \"{}\"", table.replace('"', "\"\""));
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .with_context(|| format!("count {table}"))?;
    row.try_get::<i64, _>(0).context("read count")
}

/// Measure one source's raw store: the tree, each database file in it,
/// and each table inside those files.
pub async fn scan(data_root: &Path, source_name: &str) -> Result<Vec<Subject>> {
    let raw_rel = format!("{source_name}/raw");
    let raw_dir = data_root.join(&raw_rel);
    if !raw_dir.is_dir() {
        return Ok(Vec::new());
    }

    let (bytes, files) = walk(&raw_dir);
    let mut subjects = vec![Subject {
        path: raw_rel.clone(),
        kind: MeasurementKind::Tree,
        bytes: Some(bytes),
        items: Some(files),
    }];

    let mut stores: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&raw_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".doltlite_db") && entry.path().is_file() {
                stores.push((format!("{raw_rel}/{name}"), entry.path()));
            }
        }
    }
    stores.sort();

    for (rel, abs) in stores {
        let size = abs.symlink_metadata().map(|m| m.len() as i64).ok();
        subjects.push(Subject {
            path: rel.clone(),
            kind: MeasurementKind::Store,
            bytes: size,
            items: None,
        });
        // A store we cannot open is worth saying nothing about rather
        // than failing the whole render — the file's size is already
        // recorded above, which is the half that never fails.
        let pool = match open_read_only(&abs).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(store = %rel, error = %e, "introspect: could not open store");
                continue;
            }
        };
        let tables = match table_names(&pool).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(store = %rel, error = %e, "introspect: could not list tables");
                pool.close().await;
                continue;
            }
        };
        for table in tables {
            match row_count(&pool, &table).await {
                Ok(n) => subjects.push(Subject {
                    path: format!("{rel}#{table}"),
                    kind: MeasurementKind::Table,
                    // Deliberately NULL — see the module docs.
                    bytes: None,
                    items: Some(n),
                }),
                Err(e) => {
                    tracing::warn!(store = %rel, table = %table, error = %e, "introspect: could not count")
                }
            }
        }
        pool.close().await;
    }
    Ok(subjects)
}

/// Everything one measured source contributes: the document to store,
/// the samples to append to its series, and the report body that has
/// to reach disk before the document is stored.
pub struct Measured {
    pub doc: RenderedMarkdown,
    pub samples: Vec<SourceMeasurementRow>,
    /// Written by [`Measured::write_report`], never by [`plan`]. The
    /// split is load-bearing: the caller skips a source whose numbers
    /// have not moved, and writing the file before that check would
    /// restamp `measured_at` on every run — churning the rendered tree,
    /// whose content hash is what the scheduler uses to decide that
    /// `grid_index` has nothing to do.
    body: String,
}

impl Measured {
    pub fn write_report(&self) -> Result<()> {
        let path = &self.doc.md_path;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        std::fs::write(path, &self.body).with_context(|| format!("write {}", path.display()))
    }
}

/// A digest of what the source *contains*, so the report re-renders
/// when the contents move.
///
/// **Byte sizes are deliberately not in here.** A doltlite store grows
/// a little on every run that touches it at all — a bookkeeping
/// `last_attempt_at` mutation rewrites chunks even though no row was
/// added — so a fingerprint over bytes never settles: every run would
/// find a new number, rewrite the report, and give `grid_index` work to
/// do on a pipeline where nothing changed. Row and file counts are
/// properties of the data and hold still when the data does.
///
/// So `byte_size` reads as **how big the raw store was the last time
/// this source's contents changed** — not how big it is now. On a
/// source that has stopped changing, the number stops with it, however
/// many times the pipeline runs afterwards. For bytes on their own
/// cadence, `system/usage.doltlite_db` keeps a per-step series and
/// commits nothing, which is what lets it sample freely.
fn fingerprint(subjects: &[Subject]) -> String {
    let mut h = blake3::Hasher::new();
    // The version is in here because nothing else would re-render an
    // old report: the store's version check deliberately skips these
    // documents, so this fingerprint is the only thing that notices a
    // format change.
    h.update(&RENDER_VERSION.to_le_bytes());
    for s in subjects {
        h.update(s.path.as_bytes());
        h.update(b"\x1f");
        h.update(s.kind.as_str().as_bytes());
        h.update(b"\x1f");
        h.update(s.items.unwrap_or(-1).to_le_bytes().as_slice());
        h.update(b"\x1e");
    }
    h.finalize().to_hex().to_string()
}

fn report_body(source_name: &str, subjects: &[Subject], now: &str) -> String {
    let mut out = format!(
        "---\ntitle: {source_name} storage\nsource: {source_name}\nmeasured_at: {now}\n---\n\n\
         # {source_name} — storage\n\nMeasured {now}.\n\n"
    );
    // One table, not one wrapped `<div>` per measurement. A source with
    // a dozen stores and tables used to render a dozen bordered cards
    // stacked down the page to say twelve short facts; the same facts
    // fit in a dozen rows. The anchor moves onto a span inside the Id
    // cell — the frontend keys selection off `[data-section-uuid]`
    // wherever it sits, and a row is what a reader wants to land on.
    out.push_str("| Kind | What | Size | Count | Id |\n|---|---|---|---|---|\n");
    for s in subjects {
        let uuid = s.uuid(source_name);
        out.push_str(&format!(
            "| {kind} | `{path}` | {size} | {count} | \
             <span id=\"m-{uuid}\" data-section-uuid=\"{uuid}\">`{short}`</span> |\n",
            kind = s.kind.label(),
            path = s.path,
            size = s.bytes.map(human_bytes).unwrap_or_default(),
            count = s.items.map(|n| plural(n, s.counts())).unwrap_or_default(),
            // The full uuid is on the span for the deeplink and the
            // copy button; the cell shows the head of it, which is what
            // a person compares against a grid row.
            short = &uuid[..8.min(uuid.len())],
        ));
    }
    out.push('\n');
    out
}

/// Turn a scan into the document and samples to store. Writes nothing:
/// see [`Measured::write_report`]. `None` when the source has nothing
/// to measure.
///
/// `now` stamps the rows and samples this produces, so it is the time
/// the numbers last *moved* rather than the last time anything looked —
/// a run that measures and finds no change is skipped whole by the
/// caller and leaves the previous stamp standing.
pub fn plan(
    data_root: &Path,
    source_name: &str,
    subjects: Vec<Subject>,
    now: &str,
) -> Result<Option<Measured>> {
    if subjects.is_empty() {
        return Ok(None);
    }
    // The tree row is the document's canonical row: `upsert_markdown`
    // looks for the row whose uuid equals the markdown's, and that is
    // the one whose title and timestamp should describe the file.
    let markdown_uuid = subjects
        .iter()
        .find(|s| s.kind == MeasurementKind::Tree)
        .unwrap_or(&subjects[0])
        .uuid(source_name);

    let md_path = data_root
        .join(source_name)
        .join("rendered_md")
        .join(REPORT_REL);
    let qmd_rel = format!("{source_name}/rendered_md/{REPORT_REL}");

    let mut rows = Vec::with_capacity(subjects.len());
    let mut samples = Vec::with_capacity(subjects.len());
    for s in &subjects {
        let uuid = s.uuid(source_name);
        rows.push(
            GridRow::builder()
                .uuid(uuid.clone())
                .provider(Provider::Datalib)
                .kind(s.kind.label())
                .source_label(SOURCE_LABEL)
                .when_ts(Some(now.to_string()))
                .account(Some(source_name.to_string()))
                .conversation_name(Some(format!("{source_name} storage")))
                .conversation_uuid(markdown_uuid.clone())
                .entire_chat(format!("/chat/{markdown_uuid}"))
                .text(s.summary())
                .qmd_path(Some(qmd_rel.clone()))
                .markdown_uuid(Some(markdown_uuid.clone()))
                // The machine-parsable half: `upstream_id` is the
                // measured path verbatim and `upstream_entity_kind` the
                // enum's own string, so
                // `entity_id(provider, scope, kind, id) == uuid` holds
                // by construction and the row can be taken back to the
                // thing it measured.
                .upstream_id(Some(s.path.clone()))
                .upstream_entity_kind(Some(s.kind.as_str().to_string()))
                .upstream_scope(Some(source_name.to_string()))
                .byte_size(s.bytes)
                .item_count(s.items)
                .build()
                .with_context(|| format!("build measurement row for {}", s.path))?,
        );
        samples.push(SourceMeasurementRow {
            subject: s.path.clone(),
            kind: s.kind.as_str().to_string(),
            measured_at: now.to_string(),
            bytes: s.bytes,
            items: s.items,
        });
    }

    Ok(Some(Measured {
        doc: RenderedMarkdown {
            markdown_uuid,
            source_name: source_name.to_string(),
            source_fingerprint: fingerprint(&subjects),
            upstream_cursor: None,
            md_path,
            render_version: RENDER_VERSION,
            rows,
            edges: Vec::new(),
            problems: Vec::new(),
        },
        samples,
        body: report_body(source_name, &subjects, now),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn subject(
        path: &str,
        kind: MeasurementKind,
        bytes: Option<i64>,
        items: Option<i64>,
    ) -> Subject {
        Subject {
            path: path.into(),
            kind,
            bytes,
            items,
        }
    }

    /// The id has to be a function of the source and the measured path
    /// and nothing else — a run-varying component would mint a new row
    /// every run and turn the grid into the timeseries this design
    /// deliberately keeps out of it.
    #[test]
    fn a_measurement_id_does_not_move_when_its_value_does() {
        let a = subject("s/raw", MeasurementKind::Tree, Some(10), Some(1));
        let b = subject("s/raw", MeasurementKind::Tree, Some(999), Some(42));
        assert_eq!(a.uuid("s"), b.uuid("s"));
    }

    /// Two sources measuring identically-named trees must not collide.
    #[test]
    fn two_sources_measuring_the_same_relative_path_get_different_ids() {
        let s = subject("raw", MeasurementKind::Tree, Some(1), None);
        assert_ne!(s.uuid("slack"), s.uuid("notion"));
    }

    /// The kind is part of the recipe, so a store and a tree at one
    /// path stay distinct rows.
    #[test]
    fn the_kind_separates_two_measurements_of_one_path() {
        let t = subject("s/raw", MeasurementKind::Tree, Some(1), None);
        let st = subject("s/raw", MeasurementKind::Store, Some(1), None);
        assert_ne!(t.uuid("s"), st.uuid("s"));
    }

    /// The backpointer invariant `grid_rows` documents: recomputing the
    /// id from the columns the row carries reproduces the row's uuid.
    #[test]
    fn the_stored_backpointer_reproduces_the_uuid() {
        let td = tempdir().unwrap();
        let subjects = vec![subject("s/raw", MeasurementKind::Tree, Some(64), Some(2))];
        let m = plan(td.path(), "s", subjects, "2026-09-07T10:00:00-07:00")
            .unwrap()
            .expect("a measured source");
        let row = &m.doc.rows[0];
        // The stored `provider` tag is parsed back into the namespace
        // rather than assumed: that the two spell the same thing is
        // part of what makes the backpointer usable, and it would
        // otherwise be an unchecked coincidence between two enums.
        let namespace: IdNamespace = row
            .provider
            .parse()
            .expect("the stored provider tag names an id namespace");
        let recomputed = entity_id_str(
            namespace,
            Scope::SourceInstance(row.upstream_scope.as_deref().unwrap()),
            row.upstream_entity_kind.as_deref().unwrap(),
            row.upstream_id.as_deref().unwrap(),
        );
        assert_eq!(recomputed, row.uuid);
    }

    /// The fingerprint is what decides whether the report is written
    /// again, so it has to move when the source's contents move.
    #[test]
    fn the_fingerprint_tracks_the_counts() {
        let a = vec![subject("s/raw", MeasurementKind::Tree, Some(10), Some(1))];
        let b = vec![subject("s/raw", MeasurementKind::Tree, Some(10), Some(2))];
        assert_ne!(fingerprint(&a), fingerprint(&b));
        assert_eq!(fingerprint(&a), fingerprint(&a.clone()));
    }

    /// A store that only got *bigger* must not re-render the report.
    ///
    /// This is the regression `ingested_tng_test`'s "run 2 reads 0
    /// documents" assertion caught: a doltlite store grows on any run
    /// that touches it — a bookkeeping `last_attempt_at` mutation
    /// rewrites chunks with no row added — so a fingerprint over bytes
    /// never settles, and `grid_index` gets work to do forever on a
    /// pipeline where nothing changed.
    #[test]
    fn a_store_that_only_grew_does_not_re_render_the_report() {
        let before = vec![
            subject("s/raw", MeasurementKind::Tree, Some(1_000), Some(2)),
            subject(
                "s/raw/e.doltlite_db#t",
                MeasurementKind::Table,
                None,
                Some(7),
            ),
        ];
        let after = vec![
            subject("s/raw", MeasurementKind::Tree, Some(1_400), Some(2)),
            subject(
                "s/raw/e.doltlite_db#t",
                MeasurementKind::Table,
                None,
                Some(7),
            ),
        ];
        assert_eq!(
            fingerprint(&before),
            fingerprint(&after),
            "bytes moved but nothing was added; the report must not be rewritten"
        );
    }

    /// A new table with no rows yet is still a change: the subject set
    /// is part of the fingerprint, not just the counts in it.
    #[test]
    fn a_new_subject_re_renders_even_with_nothing_in_it() {
        let before = vec![subject("s/raw", MeasurementKind::Tree, Some(10), Some(1))];
        let after = vec![
            subject("s/raw", MeasurementKind::Tree, Some(10), Some(1)),
            subject(
                "s/raw/e.doltlite_db#new",
                MeasurementKind::Table,
                None,
                Some(0),
            ),
        ];
        assert_ne!(fingerprint(&before), fingerprint(&after));
    }

    /// A source with no raw store yet is not an error and not an empty
    /// document — it simply has nothing to say.
    #[tokio::test]
    async fn a_source_with_no_raw_store_measures_nothing() {
        let td = tempdir().unwrap();
        assert!(scan(td.path(), "never-ran").await.unwrap().is_empty());
        assert!(plan(
            td.path(),
            "never-ran",
            Vec::new(),
            "2026-09-07T10:00:00-07:00"
        )
        .unwrap()
        .is_none());
    }

    /// The whole point of the scan: real files, real tables, real
    /// counts, read back off a store the pipeline itself would write.
    #[tokio::test]
    async fn a_store_is_measured_by_file_and_by_table() {
        let td = tempdir().unwrap();
        let raw = td.path().join("src/raw");
        std::fs::create_dir_all(&raw).unwrap();
        let db = raw.join("entities.doltlite_db");
        let pool = datalib_core::store::open_pool(&db).await.unwrap();
        sqlx::query("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE threads (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        for i in 0..3 {
            sqlx::query("INSERT INTO messages (id, body) VALUES (?, 'hi')")
                .bind(i)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;

        let subjects = scan(td.path(), "src").await.unwrap();

        let tree = subjects
            .iter()
            .find(|s| s.kind == MeasurementKind::Tree)
            .expect("a tree row");
        assert_eq!(tree.path, "src/raw");
        // Two, not one: doltlite leaves a zero-byte
        // `.<name>.doltlite_db-lock` sidecar beside every store, and the
        // tree total counts what is actually on disk rather than only
        // the files we think of as ours.
        assert_eq!(tree.items, Some(2), "the store plus its lock sidecar");
        assert!(tree.bytes.unwrap() > 0);

        let store = subjects
            .iter()
            .find(|s| s.kind == MeasurementKind::Store)
            .expect("a store row");
        assert_eq!(store.path, "src/raw/entities.doltlite_db");
        assert_eq!(
            store.bytes,
            Some(db.symlink_metadata().unwrap().len() as i64)
        );

        let messages = subjects
            .iter()
            .find(|s| s.path.ends_with("#messages"))
            .expect("a messages table row");
        assert_eq!(messages.items, Some(3));
        assert_eq!(
            messages.bytes, None,
            "a content-addressed store has no per-table byte layout to report"
        );

        assert!(
            subjects.iter().any(|s| s.path.ends_with("#threads")),
            "an empty table is still a table: {subjects:?}"
        );
    }

    /// Datalib's own run bookkeeping stays out of the report, checked
    /// against a store opened the way a provider's really is.
    ///
    /// `doltlite_raw::open` is the framework's own entry point and
    /// applies `SHARED_DDL` itself, so this store holds whatever the
    /// framework actually creates rather than what this test remembers
    /// it creating — a fourth shared table would appear here and fail
    /// the assertion.
    ///
    /// The one that matters is `sync_runs`: it gains a row every run,
    /// so counting it would move the fingerprint on a pipeline where
    /// nothing changed — the regression `ingested_tng_test`'s "run 2
    /// reads 0 documents" assertion caught.
    #[tokio::test]
    async fn run_bookkeeping_is_not_reported_as_source_data() {
        let td = tempdir().unwrap();
        let raw = td.path().join("src/raw");
        let pool = datalib_etl::doltlite_raw::open(
            &raw.join("entities.doltlite_db"),
            &[
                "CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY)",
                &datalib_etl::doltlite_raw::bookkeeping_ddl_for("messages"),
            ],
        )
        .await
        .expect("open a raw store the framework's way");

        // A run's worth of bookkeeping, so the excluded tables are
        // non-empty and their absence from the report is a real
        // exclusion rather than an empty-table coincidence.
        sqlx::query("INSERT INTO sync_runs (started_at, config, status) VALUES ('t', '{}', 'ok')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (id) VALUES ('m1')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let subjects = scan(td.path(), "src").await.unwrap();
        let tables: Vec<String> = subjects
            .iter()
            .filter(|s| s.kind == MeasurementKind::Table)
            .map(|s| s.path.rsplit('#').next().unwrap().to_string())
            .collect();
        assert_eq!(
            tables,
            ["messages"],
            "only the source's own tables belong in the report"
        );
        assert_eq!(
            subjects
                .iter()
                .find(|s| s.path.ends_with("#messages"))
                .and_then(|s| s.items),
            Some(1),
            "and the source's own count is the real one"
        );
    }

    /// The exclusion has to survive a *second* run. This is the shape
    /// of the bug that shipped: run once, run again with no new data,
    /// and the report must fingerprint identically — which it only does
    /// if the run counter never reached it.
    #[tokio::test]
    async fn a_second_run_over_unchanged_data_fingerprints_identically() {
        let td = tempdir().unwrap();
        let raw = td.path().join("src/raw");
        let db = raw.join("entities.doltlite_db");
        let open = || {
            datalib_etl::doltlite_raw::open(
                &db,
                &["CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY)"],
            )
        };

        let pool = open().await.expect("first run");
        sqlx::query("INSERT INTO sync_runs (started_at, config, status) VALUES ('t1', '{}', 'ok')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (id) VALUES ('m1')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let first = fingerprint(&scan(td.path(), "src").await.unwrap());

        // A second run: another `sync_runs` row, no new content.
        let pool = open().await.expect("second run");
        sqlx::query("INSERT INTO sync_runs (started_at, config, status) VALUES ('t2', '{}', 'ok')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let second = fingerprint(&scan(td.path(), "src").await.unwrap());

        assert_eq!(
            first, second,
            "a run that added nothing but a run-log row must not re-render the report"
        );

        // …and real new data still does.
        let pool = open().await.expect("third run");
        sqlx::query("INSERT INTO messages (id) VALUES ('m2')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert_ne!(
            first,
            fingerprint(&scan(td.path(), "src").await.unwrap()),
            "a real row must still re-render it"
        );
    }

    /// Every table the framework gives a provider for free is excluded
    /// — checked against `SHARED_DDL` itself, so adding a fourth shared
    /// table (or renaming one) fails here instead of silently landing
    /// in the report as if it were the source's data.
    #[test]
    fn every_shared_framework_table_is_excluded() {
        let shared: Vec<&str> = datalib_etl::doltlite_raw::SHARED_DDL
            .iter()
            .map(|ddl| table_name_in(ddl).expect("every SHARED_DDL entry names a table"))
            .collect();
        assert!(
            shared.contains(&"sync_runs"),
            "the run log is the one that must never be counted: {shared:?}"
        );
        for table in &shared {
            assert!(is_datalib_bookkeeping(table), "{table} reached the report");
        }
    }

    /// The sidecar suffix comes from the generator, not from memory.
    #[test]
    fn the_sidecar_the_framework_generates_is_excluded() {
        let ddl = datalib_etl::doltlite_raw::bookkeeping_ddl_for("widgets");
        let generated = table_name_in(&ddl).expect("bookkeeping DDL names a table");
        assert_eq!(generated, "widgets_bookkeeping");
        assert!(is_datalib_bookkeeping(generated));
    }

    /// The predicate has to be narrow as well as complete: a provider
    /// table that merely *reads* like bookkeeping stays in the report.
    #[test]
    fn provider_tables_are_not_mistaken_for_bookkeeping() {
        for kept in [
            "messages",
            "cas_objects",
            // Close enough to the framework names to catch a predicate
            // written with `starts_with("sync")` or a substring match.
            "sync_points",
            "runs",
            "bookkeeping",
            "sync_runs_archive",
        ] {
            assert!(!is_datalib_bookkeeping(kept), "{kept} was dropped");
        }
    }

    #[test]
    fn a_statement_this_cannot_parse_excludes_nothing() {
        assert_eq!(table_name_in("CREATE INDEX foo ON bar (baz)"), None);
        assert_eq!(table_name_in("CREATE TABLE plain (x INT)"), None);
        assert_eq!(
            table_name_in("CREATE TABLE IF NOT EXISTS t(x INT)"),
            Some("t")
        );
    }

    /// The report is a real file on disk with the anchors the preview
    /// pane walks, and `md_path` points at it — a document whose file
    /// is missing serves as a blank page.
    #[tokio::test]
    async fn the_report_is_written_where_the_document_says_it_is() {
        let td = tempdir().unwrap();
        let subjects = vec![
            subject("s/raw", MeasurementKind::Tree, Some(2048), Some(2)),
            subject(
                "s/raw/entities.doltlite_db#msgs",
                MeasurementKind::Table,
                None,
                Some(7),
            ),
        ];
        let m = plan(td.path(), "s", subjects, "2026-09-07T10:00:00-07:00")
            .unwrap()
            .unwrap();
        assert!(
            !m.doc.md_path.exists(),
            "planning must not touch the disk; only write_report may"
        );
        m.write_report().expect("write the report");
        let body = std::fs::read_to_string(&m.doc.md_path).expect("the report exists");
        for row in &m.doc.rows {
            assert!(
                body.contains(&format!("data-section-uuid=\"{}\"", row.uuid)),
                "every row needs its anchor in the body: {body}"
            );
        }
        assert!(
            body.contains("2.0 KiB"),
            "the report body shows the size: {body}"
        );
        assert!(body.contains("2 files"), "a tree counts files: {body}");
        assert!(body.contains("7 rows"), "a table counts rows: {body}");
    }

    /// One sample per measurement, all stamped with the run's pinned
    /// `now` rather than each sampling its own clock.
    #[test]
    fn every_measurement_becomes_one_sample_at_the_runs_own_time() {
        let td = tempdir().unwrap();
        let subjects = vec![
            subject("s/raw", MeasurementKind::Tree, Some(1), Some(1)),
            subject("s/raw/x.doltlite_db", MeasurementKind::Store, Some(1), None),
        ];
        let m = plan(
            td.path(),
            "s",
            subjects.clone(),
            "2026-09-07T10:00:00-07:00",
        )
        .unwrap()
        .unwrap();
        assert_eq!(m.samples.len(), subjects.len());
        assert!(m
            .samples
            .iter()
            .all(|s| s.measured_at == "2026-09-07T10:00:00-07:00"));
    }

    /// The size must reach the row and the report, but never
    /// `grid_rows.text` — which `compute_row_set_hash` covers. A
    /// doltlite store's size differs between machines, so a byte
    /// figure in that string made the fixture golden unable to pass on
    /// CI and a developer's machine at once.
    #[test]
    fn the_hashed_text_carries_no_byte_figure() {
        let s = subject("s/raw", MeasurementKind::Tree, Some(2048), Some(2));
        assert_eq!(s.summary(), "s/raw — 2 files");
        assert!(
            !s.summary().contains("KiB") && !s.summary().contains("2048"),
            "the hashed text must not carry a size: {}",
            s.summary()
        );
        // …and it is not lost: the report's Size cell says it, and so
        // does the column.
        let body = report_body("s", std::slice::from_ref(&s), "2026-09-07T10:00:00-07:00");
        assert!(body.contains("| 2.0 KiB |"), "{body}");

        let td = tempdir().unwrap();
        let m = plan(td.path(), "s", vec![s], "2026-09-07T10:00:00-07:00")
            .unwrap()
            .unwrap();
        assert_eq!(m.doc.rows[0].byte_size, Some(2048));
    }

    /// The report is a table, and every measurement is one row of it.
    /// It used to be a bordered `<div>` per measurement — a dozen cards
    /// stacked down the page to say a dozen short facts.
    #[test]
    fn the_report_is_one_table_row_per_measurement() {
        let subjects = vec![
            subject("s/raw", MeasurementKind::Tree, Some(2048), Some(2)),
            subject(
                "s/raw/db.doltlite_db",
                MeasurementKind::Store,
                Some(512),
                None,
            ),
        ];
        let body = report_body("s", &subjects, "2026-09-07T10:00:00-07:00");

        assert!(
            body.contains("| Kind | What | Size | Count | Id |"),
            "{body}"
        );
        assert!(!body.contains("<div"), "no per-measurement card: {body}");
        // Two measurements, two rows — plus the header and its rule.
        assert_eq!(
            body.lines().filter(|l| l.starts_with('|')).count(),
            4,
            "{body}"
        );
        // Every row still carries the anchor the grid scrolls to.
        for s in &subjects {
            let uuid = s.uuid("s");
            assert!(
                body.contains(&format!("data-section-uuid=\"{uuid}\"")),
                "row for {} lost its anchor: {body}",
                s.path
            );
        }
    }

    #[test]
    fn bytes_read_in_the_units_a_person_thinks_in() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
