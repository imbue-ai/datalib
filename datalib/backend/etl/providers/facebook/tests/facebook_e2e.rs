//! End-to-end test for the Facebook export provider: ingest the
//! checked-in TNG export, commit, render every feed, and read back what
//! landed — the tables, the CAS'd media, the documents.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_facebook::ingest::schema_raw::{
    ALBUMS_TABLE, COMMENTS_TABLE, FRIENDS_TABLE, POSTS_TABLE, PROFILE_TABLE, REACTIONS_TABLE,
};
use datalib_etl_facebook::ingest::{self, db_path_for, FetchOptions, RawDb};
use datalib_etl_facebook_render::processor::{render_source, Source};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;

fn fixture() -> PathBuf {
    let rel = std::env::var("FACEBOOK_TNG_FIXTURE").expect("FACEBOOK_TNG_FIXTURE is set by BUILD");
    let p = PathBuf::from(rel);
    if p.is_dir() {
        return p;
    }
    // Under `cargo test` the cwd is the crate root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/facebook_tng")
}

async fn rows(db: &RawDb, table: &str) -> Vec<serde_json::Value> {
    db.load_payloads(table).await.unwrap_or_default()
}

#[test]
fn ingests_the_export_and_renders_every_feed() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let export = fixture();
    let raw_dir = tmp.path().join("raw");
    fs::create_dir_all(&raw_dir)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async {
        // ── ingest ───────────────────────────────────────────────
        // The test owns the store: one connection for the ingest and the
        // assertions both, because the file takes one writer at a time.
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let summary = ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch")?;
        datalib_etl::store_handle::RawStoreHandle::commit_all(&db, "test: facebook fetch").await?;

        // 11 JSON files; `no-data.txt` and the HTML are not files to us.
        assert_eq!(summary.files, 11, "json files ingested");
        assert_eq!(summary.parse_errors, 0);
        // Four PNGs, each stored once however many records point at it;
        // the video the export left out is counted, not fatal.
        assert_eq!(summary.media_stored, 4, "distinct media files stored");
        assert_eq!(summary.media_missing, 1, "the missing video");

        assert_eq!(rows(&db, POSTS_TABLE).await.len(), 4, "timeline posts");
        assert_eq!(rows(&db, ALBUMS_TABLE).await.len(), 1);
        assert_eq!(rows(&db, COMMENTS_TABLE).await.len(), 3);
        // Both reaction files land in the one table.
        assert_eq!(rows(&db, REACTIONS_TABLE).await.len(), 4);
        assert_eq!(rows(&db, FRIENDS_TABLE).await.len(), 3);
        assert_eq!(rows(&db, PROFILE_TABLE).await.len(), 1);
        // A file nothing renders is ingested all the same, under its
        // path's slug with the chunk-index rule leaving `7_days` alone.
        assert_eq!(
            rows(&db, "logged_information_search_your_search_history")
                .await
                .len(),
            1
        );
        assert_eq!(
            rows(&db, "ads_information_story_views_in_past_7_days")
                .await
                .len(),
            1
        );

        // The mojibake is undone at ingest, so the raw store holds the
        // text the person wrote.
        let posts = rows(&db, POSTS_TABLE).await;
        let texts: Vec<&str> = posts
            .iter()
            .filter_map(|p| p.pointer("/data/0/post").and_then(|v| v.as_str()))
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("Tea, Earl Grey, hot. ☕")),
            "mojibake undone in the raw payload: {texts:?}"
        );

        // A second run is a no-op on the rows: same ids, nothing pruned,
        // every media file already known.
        let again = ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await?;
        assert_eq!(again.rows, summary.rows);
        assert_eq!(again.media_stored, 0);
        assert_eq!(again.media_known, summary.media_stored + summary.media_known);
        assert_eq!(rows(&db, POSTS_TABLE).await.len(), 4);

        // ── render ───────────────────────────────────────────────
        // `render_source` opens the store itself, so hand the file over
        // first: one doltlite file takes one connection at a time.
        db.close().await;
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&out_dir)?;
        let source = Source {
            raw_dir: &raw_dir,
            out_dir: &out_dir,
            name: "facebook",
            range: RawRange::cold(),
        };
        let mut docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                docs.push(d);
                Ok(())
            };
            render_source(&source, &Progress::noop(), &mut on_doc).context("render")?;
        }

        // 5 posts (4 timeline + 1 on another page) + 1 album + a year of
        // comments + a year of reactions + 3 friends.
        assert_eq!(docs.len(), 5 + 1 + 1 + 1 + 3, "documents rendered");
        let all_rows: Vec<_> = docs.iter().flat_map(|d| d.rows.iter()).collect();
        assert!(
            all_rows
                .iter()
                .all(|r| r.account.as_deref() == Some("picard@enterprise.starfleet")),
            "every row names the export's owner as its account"
        );
        assert!(all_rows.iter().all(|r| r.provider == "facebook"));

        // The check-in post: text, the place with its page link, and the
        // ☕ that the export had mangled.
        let check_in = docs
            .iter()
            .find(|d| d.rows.iter().any(|r| r.preview.contains("Tea, Earl Grey, hot.")))
            .expect("check-in post rendered");
        let md = fs::read_to_string(&check_in.md_path)?;
        assert!(md.contains("Tea, Earl Grey, hot. ☕"), "{md}");
        assert!(
            md.contains("📍 [Ten Forward](https://www.facebook.com/pages/Ten-Forward/300000000000001) — Deck 10, USS Enterprise"),
            "one place line, with the page URL: {md}"
        );
        assert!(check_in.rows.iter().any(|r| r.kind == "Facebook Post"));
        assert!(check_in.rows.iter().any(|r| r.kind == "Facebook Post Message"));

        // The photo post: its two images were CAS'd at ingest and are now
        // materialized beside the page, so the markdown embeds them.
        let photo_post = docs
            .iter()
            .find(|d| d.rows.iter().any(|r| r.preview.contains("Two views of the bridge")))
            .expect("photo post rendered");
        let md = fs::read_to_string(&photo_post.md_path)?;
        assert_eq!(
            md.matches("blobs/").count(),
            2,
            "two materialized images: {md}"
        );
        let blobs_dir = photo_post.md_path.parent().unwrap().join("blobs");
        assert_eq!(
            fs::read_dir(&blobs_dir)?.count(),
            2,
            "two files under {}",
            blobs_dir.display()
        );
        assert!(md.contains("The bridge at dawn."), "photo caption: {md}");
        assert!(md.contains("— with Data"), "tag: {md}");

        // The album: description then two photos, one captioned.
        let album = docs
            .iter()
            .find(|d| d.rows.iter().any(|r| r.kind == "Facebook Album"))
            .expect("album rendered");
        let md = fs::read_to_string(&album.md_path)?;
        assert!(md.contains("Off-duty evenings on Deck 10."), "{md}");
        assert!(md.contains("Guinan behind the bar."), "{md}");
        assert_eq!(
            album.rows.iter().filter(|r| r.kind == "Facebook Photo").count(),
            2
        );

        // Comments: one document per year, the title folded in, the
        // mojibake'd 🍸 restored.
        let comments: Vec<_> = docs
            .iter()
            .filter(|d| d.rows.iter().any(|r| r.kind == "Facebook Comment"))
            .collect();
        let [year] = comments.as_slice() else {
            let paths: Vec<_> = comments.iter().map(|d| &d.md_path).collect();
            panic!("two months of comments are one year's document: {paths:?}")
        };
        assert_eq!(year.md_path.file_name().unwrap(), "2369.md");
        let md = fs::read_to_string(&year.md_path)?;
        assert!(md.contains("Guinan's synthehol never disappoints. 🍸"), "{md}");
        assert!(md.contains("*Jean-Luc Picard commented on his own album.*"), "{md}");

        // Reactions: two events from four rows, each with its linkout.
        let reactions: Vec<_> = docs
            .iter()
            .filter(|d| d.rows.iter().any(|r| r.kind == "Facebook Reaction"))
            .collect();
        assert_eq!(reactions.len(), 1, "two months of reactions, one year");
        let reaction_rows: Vec<_> = reactions
            .iter()
            .flat_map(|d| d.rows.iter())
            .filter(|r| r.kind == "Facebook Reaction")
            .collect();
        assert_eq!(reaction_rows.len(), 2, "one row per reaction, not per file");
        assert!(reaction_rows.iter().any(|r| r.source_url.as_deref()
            == Some("https://www.facebook.com/will.riker/posts/pfbid0RIKER")));

        // Friends: three contacts, filed under one channel.
        let friends: Vec<_> = docs
            .iter()
            .filter(|d| d.rows.iter().any(|r| r.kind == "Contact"))
            .collect();
        assert_eq!(friends.len(), 3);
        assert!(friends.iter().all(|d| d
            .rows
            .iter()
            .all(|r| r.channel.as_deref() == Some("Friends"))));

        // Every document declares the rows it read, so a change to any of
        // them renders it again; every one includes the profile row.
        for d in &docs {
            assert!(d.bucket_key.is_some(), "{} has a bucket", d.md_path.display());
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

// ── what a run could not read ──────────────────────────────────────

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

const ALBUM_1: &str = "your_facebook_activity/posts/album/1.json";

/// A private copy of the fixture export with a second album chunk, so
/// the album table is fed by two files, and a store.
struct Export {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    db: RawDb,
}

impl Export {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("export");
        copy_tree(&fixture(), &root);
        fs::write(
            root.join(ALBUM_1),
            r#"{"name": "Holodeck Three", "photos": [], "description": "Dixon Hill."}"#,
        )
        .unwrap();
        let db = RawDb::open(&db_path_for(&tmp.path().join("raw")))
            .await
            .unwrap();
        Self {
            _tmp: tmp,
            root,
            db,
        }
    }

    async fn sync(&self) -> ingest::FetchSummary {
        ingest::fetch(FetchOptions {
            db: self.db.clone(),
            input_path: self.root.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .unwrap()
    }

    async fn problems(&self) -> Vec<(String, String, String, String)> {
        sqlx::query_as(
            "SELECT scope_key, severity, reason, sample FROM problems ORDER BY scope_key",
        )
        .fetch_all(self.db.pool())
        .await
        .unwrap()
    }
}

/// A chunk that would not read leaves the table it shares with its
/// siblings unpruned — its rows are not in this run's set, and that says
/// nothing about whether they went — and is a `listing:` row until it
/// reads again.
#[tokio::test(flavor = "multi_thread")]
async fn a_chunk_that_will_not_read_keeps_its_rows_and_is_a_problem_until_it_reads() {
    let e = Export::new().await;
    e.sync().await;
    assert_eq!(rows(&e.db, ALBUMS_TABLE).await.len(), 2);

    let good = fs::read(e.root.join(ALBUM_1)).unwrap();
    fs::write(e.root.join(ALBUM_1), "{\"name\": ").unwrap();
    let s = e.sync().await;
    assert_eq!(s.parse_errors, 1);
    assert_eq!(rows(&e.db, ALBUMS_TABLE).await.len(), 2, "nothing pruned");
    let keys: Vec<String> = e.problems().await.into_iter().map(|r| r.0).collect();
    assert!(
        keys.contains(&format!("listing:file {ALBUM_1}")),
        "{keys:?}"
    );

    fs::write(e.root.join(ALBUM_1), good).unwrap();
    e.sync().await;
    let keys: Vec<String> = e.problems().await.into_iter().map(|r| r.0).collect();
    assert!(keys.iter().all(|k| !k.starts_with("listing:")), "{keys:?}");
    e.db.clone().close().await;
}

/// A walk that could not read part of the export deletes nothing: a
/// directory it failed to list looks exactly like one whose files went.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_error_deletes_nothing() {
    let e = Export::new().await;
    e.sync().await;

    fs::remove_file(e.root.join(ALBUM_1)).unwrap();
    std::os::unix::fs::symlink(e.root.join("nowhere"), e.root.join("lost_album")).unwrap();
    e.sync().await;
    assert_eq!(rows(&e.db, ALBUMS_TABLE).await.len(), 2, "nothing pruned");
    let keys: Vec<String> = e.problems().await.into_iter().map(|r| r.0).collect();
    assert!(keys.contains(&"listing:files".to_string()), "{keys:?}");
    e.db.clone().close().await;
}

/// A media file the export left out is a warning on its edge — the
/// export is what it is, nothing failed — and says what the read said.
#[tokio::test(flavor = "multi_thread")]
async fn a_media_file_the_export_left_out_is_a_warning() {
    let e = Export::new().await;
    e.sync().await;
    let media: Vec<_> = e
        .problems()
        .await
        .into_iter()
        .filter(|r| r.0.starts_with("media_blobs:"))
        .collect();
    assert_eq!(media.len(), 1, "{media:?}");
    let (key, severity, reason, sample) = &media[0];
    assert!(
        key.ends_with("#your_facebook_activity/posts/media/videos/600000000000001.mp4"),
        "{key}"
    );
    assert_eq!(
        (severity.as_str(), reason.as_str()),
        ("warning", "not_found")
    );
    assert!(
        sample.starts_with("media file not in the export: "),
        "{sample}"
    );
    e.db.clone().close().await;
}

/// The same, on the fixture's own two-file table: both reactions files
/// land in one table, and one failing to parse once pruned every row it
/// had contributed.
#[tokio::test(flavor = "multi_thread")]
async fn a_reactions_file_that_will_not_parse_keeps_its_rows() {
    let e = Export::new().await;
    e.sync().await;
    assert_eq!(rows(&e.db, REACTIONS_TABLE).await.len(), 4);

    let broken = "your_facebook_activity/comments_and_reactions/likes_and_reactions_1.json";
    fs::write(e.root.join(broken), "{ not json").unwrap();
    let s = e.sync().await;
    assert_eq!(s.parse_errors, 1);
    assert_eq!(
        rows(&e.db, REACTIONS_TABLE).await.len(),
        4,
        "nothing pruned"
    );
    let keys: Vec<String> = e.problems().await.into_iter().map(|r| r.0).collect();
    assert!(keys.contains(&format!("listing:file {broken}")), "{keys:?}");
    e.db.clone().close().await;
}

/// An export unpacked only in part, missing one chunk of a table it has
/// the rest of, pruned that chunk's rows: the table prunes only when every
/// chunk it was last read from is there and read. A table missing every
/// chunk is left out of the export, and deletes nothing either.
#[tokio::test(flavor = "multi_thread")]
async fn a_table_missing_one_of_its_chunks_deletes_nothing() {
    let e = Export::new().await;
    e.sync().await;
    assert_eq!(rows(&e.db, ALBUMS_TABLE).await.len(), 2);

    fs::remove_file(e.root.join(ALBUM_1)).unwrap();
    e.sync().await;
    assert_eq!(
        rows(&e.db, ALBUMS_TABLE).await.len(),
        2,
        "the missing chunk's album stays"
    );
    let keys: Vec<String> = e.problems().await.into_iter().map(|r| r.0).collect();
    assert!(
        keys.contains(&format!("listing:file {ALBUM_1}")),
        "{keys:?}"
    );

    // Whole again, a chunk is the whole of its rows: what it dropped goes.
    fs::write(
        e.root.join(ALBUM_1),
        r#"{"name": "Holodeck Four", "photos": [], "description": "Sherlock."}"#,
    )
    .unwrap();
    e.sync().await;
    assert_eq!(rows(&e.db, ALBUMS_TABLE).await.len(), 2);
    let keys: Vec<String> = e.problems().await.into_iter().map(|r| r.0).collect();
    assert!(keys.iter().all(|k| !k.starts_with("listing:")), "{keys:?}");
    e.db.clone().close().await;
}

// ── media edges follow their records ───────────────────────────────

const POSTS_FILE: &str =
    "your_facebook_activity/posts/your_posts__check_ins__photos_and_videos_1.json";
const A_POSTED_PHOTO: &str = "your_facebook_activity/posts/media/your_posts/200000000000003.png";

impl Export {
    async fn edge_owners(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT owner_id FROM media_blobs ORDER BY owner_id")
            .fetch_all(self.db.pool())
            .await
            .unwrap()
    }

    /// The id of every record, whichever table it landed in.
    async fn record_ids(&self) -> std::collections::HashSet<String> {
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(self.db.pool())
                .await
                .unwrap();
        let mut ids = std::collections::HashSet::new();
        for table in tables.iter().filter(|t| {
            !t.starts_with("media_blobs") && !t.ends_with("_bookkeeping") && *t != "ingested_files"
        }) {
            // Audited: a table name the store itself lists, quoted.
            let found: Result<Vec<String>, _> =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT id FROM \"{table}\"")))
                    .fetch_all(self.db.pool())
                    .await;
            ids.extend(found.unwrap_or_default());
        }
        ids
    }
}

/// A post a newer export no longer holds was deleted, and the edge to its
/// photo stayed, owned by a record that is gone. Now no edge is left
/// owned by a gone record.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_post_takes_its_media_edges_and_no_edge_is_left_owned_by_a_gone_record() {
    let e = Export::new().await;
    e.sync().await;
    let before = e.edge_owners().await.len();

    // Post 1 carries two photos no other record names.
    let path = e.root.join(POSTS_FILE);
    let mut posts: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let dropped = posts.as_array_mut().unwrap().remove(1);
    assert!(dropped.to_string().contains(A_POSTED_PHOTO));
    fs::write(&path, serde_json::to_vec(&posts).unwrap()).unwrap();
    e.sync().await;

    assert_eq!(rows(&e.db, POSTS_TABLE).await.len(), 3);
    let owners = e.edge_owners().await;
    let records = e.record_ids().await;
    for owner in &owners {
        assert!(
            records.contains(owner),
            "edge owned by a gone record: {owner}"
        );
    }
    assert_eq!(owners.len(), before - 2);
    e.db.clone().close().await;
}

/// A table held back (a chunk missing) deletes no record, so its records
/// keep their media edges too.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_table_keeps_its_records_media_edges() {
    let e = Export::new().await;
    fs::write(
        e.root.join(ALBUM_1),
        format!(
            r#"{{"name": "Holodeck Three", "photos": [{{"uri": "{A_POSTED_PHOTO}"}}], "description": "Dixon Hill."}}"#
        ),
    )
    .unwrap();
    e.sync().await;
    let before = e.edge_owners().await;

    fs::remove_file(e.root.join(ALBUM_1)).unwrap();
    e.sync().await;
    assert_eq!(e.edge_owners().await, before);
    e.db.clone().close().await;
}

/// Every run reads the whole export, so reading an unchanged one again
/// must leave the store as it was: a re-stamped sidecar is a commit, and
/// a bigger store, on every sync. The video the fixture leaves out stays
/// out: a problem recorded again unchanged changes nothing either.
#[tokio::test(flavor = "multi_thread")]
async fn reading_an_unchanged_export_again_commits_nothing() {
    let ex = Export::new().await;
    let mut commits = Vec::new();
    for _ in 0..2 {
        ex.sync().await;
        commits.push(
            datalib_etl::doltlite_raw::commit_run(ex.db.pool(), "test")
                .await
                .unwrap(),
        );
    }
    assert!(commits[0].is_some());
    assert_eq!(
        commits[1], None,
        "reading an unchanged export again changes nothing in the store"
    );
}

/// A file named again after a mid-run flush takes the key that flush
/// stored its bytes under, without a second read. The fixture's media is
/// a few hundred bytes, so the flush threshold was never crossed and this
/// path never ran; a 33 MiB photo on the comment that names it first
/// crosses it before the post that names it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_named_again_after_a_flush_takes_the_key_its_bytes_went_in_under() {
    const SHARED: &str = "your_facebook_activity/posts/media/your_posts/200000000000004.png";
    let e = Export::new().await;
    let big: Vec<u8> = (0..33 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    fs::write(e.root.join(SHARED), &big).unwrap();

    let s = e.sync().await;
    assert_eq!(
        s.media_stored, 4,
        "read once, not again after the flush: {s:?}"
    );
    let edges: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT owner_id, blake3 FROM media_blobs WHERE uri = ? ORDER BY owner_id")
            .bind(SHARED)
            .fetch_all(e.db.pool())
            .await
            .unwrap();
    assert!(edges.len() >= 2, "{edges:?}");
    let key = datalib_etl::blob_cas::blake3_hex(&big);
    for (owner, blake3) in &edges {
        assert_eq!(blake3.as_deref(), Some(key.as_str()), "{owner}");
    }
}
