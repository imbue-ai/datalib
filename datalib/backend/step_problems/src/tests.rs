use super::*;

const STEP: &str = "enterprise/ingest";

fn found(key: &str, sample: &str) -> Found {
    Found {
        key: key.to_string(),
        severity: Severity::Warning,
        outcome: "dropped".into(),
        reason: "fetch_failed".into(),
        sample: sample.to_string(),
        about: About::default(),
        record: key.strip_prefix("record:").and_then(|rest| {
            let (table, id) = rest.split_once(':')?;
            Some(Record {
                table: table.into(),
                id: id.into(),
            })
        }),
    }
}

fn at(day: u32) -> StoredStamp {
    datalib_time::split_stamp(&format!("2370-01-{day:02}T00:00:00+02:00"))
}

fn row(key: &str) -> Row {
    stamped(STEP, found(key, "x"), None, &at(1), "r1")
}

fn file_row(path: &str) -> Row {
    let mut f = found(&format!("file:{path}"), "x");
    f.about.path = Some(path.to_string());
    stamped(STEP, f, None, &at(1), "r1")
}

async fn run(
    root: &Path,
    id: &str,
    day: u32,
    found: Vec<Found>,
    covered: &[Covered],
    ended: Ended,
) -> Vec<Row> {
    let file = StepFile::open(root, STEP, id, at(day)).await.unwrap();
    file.record(found).await.unwrap();
    file.finish(covered, ended).await.unwrap();
    read(&file_of(root, STEP)).await.unwrap()
}

fn keys(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|r| r.key.as_str()).collect()
}

#[test]
fn a_problem_found_again_unchanged_keeps_its_stamps_and_a_changed_one_moves() {
    let first = stamped(STEP, found("record:t:1", "404"), None, &at(1), "r1");
    let again = stamped(STEP, found("record:t:1", "404"), Some(&first), &at(2), "r2");
    assert_eq!(again.changed_at_utc, first.changed_at_utc);
    assert_eq!(again.run, "r2");
    let changed = stamped(STEP, found("record:t:1", "500"), Some(&first), &at(3), "r3");
    assert_eq!(changed.first_seen_at_utc, first.first_seen_at_utc);
    assert_eq!(changed.changed_at_utc, at(3).utc);
    assert_eq!(
        first.problem_uuid, changed.problem_uuid,
        "the id is the key's"
    );
}

/// A run that ended short vouches only for the records it named;
/// listings, phases and "every record" wait for a run that succeeds.
#[test]
fn a_short_run_clears_only_what_it_named() {
    let named = Covered::Records {
        table: "t".into(),
        all_but: Some(vec!["2".into()]),
        only: None,
    };
    let every = Covered::Records {
        table: "t".into(),
        all_but: None,
        only: None,
    };
    let rendered = Covered::Records {
        table: "t".into(),
        all_but: None,
        only: Some(vec!["3".into()]),
    };
    for (row, covered, short, whole) in [
        (row("record:t:1"), named.clone(), true, true),
        (row("record:t:2"), named.clone(), false, false),
        (row("record:t:1"), every.clone(), false, true),
        (row("record:u:1"), every.clone(), false, false),
        (row("record:tt:1"), every.clone(), false, false),
        (row("listing:channels"), Covered::Listings, false, true),
        (row("phase:attachments"), Covered::Listings, false, true),
        (row("record:t:1"), Covered::Listings, false, false),
        (row("anything"), Covered::All, false, true),
        (row("record:t:3"), rendered.clone(), true, true),
        (row("record:t:4"), rendered.clone(), false, false),
    ] {
        let c = std::slice::from_ref(&covered);
        assert_eq!(
            clears(&row, c, Ended::Short),
            short,
            "{} {covered:?} short",
            row.key
        );
        assert_eq!(
            clears(&row, c, Ended::Succeeded),
            whole,
            "{} {covered:?}",
            row.key
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
    assert!(clears(&file_row("deck8/a.fit"), &c, Ended::Short));
    assert!(!clears(&file_row("deck8/b.fit"), &c, Ended::Short));
    assert!(!clears(&file_row("deck80/a.fit"), &c, Ended::Short));
    assert!(!clears(
        &file_row("deck8/a.fit"),
        &[under(None)],
        Ended::Short
    ));
    assert!(clears(
        &file_row("deck8/a.fit"),
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
        "r1",
        1,
        vec![found("record:t:1", "404"), found("listing:c", "500")],
        &all,
        Ended::Succeeded,
    )
    .await;
    assert_eq!(keys(&rows), ["listing:c", "record:t:1"]);

    let rows = run(
        root,
        "r2",
        2,
        vec![found("record:t:2", "404")],
        &all,
        Ended::Short,
    )
    .await;
    assert_eq!(keys(&rows), ["listing:c", "record:t:1", "record:t:2"]);

    let rows = run(
        root,
        "r3",
        3,
        vec![found("record:t:1", "404")],
        &all,
        Ended::Succeeded,
    )
    .await;
    assert_eq!(keys(&rows), ["record:t:1"]);
    assert_eq!(rows[0].first_seen_at_utc, at(1).utc);
    assert_eq!(rows[0].changed_at_utc, at(1).utc, "found again unchanged");
}

/// A reader sees a batch the writer has committed while the writer
/// still holds the file, which is what makes a problem show while its
/// step runs.
#[tokio::test]
async fn a_reader_sees_a_committed_batch_while_the_run_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let file = StepFile::open(dir.path(), STEP, "r1", at(1)).await.unwrap();
    file.record(vec![found("record:t:1", "404")]).await.unwrap();
    let seen = read(&file_of(dir.path(), STEP)).await.unwrap();
    assert_eq!(keys(&seen), ["record:t:1"]);
    file.finish(&[], Ended::Succeeded).await.unwrap();
}

#[tokio::test]
async fn no_file_is_no_problems() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read(&file_of(dir.path(), STEP)).await.unwrap().is_empty());
}

/// The wire shapes the protocol documents.
#[test]
fn the_events_read_as_the_protocol_spells_them() {
    let f: Found = serde_json::from_str(
        r#"{"key":"record:messages:C01/1","severity":"error","outcome":"dropped","reason":"fetch_failed","about":{"kind":"message","id":"C01/1"}}"#,
    )
    .unwrap();
    assert_eq!(f.severity, Severity::Error);
    assert_eq!(f.about.id.as_deref(), Some("C01/1"));
    let c: Covered =
        serde_json::from_str(r#"{"kind":"records","table":"messages","all_but":["C01/2"]}"#)
            .unwrap();
    assert_eq!(
        c,
        Covered::Records {
            table: "messages".into(),
            all_but: Some(vec!["C01/2".into()]),
            only: None,
        }
    );
    let all: Covered = serde_json::from_str(r#"{"kind":"all"}"#).unwrap();
    assert_eq!(all, Covered::All);
}
