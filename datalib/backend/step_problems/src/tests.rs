use super::*;
use datalib_sql::{test_sql, test_sql_id, Sql};

const STEP: &str = "enterprise/render_markdown";

fn found(scope_key: &str, sample: &str) -> Found {
    Found {
        stage: Stage::Render,
        scope_kind: ScopeKind::Entity,
        scope_key: scope_key.to_string(),
        outcome: Outcome::Dropped,
        reason: Reason::RenderFailed,
        severity: Some(Severity::Warning),
        sample: sample.to_string(),
        item_uuid: None,
        field: None,
        path: None,
        rule: None,
        upstream_entity_kind: None,
        upstream_id: None,
        upstream_account: None,
    }
}

fn row(scope_key: &str) -> ProblemRow {
    found(scope_key, "x").into_row("enterprise")
}

fn at(day: u32) -> StoredStamp {
    datalib_time::split_stamp(&format!("2370-01-{day:02}T00:00:00+02:00"))
}

async fn run(
    root: &Path,
    day: u32,
    found: Vec<Found>,
    covered: &[Covered],
    ended: Ended,
) -> Vec<ProblemRow> {
    let mut file = StepFile::open(root, STEP, at(day)).await.unwrap();
    file.record(
        found
            .into_iter()
            .map(|f| f.into_row("enterprise"))
            .collect(),
    )
    .await
    .unwrap();
    file.finish(covered, ended).await.unwrap();
    read(&file_of(root, STEP)).await.unwrap()
}

fn keys(rows: &[ProblemRow]) -> Vec<&str> {
    rows.iter().map(|r| r.scope_key.as_str()).collect()
}

/// A run that ended short vouches only for the records it named;
/// listings, phases and "every record" wait for a run that succeeds.
#[test]
fn a_short_run_clears_only_what_it_named() {
    let records = |all_but: Option<Vec<&str>>, only: Option<Vec<&str>>| Covered::Records {
        table: "t".into(),
        all_but: all_but.map(|v| v.into_iter().map(String::from).collect()),
        only: only.map(|v| v.into_iter().map(String::from).collect()),
    };
    let named = records(Some(vec!["2"]), None);
    let every = records(None, None);
    let rendered = records(None, Some(vec!["3"]));
    for (row, covered, short, whole) in [
        (row("t:1"), named.clone(), true, true),
        (row("t:2"), named.clone(), false, false),
        (row("t:1"), every.clone(), false, true),
        (row("u:1"), every.clone(), false, false),
        (row("tt:1"), every.clone(), false, false),
        (row("listing:channels"), Covered::Listings, false, true),
        (row("phase:attachments"), Covered::Listings, false, true),
        (row("t:1"), Covered::Listings, false, false),
        (row("anything"), Covered::All, false, true),
        (row("t:3"), rendered.clone(), true, true),
        (row("t:4"), rendered.clone(), false, false),
    ] {
        let c = std::slice::from_ref(&covered);
        assert_eq!(
            clears(&row, c, Ended::Short),
            short,
            "{} {covered:?} short",
            row.scope_key
        );
        assert_eq!(
            clears(&row, c, Ended::Succeeded),
            whole,
            "{} {covered:?}",
            row.scope_key
        );
    }
}

#[test]
fn a_file_is_covered_by_the_folder_it_is_under_and_not_by_a_neighbour() {
    let under = |all_but: Option<Vec<String>>| Covered::Files {
        under: "deck8/".into(),
        all_but,
    };
    let c = [under(Some(vec!["deck8/b.fit".into()]))];
    assert!(clears(&row("file:deck8/a.fit"), &c, Ended::Short));
    assert!(!clears(&row("file:deck8/b.fit"), &c, Ended::Short));
    assert!(!clears(&row("file:deck80/a.fit"), &c, Ended::Short));
    assert!(!clears(
        &row("file:deck8/a.fit"),
        &[under(None)],
        Ended::Short
    ));
    assert!(clears(
        &row("file:deck8/a.fit"),
        &[under(None)],
        Ended::Succeeded
    ));
}

/// The file across three runs: what a run finds is written as found and
/// stamped against the row before; what it covered and did not find
/// again goes; a run that failed takes nothing it did not name.
#[tokio::test]
async fn the_file_keeps_what_was_found_until_a_run_that_covered_it_does_not_find_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let all = [Covered::All];
    let rows = run(
        root,
        1,
        vec![found("t:1", "404"), found("listing:c", "500")],
        &all,
        Ended::Succeeded,
    )
    .await;
    assert_eq!(keys(&rows), ["listing:c", "t:1"]);
    assert_eq!(rows[0].source_id, "enterprise");

    let rows = run(root, 2, vec![found("t:2", "404")], &all, Ended::Short).await;
    assert_eq!(keys(&rows), ["listing:c", "t:1", "t:2"]);

    let rows = run(root, 3, vec![found("t:1", "404")], &all, Ended::Succeeded).await;
    assert_eq!(keys(&rows), ["t:1"]);
    assert_eq!(rows[0].first_seen_at_utc, at(1).utc);
    assert_eq!(rows[0].changed_at_utc, at(1).utc, "found again unchanged");
}

/// A reader sees a batch the writer has committed while the writer
/// still holds the file, which is what makes a problem show while its
/// step runs.
#[tokio::test]
async fn a_reader_sees_a_committed_batch_while_the_run_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = StepFile::open(dir.path(), STEP, at(1)).await.unwrap();
    file.record(vec![row("t:1")]).await.unwrap();
    let seen = read(&file_of(dir.path(), STEP)).await.unwrap();
    assert_eq!(keys(&seen), ["t:1"]);
    file.finish(&[], Ended::Succeeded).await.unwrap();
}

#[tokio::test]
async fn no_file_is_no_problems() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read(&file_of(dir.path(), STEP)).await.unwrap().is_empty());
}

/// The wire shapes the protocol documents, and the upstream record a
/// problem names reaching its row.
#[test]
fn the_lines_read_as_the_protocol_spells_them() {
    let f: Found = serde_json::from_str(
        r#"{"event":"problem","stage":"fetch","scope_kind":"entity","scope_key":"messages:C01/1","outcome":"dropped","reason":"fetch_failed","severity":"error","upstream_entity_kind":"message","upstream_id":"C01/1"}"#,
    )
    .unwrap();
    let row = f.into_row("enterprise");
    assert_eq!(row.severity, Severity::Error);
    assert_eq!(row.upstream_id.as_deref(), Some("C01/1"));
    let c: Covered = serde_json::from_str(
        r#"{"event":"covered","kind":"records","table":"messages","only":["C01/2"]}"#,
    )
    .unwrap();
    assert_eq!(
        c,
        Covered::Records {
            table: "messages".into(),
            all_but: None,
            only: Some(vec!["C01/2".into()]),
        }
    );
    let all: Covered = serde_json::from_str(r#"{"event":"covered","kind":"all"}"#).unwrap();
    assert_eq!(all, Covered::All);
}

async fn insert(pool: &sqlx::SqlitePool, row: &ProblemRow) {
    // Audited: `insert_sql` is built from `ProblemRow`'s consts.
    row.bind_into(Sql::dynamic(test_sql_id!(), datalib_table::insert_sql::<ProblemRow>()).query())
        .execute(pool)
        .await
        .unwrap();
}

/// A step's problems are read from every store in its folder: a raw
/// store as its last seal left it, never the rows its writer holds
/// uncommitted; and the step's own `problems.sqlite` beside it.
#[tokio::test]
async fn a_steps_problems_are_its_stores_rows_as_last_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let step = "riker/ingest";
    let store = dir.path().join(step).join("entities.doltlite_db");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    let pool = datalib_etl::doltlite_raw::open(&store, &[]).await.unwrap();
    insert(&pool, &found("messages:1", "404").into_row("riker")).await;
    datalib_etl::doltlite_raw::commit_run(&pool, "seal")
        .await
        .unwrap();
    insert(&pool, &found("messages:2", "404").into_row("riker")).await;

    let mut file = StepFile::open(dir.path(), step, at(1)).await.unwrap();
    file.record(vec![row("listing:channels")]).await.unwrap();
    file.finish(&[], Ended::Succeeded).await.unwrap();

    let read = of_step(dir.path(), step).await;
    pool.close().await;
    assert!(read.unread.is_empty(), "{:?}", read.unread);
    let mut seen: Vec<(&str, &str)> = read
        .rows
        .iter()
        .map(|r| (r.scope_key.as_str(), r.source_id.as_str()))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [("listing:channels", "enterprise"), ("messages:1", "riker")]
    );
}

/// `replace`, for a step that finds every problem again on each run: the
/// run's rows are the store's whole; one found again unchanged keeps its
/// stamps, one fixed since is gone, and two reasons for one record are
/// one row saying both.
#[tokio::test]
async fn replace_keeps_the_stamps_of_the_unchanged_and_joins_two_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let file = StepFile::open(dir.path(), STEP, at(1)).await.unwrap();
    let pool = file.pool.clone();
    replace(&pool, vec![row("a:1"), row("b:1")], &at(1))
        .await
        .unwrap();
    let mut twice = found("c:1", "x").into_row("enterprise");
    let again = ProblemRow {
        sample: "y".into(),
        ..twice.clone()
    };
    twice.sample = "x".into();
    replace(&pool, vec![row("a:1"), twice, again], &at(2))
        .await
        .unwrap();
    let rows = read(&file_of(dir.path(), STEP)).await.unwrap();
    assert_eq!(keys(&rows), ["a:1", "c:1"]);
    assert_eq!(
        rows[0].changed_at_utc,
        at(1).utc,
        "unchanged keeps its stamp"
    );
    assert_eq!(rows[1].changed_at_utc, at(2).utc);
    assert_eq!(rows[1].sample, "x; y");
    file.finish(&[], Ended::Succeeded).await.unwrap();
}

async fn plain(path: &std::path::Path) -> sqlx::SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(path))
        .await
        .unwrap()
}

/// A store whose `_datalib_meta` names another `problems` shape is
/// reported as in another shape, by what it records and without asking
/// the table, rather than read around or as a failure: a turned-off
/// render kept one for good, and the Manage screen logged it as an error
/// on every refresh.
#[tokio::test]
async fn a_problems_table_in_another_shape_is_reported_by_its_recorded_shape() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = StepFile::open(dir.path(), STEP, at(1)).await.unwrap();
    file.record(vec![row("messages:1")]).await.unwrap();
    file.finish(&[], Ended::Succeeded).await.unwrap();
    let pool = plain(&file_of(dir.path(), STEP)).await;
    test_sql!("ALTER TABLE problems DROP COLUMN upstream_entity_kind")
        .query()
        .execute(&pool)
        .await
        .unwrap();
    test_sql!("UPDATE _datalib_meta SET value = '0' WHERE key = 'shared_schema_version'")
        .query()
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let read = of_step(dir.path(), STEP).await;
    assert!(read.unread.is_empty(), "{:?}", read.unread);
    assert!(read.rows.is_empty());
    assert_eq!(read.other_shape, [file_of(dir.path(), STEP)]);
    let steps = of_steps(dir.path()).await;
    assert_eq!(steps.len(), 1, "the step is listed for what it holds");

    let file = StepFile::open(dir.path(), STEP, at(2)).await.unwrap();
    file.finish(&[], Ended::Succeeded).await.unwrap();
    let read = of_step(dir.path(), STEP).await;
    assert!(
        read.other_shape.is_empty(),
        "its next run writes this shape"
    );
    assert!(read.unread.is_empty(), "{:?}", read.unread);
}

/// A render store whose step an older build ran last, and nothing has run
/// since: the folder of a source taken out of the config keeps one for
/// good. It lacks a table this build reads, and was logged as an error at
/// every start.
#[tokio::test]
async fn a_render_store_without_the_table_this_build_reads_is_in_another_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("slack/render_markdown/indexed_markdown.sqlite");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(&path).create_if_missing(true))
        .await
        .unwrap();
    Sql::dynamic(test_sql_id!(), datalib_problems::DDL[0].1)
        .query()
        .execute(&pool)
        .await
        .unwrap();
    let shape = datalib_store_meta::Shape {
        versions: StepFile::shape().versions,
        ..datalib_store_meta::Shape::of_tables("an older build's".into())
    };
    datalib_store_meta::write(&pool, datalib_store_meta::StoreKind::Render, &shape)
        .await
        .unwrap();
    let read = of_step(dir.path(), "slack/render_markdown").await;
    assert!(read.unread.is_empty(), "{:?}", read.unread);
    assert_eq!(read.other_shape, std::slice::from_ref(&path));

    // The same file once its step has run again.
    test_sql!("CREATE TABLE entity_items (entity_key TEXT PRIMARY KEY, item_uuid TEXT)")
        .query()
        .execute(&pool)
        .await
        .unwrap();
    insert(&pool, &row("messages:1")).await;
    pool.close().await;
    let read = of_step(dir.path(), "slack/render_markdown").await;
    assert!(read.unread.is_empty() && read.other_shape.is_empty());
    assert_eq!(keys(&read.rows), ["messages:1"]);
}

/// A step's own file from before it recorded its shape is in another
/// shape, said so rather than read as holding nothing.
#[tokio::test]
async fn a_steps_own_file_that_records_no_shape_is_in_another_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_of(dir.path(), STEP);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(&path).create_if_missing(true))
        .await
        .unwrap();
    Sql::dynamic(test_sql_id!(), datalib_problems::DDL[0].1)
        .query()
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let read = of_step(dir.path(), STEP).await;
    assert_eq!(read.other_shape, [path]);
}

/// A file that records no kind of store holding problems — a blob store
/// beside a raw store — gives none, without a question to its tables.
#[tokio::test]
async fn a_file_that_records_no_problems_gives_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STEP).join("blobs.sqlite");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(&path).create_if_missing(true))
        .await
        .unwrap();
    test_sql!("CREATE TABLE blobs (hash TEXT)")
        .query()
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let read = of_step(dir.path(), STEP).await;
    assert!(read.unread.is_empty(), "{:?}", read.unread);
    assert!(read.rows.is_empty() && read.other_shape.is_empty());
}

/// Each thing a reader could not read is said the first pass it appears
/// in and not again while it stays, and again once it has gone and come
/// back.
#[test]
fn what_could_not_be_read_is_said_once_while_it_lasts() {
    use std::collections::BTreeSet;
    let one = |what: &str, older: bool| (STEP.to_string(), what.to_string(), older);
    let pass1: BTreeSet<Said> = [one("locked", false), one("old.doltlite_db", true)].into();
    assert_eq!(newly(&BTreeSet::new(), &pass1).len(), 2);
    assert!(newly(&pass1, &pass1).is_empty(), "nothing new to say");
    let pass3: BTreeSet<Said> = [one("old.doltlite_db", true)].into();
    assert!(newly(&pass1, &pass3).is_empty());
    assert_eq!(newly(&pass3, &pass1), [one("locked", false)], "back again");
}
