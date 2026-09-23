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
    db.load_payloads(datalib_etl::pin::Reads::Own, table)
        .await
        .unwrap_or_default()
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
        // assertions both, because two is what breaks a doltlite file.
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

        // 5 posts (4 timeline + 1 on another page) + 1 album + comments
        // over two months + reactions over two months + 3 friends.
        assert_eq!(docs.len(), 5 + 1 + 2 + 2 + 3, "documents rendered");
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
            .find(|d| d.rows.iter().any(|r| r.text.contains("Tea, Earl Grey, hot.")))
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
            .find(|d| d.rows.iter().any(|r| r.text.contains("Two views of the bridge")))
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

        // Comments: one document per month, the title folded in, the
        // mojibake'd 🍸 restored.
        let comments: Vec<_> = docs
            .iter()
            .filter(|d| d.rows.iter().any(|r| r.kind == "Facebook Comment"))
            .collect();
        assert_eq!(comments.len(), 2, "comments span two months");
        let april = comments
            .iter()
            .find(|d| d.md_path.file_name().unwrap() == "2369-04.md")
            .expect("April comments");
        let md = fs::read_to_string(&april.md_path)?;
        assert!(md.contains("Guinan's synthehol never disappoints. 🍸"), "{md}");
        assert!(md.contains("*Jean-Luc Picard commented on his own album.*"), "{md}");

        // Reactions: two events from four rows, each with its linkout.
        let reactions: Vec<_> = docs
            .iter()
            .filter(|d| d.rows.iter().any(|r| r.kind == "Facebook Reaction"))
            .collect();
        assert_eq!(reactions.len(), 2, "reactions span two months");
        let reaction_rows: Vec<_> = reactions
            .iter()
            .flat_map(|d| d.rows.iter())
            .filter(|r| r.kind == "Facebook Reaction")
            .collect();
        assert_eq!(reaction_rows.len(), 2, "one row per reaction, not per file");
        assert!(reaction_rows.iter().any(|r| r.source_url.as_deref()
            == Some("https://www.facebook.com/will.riker/posts/pfbid0RIKER")));

        // Friends: three contacts in one group.
        let friends: Vec<_> = docs
            .iter()
            .filter(|d| d.rows.iter().any(|r| r.kind == "Contact"))
            .collect();
        assert_eq!(friends.len(), 3);
        assert!(friends.iter().all(|d| d
            .rows
            .iter()
            .all(|r| r.conversation_name.as_deref() == Some("Friends"))));

        // Every document declares the rows it read, so a change to any of
        // them renders it again; every one includes the profile row.
        for d in &docs {
            assert!(d.bucket_key.is_some(), "{} has a bucket", d.md_path.display());
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}
