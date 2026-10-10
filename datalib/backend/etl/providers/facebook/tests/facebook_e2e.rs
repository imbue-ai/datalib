//! End-to-end test for the Facebook export provider: ingest the
//! checked-in TNG export, commit, render every feed, and read back what
//! landed — the tables, the CAS'd media, the documents.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_facebook::ingest::schema_raw::{
    ALBUMS_TABLE, COMMENTS_TABLE, FRIENDS_TABLE, MESSENGER_MESSAGES_TABLE, MESSENGER_THREADS_TABLE,
    POSTS_TABLE, PROFILE_TABLE, REACTIONS_TABLE,
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

        // 18 JSON files, seven of them Messenger conversations;
        // `no-data.txt` and the HTML are not files to us.
        assert_eq!(summary.files, 18, "json files ingested");
        assert_eq!(summary.parse_errors, 0);
        // Six PNGs (four posted, a Messenger photo and a sticker), each
        // stored once however many records point at it; the video the
        // export left out is counted, not fatal.
        assert_eq!(summary.media_stored, 6, "distinct media files stored");
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
        // comments + a year of reactions + 3 friends + a year each of 7
        // Messenger conversations.
        assert_eq!(docs.len(), 5 + 1 + 1 + 1 + 3 + 7, "documents rendered");
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

        // Reactions: two events from four rows, each with its linkout. A
        // Messenger conversation has reactions too; this is the feed.
        let reactions: Vec<_> = docs
            .iter()
            .filter(|d| d.rows.iter().any(|r| r.kind == "Facebook Reaction"))
            .filter(|d| d.rows.iter().all(|r| r.kind != "Facebook Conversation"))
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

        // Messenger: a conversation is a chat, its people chips by name.
        let conversation = |name: &str| {
            let d = docs
                .iter()
                .find(|d| {
                    d.rows.iter().any(|r| {
                        r.kind == "Facebook Conversation"
                            && r.conversation_name.as_deref() == Some(name)
                    })
                })
                .unwrap_or_else(|| panic!("conversation {name} rendered"));
            (d, fs::read_to_string(&d.md_path).unwrap())
        };
        let (riker, md) = conversation("William Riker");
        assert!(
            md.contains("(datalib:handle/facebook/name/William%20Riker"),
            "Riker is a chip: {md}"
        );
        assert!(md.contains("Tea, Earl Grey, hot? ☕"), "{md}");
        assert!(md.contains("🔗 [The Picard Maneuver"), "the share: {md}");
        assert!(md.contains("Unsent"), "the unsent message: {md}");
        assert_eq!(md.matches("blobs/").count(), 2, "the photo and the sticker: {md}");
        assert!(md.find("Engage.") < md.find("Now."), "the older first: {md}");
        assert!(riker.rows.iter().any(|r| r.kind == "Facebook Reaction"));
        assert_eq!(riker.md_path.file_name().unwrap(), "2369.md");

        // What render could not use is a problem row on the document,
        // at the message's own section, and says what it was by its
        // shape alone, never by what it said.
        let call = riker
            .problems
            .iter()
            .find(|p| p.field.as_deref() == Some("call_duration"))
            .unwrap_or_else(|| panic!("the unread field is a problem: {:?}", riker.problems));
        assert_eq!(call.scope_key, riker.markdown_uuid);
        assert_eq!(call.sample, "int");
        assert_eq!(call.path.as_deref(), Some("/message/call_duration"));
        let section = call.item_uuid.as_deref().expect("a message's own section");
        let call_row = riker
            .rows
            .iter()
            .find(|r| r.uuid == section)
            .expect("the section is a row of the document");
        assert!(call_row.preview.contains("started a video chat"));
        assert!(md.contains(&format!("data-section-uuid=\"{section}\"")));

        let (_, md) = conversation("Facebook user · 1000000002");
        assert!(
            md.contains("(datalib:handle/facebook/deleted/1000000002"),
            "the one deleted account is its conversation's: {md}"
        );
        let (_, md) = conversation("Facebook user · 1000000003");
        assert!(
            md.contains("(datalib:handle/facebook/deleted/1000000003"),
            "an empty name is a deleted account too: {md}"
        );
        let (ten_forward, md) = conversation("Ten Forward");
        assert_eq!(
            md.matches("Facebook user (one of 3 deleted accounts here)").count(),
            4,
            "three messages and a reaction from accounts that cannot be told apart: {md}"
        );
        assert!(!md.contains("facebook/deleted/"), "{md}");
        let noted: Vec<(&str, &str)> = ten_forward
            .problems
            .iter()
            .filter_map(|p| Some((p.field.as_deref()?, p.sample.as_str())))
            .collect();
        assert!(
            noted.contains(&(
                "participants",
                "3 deleted accounts among 7 participants; their messages cannot be told apart"
            )),
            "{noted:?}"
        );
        assert_eq!(
            noted.iter().filter(|(f, _)| *f == "sender_name").count(),
            1,
            "Wesley, who left, is noted once: {noted:?}"
        );
        assert!(
            md.contains("(datalib:handle/facebook/name/Wesley%20Crusher"),
            "one who left is still named: {md}"
        );
        assert!(ten_forward
            .rows
            .iter()
            .all(|r| r.project.as_deref() == Some("Messenger")));
        let (requests, _) = conversation("Lwaxana Troi");
        assert!(requests
            .rows
            .iter()
            .all(|r| r.project.as_deref() == Some("Messenger · requests")));

        // Every field render leaves unread is a problem, by its shape
        // alone, so a run over a real export says what it left out.
        let mut found: Vec<String> = docs
            .iter()
            .flat_map(|d| &d.problems)
            .map(|p| {
                format!(
                    "{} {} {} = {}",
                    p.severity.as_str(),
                    p.field.as_deref().unwrap_or("-"),
                    p.path.as_deref().unwrap_or("-"),
                    p.sample
                )
            })
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "info participants - = 3 deleted accounts among 7 participants; \
                 their messages cannot be told apart",
                "info sender_name - = a sender the conversation's participants do not \
                 list (left the conversation?)",
                "warning call_duration /message/call_duration = int",
                "warning label_values: /label_values/7 = object{timestamp_value}",
                "warning label_values:Attachments /label_values/5 = object{dict,title}",
                "warning label_values:Detected dialect /label_values/4 = object{label,value}",
                "warning label_values:Last modified /label_values/1 = \
                 object{label,timestamp_value}",
                "warning label_values:Target /label_values/3 = object{label}",
            ]
        );
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
        s.media_stored, 6,
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

// ── Messenger ──────────────────────────────────────────────────────

const RIKER_PHOTO: &str =
    "your_facebook_activity/messages/inbox/williamriker_1000000001/photos/300000000000001.png";

async fn message(db: &RawDb, id: &str) -> serde_json::Value {
    let payload: String =
        sqlx::query_scalar("SELECT json(payload) FROM messenger_messages WHERE id = ?")
            .bind(id)
            .fetch_one(db.pool())
            .await
            .unwrap_or_else(|e| panic!("no message {id}: {e}"));
    serde_json::from_str(&payload).unwrap()
}

/// Every conversation, from every folder Facebook files one under, lands
/// in the two Messenger tables — not a table per conversation — keyed by
/// the conversation's id and each message's time.
#[tokio::test(flavor = "multi_thread")]
async fn messenger_conversations_land_as_threads_and_messages() {
    let e = Export::new().await;
    e.sync().await;

    let threads = rows(&e.db, MESSENGER_THREADS_TABLE).await;
    let mut folders: Vec<(String, String)> = threads
        .iter()
        .map(|t| {
            (
                t["thread_id"].as_str().unwrap().to_string(),
                t["folder"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    folders.sort();
    assert_eq!(
        folders,
        [
            ("1000000001", "inbox"),
            ("1000000002", "inbox"),
            ("1000000003", "inbox"),
            ("1000000004", "inbox"),
            ("1000000005", "filtered_threads"),
            ("1000000006", "message_requests"),
            ("1000000007", "e2ee_cutover"),
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    assert!(
        threads
            .iter()
            .all(|t| t["thread"].get("messages").is_none()),
        "a thread row holds the conversation, not its messages"
    );
    assert_eq!(rows(&e.db, MESSENGER_MESSAGES_TABLE).await.len(), 25);

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE '%messages_inbox%'",
    )
    .fetch_all(e.db.pool())
    .await
    .unwrap();
    assert!(tables.is_empty(), "no table per conversation: {tables:?}");

    // Two in one millisecond: the older is 0, as the file lists it last.
    let engage = message(&e.db, "1000000001:12600000360000:0").await;
    assert_eq!(engage["message"]["content"], "Engage.");
    assert_eq!(
        message(&e.db, "1000000001:12600000360000:1").await["message"]["content"],
        "Now."
    );
    // Mojibake undone, in the text and in a reaction.
    let tea = message(&e.db, "1000000001:12600000300000:0").await;
    assert_eq!(tea["message"]["content"], "Tea, Earl Grey, hot? ☕");
    assert_eq!(tea["message"]["reactions"][0]["reaction"], "❤");
    // An unsent message is kept, flag and all.
    assert_eq!(
        message(&e.db, "1000000001:12600000420000:0").await["message"]["is_unsent"],
        true
    );
    // A deleted account with no name at all, in a directory that is its id.
    assert_eq!(
        message(&e.db, "1000000003:12600002000000:0").await["message"]["sender_name"],
        ""
    );

    // The photo's edge is owned by the message that sent it.
    let owners: Vec<String> = sqlx::query_scalar("SELECT owner_id FROM media_blobs WHERE uri = ?")
        .bind(RIKER_PHOTO)
        .fetch_all(e.db.pool())
        .await
        .unwrap();
    assert_eq!(owners, ["1000000001:12600000120000:0"]);
    e.db.clone().close().await;
}

/// A store written before the Messenger tables held each conversation
/// file as one row of a table named for its path. Rung 1 of the ladder
/// splits it into the two tables, moves the photo's edge (and the bytes
/// it names) to the message that sent it, and drops the old table; the
/// next sync then finds nothing new to read.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_from_before_the_messenger_tables_is_migrated_on_open() {
    use datalib_etl::doltlite_raw as dr;

    const OLD_TABLE: &str = "your_facebook_activity_messages_inbox_williamriker_1000000001_message";
    const OLD_ID: &str = "0b4e2f1a-0000-5000-8000-000000000001";
    const KEPT_BLAKE3: &str = "the-bytes-an-earlier-run-stored";
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("export");
    copy_tree(&fixture(), &root);
    let path = db_path_for(&tmp.path().join("raw"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file: serde_json::Value =
        serde_json::from_slice(
            &fs::read(root.join(
                "your_facebook_activity/messages/inbox/williamriker_1000000001/message_1.json",
            ))
            .unwrap(),
        )
        .unwrap();
    {
        let pool = dr::open(&path, &[]).await.unwrap();
        for ddl in [
            dr::wire_payload_table_ddl(OLD_TABLE, &[]),
            "CREATE TABLE media_blobs (id TEXT PRIMARY KEY, owner_id TEXT NOT NULL, \
             uri TEXT NOT NULL, blake3 TEXT NULL)"
                .to_string(),
            dr::bookkeeping_ddl_for("media_blobs"),
            datalib_etl_files::file_checkpoint::INGESTED_FILES_DDL.to_string(),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(ddl))
                .execute(&pool)
                .await
                .unwrap();
        }
        let old_edge = format!("{OLD_ID}#{RIKER_PHOTO}");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {OLD_TABLE} (id, payload) VALUES (?, jsonb(?))"
        )))
        .bind(OLD_ID)
        .bind(file.to_string())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO media_blobs (id, owner_id, uri, blake3) VALUES (?, ?, ?, ?)")
            .bind(&old_edge)
            .bind(OLD_ID)
            .bind(RIKER_PHOTO)
            .bind(KEPT_BLAKE3)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO media_blobs_bookkeeping (id, fetched_at_utc, attempt_count) \
             VALUES (?, '2369-01-01T00:00:00Z', 1)",
        )
        .bind(&old_edge)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ingested_files (scope, rel_path, blake3, size_bytes, last_finished_at_utc) \
             VALUES (?, 'x', 'y', 1, '2369-01-01T00:00:00Z')",
        )
        .bind(format!("facebook/{OLD_TABLE}"))
        .execute(&pool)
        .await
        .unwrap();
        dr::commit_run(&pool, "an earlier build").await.unwrap();
        pool.close().await;
    }

    let db = RawDb::open(&path).await.expect("the rung carries it");
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(OLD_TABLE)
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert!(names.is_empty(), "the old table is gone");
    assert_eq!(rows(&db, MESSENGER_THREADS_TABLE).await.len(), 1);
    assert_eq!(rows(&db, MESSENGER_MESSAGES_TABLE).await.len(), 10);
    let sender = "1000000001:12600000120000:0";
    let edges: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT id, owner_id, blake3 FROM media_blobs")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(
        edges,
        [(
            format!("{sender}#{RIKER_PHOTO}"),
            sender.to_string(),
            Some(KEPT_BLAKE3.to_string())
        )]
    );
    let stamped: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM media_blobs_bookkeeping WHERE fetched_at_utc IS NOT NULL",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(stamped, [format!("{sender}#{RIKER_PHOTO}")]);
    let scopes: Vec<String> = sqlx::query_scalar("SELECT scope FROM ingested_files")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert!(scopes.is_empty(), "{scopes:?}");

    // The next sync keys the same rows the rung wrote, and takes the
    // photo's bytes from the edge it moved rather than reading it again.
    let summary = ingest::fetch(FetchOptions {
        db: db.clone(),
        input_path: root.clone(),
        progress: Progress::noop(),
        control: Default::default(),
    })
    .await
    .unwrap();
    assert_eq!(rows(&db, MESSENGER_MESSAGES_TABLE).await.len(), 25);
    let photo_edges: Vec<Option<String>> =
        sqlx::query_scalar("SELECT blake3 FROM media_blobs WHERE uri = ?")
            .bind(RIKER_PHOTO)
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(photo_edges, [Some(KEPT_BLAKE3.to_string())]);
    assert_eq!(
        summary.media_stored, 5,
        "every PNG but the one already held"
    );
    db.close().await;
}
