//! End-to-end test for the LinkedIn export ingester.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::Progress;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_linkedin::ingest::photos::load_photo_blobs;
use datalib_etl_linkedin::ingest::schema_raw::connection_uuid;
use datalib_etl_linkedin::ingest::{self, db_path_for, FetchOptions, RawDb};
use datalib_etl_linkedin::synthesize::LinkedinSynth;
use datalib_etl_linkedin_render::connections;
use datalib_etl_linkedin_render::posts;
use datalib_etl_linkedin_render::processor::Source;
use datalib_etl_linkedin_render::render;
use datalib_etl_render::grid_index::RenderedMarkdown;

fn build_export(root: &Path) -> Result<()> {
    // Who the export belongs to. The primary address is deliberately not
    // the first row.
    fs::write(
        root.join("Email Addresses.csv"),
        "Email Address,Confirmed,Primary,Updated On\n\
         data.soong@starfleet.gov,Yes,No,Not Available\n\
         data@enterprise.starfleet.test,Yes,Yes,Not Available\n",
    )?;

    // Connections.csv with the Notes: preamble we strip, and the real
    // column shape (URL is the natural key → uuid identity).
    fs::write(
        root.join("Connections.csv"),
        "Notes:\n\"Some preamble text about email visibility.\"\n\n\
         First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
         Jean-Luc,Picard,https://www.linkedin.com/in/jlp,,Starfleet,Captain,16 Jun 2026\n\
         Beverly,Crusher,https://www.linkedin.com/in/bev,,Starfleet,CMO,17 Jun 2026\n",
    )?;

    // Member-id-suffixed filename → canonical table `comments`. Two
    // comments: one on the user's own ugcPost (merges into its Shares
    // thread by URN), one on someone else's post (its body isn't in the
    // export → a comment-only thread).
    // The third row is the undated case, and it is real: the live
    // manual-e2e corpus contains a comment whose `Date` the export left
    // blank, on a post that isn't in Shares. Before `when_ts` learned to
    // be null it rendered as `1970-01-01 00:00:00 UTC` in the transcript
    // and sorted to the top of the grid as a genuine-looking 1970 row.
    // No checked-in fixture had an undated record, so nothing caught it.
    fs::write(
        root.join("Comments_17529409.csv"),
        "Date,Link,Message\n\
         2026-05-08 09:00:00,https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216,Replying to my own post thread.\n\
         2026-04-30 15:32:07,https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226567681,\"Great point, Jean-Luc!\"\n\
         ,https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226567999,The export left this comment's Date blank.\n",
    )?;

    // The user's own posts. The first shares a URN with a comment above
    // (they merge into one thread); the second is a standalone post.
    fs::write(
        root.join("Shares_17529409.csv"),
        "Date,ShareLink,ShareCommentary,SharedUrl,MediaUrl,Visibility\n\
         2026-05-07 16:41:18,https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216,Excited to share our new treemap viz!,,,MEMBER_NETWORK\n\
         2026-04-01 12:00:00,https://www.linkedin.com/feed/update/urn%3Ali%3Ashare%3A7448081445065035776,Check this out,https://example.com/article,,PUBLIC\n",
    )?;

    // Primary messages feed: two conversations.
    fs::write(
        root.join("messages.csv"),
        "CONVERSATION ID,CONVERSATION TITLE,FROM,SENDER PROFILE URL,TO,DATE,CONTENT\n\
         conv-a,,Picard,https://www.linkedin.com/in/jlp,Riker,2026-01-01 10:00:00 UTC,Report.\n\
         conv-a,,Riker,https://www.linkedin.com/in/wtr,Picard,2026-01-01 10:01:00 UTC,On my way.\n\
         conv-b,,Picard,https://www.linkedin.com/in/jlp,Data,2026-02-01 08:00:00 UTC,Status?\n",
    )?;

    // A second message-shaped feed (AI coach), same schema.
    fs::write(
        root.join("guide_messages.csv"),
        "CONVERSATION ID,CONVERSATION TITLE,FROM,SENDER PROFILE URL,TO,DATE,CONTENT\n\
         guide-1,Coaching,Guide,,You,2026-03-01 09:00:00 UTC,Welcome aboard.\n",
    )?;

    // A CSV that isn't in KNOWN_FILES — should still ingest (with a WARN).
    fs::write(root.join("Some Future Feed.csv"), "Col A,Col B\nx,y\n")?;

    // An article: Articles/Articles/<file>.html (note the nested dir,
    // mirroring the real export layout).
    let articles = root.join("Articles").join("Articles");
    fs::create_dir_all(&articles)?;
    fs::write(
        articles.join("my-post.html"),
        "<html><body><h1>Treemaps</h1></body></html>",
    )?;
    Ok(())
}

async fn rows(db: &RawDb, table: &str) -> Vec<serde_json::Value> {
    db.load_payloads(datalib_etl::pin::Reads::Own, table)
        .await
        .unwrap_or_default()
}

#[test]
fn ingests_complete_export_and_renders_all_message_feeds() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let export = tmp.path().join("export");
    fs::create_dir_all(&export)?;
    build_export(&export)?;

    // The raw store lives alongside, mirroring `<data_root>/<name>/raw`.
    let raw_dir = tmp.path().join("raw");
    fs::create_dir_all(&raw_dir)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async {
        // ── download ──────────────────────────────────────────────
        // The test owns each store: one connection for the download and
        // the assertions both, because two is what breaks a doltlite
        // file.
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let summary = ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            fetch_photos: false,
            photo_max_consecutive_failures: 50,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch")?;
        // Commit, the way the processor does in production: render reads
        // committed state only.
        datalib_etl::doltlite_raw::commit_run(db.pool(), "test: linkedin fetch").await?;

        // 7 CSVs + 1 articles batch = 8 "files".
        assert_eq!(summary.files, 8, "files (7 csv + articles)");
        assert_eq!(summary.parse_errors, 0, "no parse errors");

        // Member-id suffix stripped: table is `comments`, not
        // `comments_17529409`.
        assert_eq!(rows(&db, "comments").await.len(), 3, "comments rows");
        assert!(
            rows(&db, "comments_17529409").await.is_empty(),
            "no member-id-suffixed table"
        );
        // Shares ingested under the suffix-stripped `shares` table.
        assert_eq!(rows(&db, "shares").await.len(), 2, "shares rows");

        // Notes: preamble stripped, both connection rows landed.
        assert_eq!(rows(&db, "connections").await.len(), 2, "connections rows");

        // The unknown CSV still ingested under its slug.
        assert_eq!(
            rows(&db, "some_future_feed").await.len(),
            1,
            "unknown feed ingested"
        );

        // Articles HTML ingested, one row, payload carries the html.
        let articles = rows(&db, "articles").await;
        assert_eq!(articles.len(), 1, "one article row");
        let html = articles[0]["html"].as_str().unwrap_or_default();
        assert!(html.contains("Treemaps"), "article html captured");

        // ── render ───────────────────────────────────────────────
        // `render` opens the store itself, so hand the file over first:
        // one doltlite file takes one connection at a time.
        db.close().await;
        // render() uses block_in_place internally, so it must run on a
        // multi-threaded runtime worker (this `block_on`), not a
        // spawn_blocking thread.
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&out_dir)?;
        // The export names its owner in `Email Addresses.csv`; every row
        // of every feed carries that on `account`.
        let account = datalib_etl_linkedin_render::account::load_account(&raw_dir)?;
        assert_eq!(account.as_deref(), Some("data@enterprise.starfleet.test"));
        let source = Source {
            raw_dir: &raw_dir,
            out_dir: &out_dir,
            name: "linkedin",
            account: account.as_deref(),
        };
        let mut docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                docs.push(d);
                Ok(())
            };
            render::render(
                &source,
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
                &mut std::collections::HashSet::new(),
            )
            .context("render")?;
        }

        // Two `messages` conversations + one `guide_messages` = 3 chats,
        // each rendering at least one markdown doc.
        assert!(
            docs.len() >= 3,
            "rendered at least 3 docs, got {}",
            docs.len()
        );
        assert!(
            docs.iter()
                .flat_map(|d| d.rows.iter())
                .all(|r| r.account.as_deref() == Some("data@enterprise.starfleet.test")),
            "every message row names the export's owner as its account"
        );

        // ── shares + comments → one thread per post ──────────────
        let mut post_docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                post_docs.push(d);
                Ok(())
            };
            posts::render_posts(
                &source,
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
                &mut std::collections::HashSet::new(),
            )
            .context("render_posts")?;
        }
        // Two shares (two URNs) + a comment that merges into the first +
        // a comment on an external post + the undated comment = 4 threads.
        assert_eq!(post_docs.len(), 4, "four post threads");

        // Thread A: the user's ugcPost, with their follow-up comment
        // merged into the same thread by shared URN.
        let ugc = "https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216";
        let thread_a = post_docs
            .iter()
            .find(|d| d.rows.iter().any(|r| r.source_url.as_deref() == Some(ugc)))
            .expect("ugcPost thread rendered");
        let md_a = fs::read_to_string(&thread_a.md_path)?;
        assert!(
            md_a.contains("Excited to share our new treemap viz!"),
            "post body in thread"
        );
        assert!(
            md_a.contains("Replying to my own post thread."),
            "comment merged into the post's thread: {md_a}"
        );
        // Message-level grid rows carry the linkout back to the post.
        assert!(
            thread_a
                .rows
                .iter()
                .any(|r| r.kind == "LinkedIn Post Message" && r.source_url.as_deref() == Some(ugc)),
            "message row carries the post linkout"
        );
        // The chat-level row (whole post) carries it too, and the page
        // title renders the `↗` source link.
        assert!(
            thread_a
                .rows
                .iter()
                .any(|r| r.kind == "LinkedIn Post" && r.source_url.as_deref() == Some(ugc)),
            "chat-level row carries the post linkout"
        );
        assert!(
            md_a.contains("class=\"source-link\"") && md_a.contains(ugc),
            "page title carries the `↗` linkout: {md_a}"
        );

        // Thread C: a comment on someone else's post — the original body
        // isn't in the export, so we note that and still link out.
        let act = "https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226567681";
        let thread_c = post_docs
            .iter()
            .find(|d| d.rows.iter().any(|r| r.source_url.as_deref() == Some(act)))
            .expect("external-post comment thread rendered");
        let md_c = fs::read_to_string(&thread_c.md_path)?;
        assert!(md_c.contains("Great point, Jean-Luc!"), "comment body");
        assert!(
            md_c.to_lowercase()
                .contains("not included in the linkedin export"),
            "missing-original note: {md_c}"
        );

        // Thread D: the undated comment. Every row it produces — the
        // chat-level row, the placeholder for the missing original, and
        // the comment itself — must carry NO timestamp.
        let undated_urn =
            "https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226567999";
        let thread_d = post_docs
            .iter()
            .find(|d| {
                d.rows
                    .iter()
                    .any(|r| r.source_url.as_deref() == Some(undated_urn))
            })
            .expect("undated comment thread rendered");
        assert!(
            thread_d.rows.iter().all(|r| r.when_ts.is_none()),
            "a comment with a blank Date must leave when_ts null on every row it \
             produces, never a fabricated epoch: {:?}",
            thread_d
                .rows
                .iter()
                .map(|r| (r.kind.clone(), r.when_ts.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(thread_d.rows.len(), 3, "chat row + placeholder + comment");
        // ...and the markdown says so in words rather than printing a
        // date that isn't real.
        let md_d = fs::read_to_string(&thread_d.md_path)?;
        assert!(
            md_d.contains("(no timestamp)"),
            "undated items say so in the transcript: {md_d}"
        );
        assert!(
            !md_d.contains("1970-01-01"),
            "no fabricated epoch anywhere in the rendered page: {md_d}"
        );

        // ── connections → contacts ───────────────────────────────
        let mut contact_docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                contact_docs.push(d);
                Ok(())
            };
            connections::render_connections(
                &source,
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
                &mut std::collections::HashSet::new(),
            )
            .context("render_connections")?;
        }
        assert_eq!(contact_docs.len(), 2, "two connection contacts");
        // Identity + grid row are keyed off the profile URL.
        let picard_uuid = connection_uuid("https://www.linkedin.com/in/jlp");
        let picard = contact_docs
            .iter()
            .find(|d| d.markdown_uuid == picard_uuid)
            .expect("Picard rendered under his URL-derived uuid");
        let row = &picard.rows[0];
        assert_eq!(row.kind, "Contact");
        assert_eq!(row.source_label, "LinkedIn");
        assert_eq!(
            row.source_url.as_deref(),
            Some("https://www.linkedin.com/in/jlp")
        );
        assert!(row.text.contains("Captain"), "field values in search text");

        // ── photo fetch (hermetic via the synthesizer + playback) ──
        // Synthesize profile-page + image fixtures, point the curl
        // chokepoint at them, and re-download with fetch_photos on. This
        // is the only test in this binary, so mutating the playback env
        // var here is race-free.
        let playback = tmp.path().join("playback");
        fs::create_dir_all(&playback)?;
        Synthesizer::synthesize(&LinkedinSynth::new(export.clone()), &playback)?;
        std::env::set_var(PLAYBACK_ENV, &playback);
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            fetch_photos: true,
            photo_max_consecutive_failures: 50,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch with photos")?;
        // Commit, the way the processor does in production: render reads
        // committed state only.
        datalib_etl::doltlite_raw::commit_run(db.pool(), "test: linkedin fetch").await?;
        std::env::remove_var(PLAYBACK_ENV);

        // The photo landed in CAS, keyed by the connection's uuid.
        let blobs = load_photo_blobs(&db, datalib_etl::pin::Reads::Own).await?;
        let (bytes, content_type) = blobs
            .get(&picard_uuid)
            .expect("Picard's photo fetched into CAS");
        assert!(!bytes.is_empty(), "photo bytes stored");
        assert_eq!(content_type.as_deref(), Some("image/png"));

        // Re-render: the contact markdown now embeds the photo blob.
        let out2 = tmp.path().join("out2");
        fs::create_dir_all(&out2)?;
        let source2 = Source {
            out_dir: &out2,
            ..source
        };
        let mut with_photo: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                with_photo.push(d);
                Ok(())
            };
            connections::render_connections(
                &source2,
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
                &mut std::collections::HashSet::new(),
            )
            .context("render_connections with photo")?;
        }
        let picard_doc = with_photo
            .iter()
            .find(|d| d.markdown_uuid == picard_uuid)
            .expect("picard re-rendered");
        let md = fs::read_to_string(&picard_doc.md_path)?;
        assert!(
            md.contains(&format!("blobs/{picard_uuid}")),
            "markdown embeds the photo blob: {md}"
        );

        // ── transient misses are retryable ─────────────────────────
        // A fresh store, then a photo pass pointed at an EMPTY playback
        // dir: every fetch is a playback miss (transient), so NOTHING is
        // recorded. A second pass with real fixtures retries and fetches.
        let raw2 = tmp.path().join("raw2");
        fs::create_dir_all(&raw2)?;
        let db2 = RawDb::open(&db_path_for(&raw2)).await?;
        ingest::fetch(FetchOptions {
            db: db2.clone(),
            input_path: export.clone(),
            fetch_photos: false,
            photo_max_consecutive_failures: 50,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await?;
        // Commit, the way the processor does in production: render reads
        // committed state only.
        datalib_etl::doltlite_raw::commit_run(db2.pool(), "test: linkedin fetch").await?;

        let empty_pb = tmp.path().join("empty_pb");
        fs::create_dir_all(&empty_pb)?;
        std::env::set_var(PLAYBACK_ENV, &empty_pb);
        let s1 = ingest::photos::fetch_connection_photos(
            &db2,
            db2.cas().expect("the download handle has a CAS"),
            &Progress::noop(),
            50,
        )
        .await?;
        std::env::remove_var(PLAYBACK_ENV);
        assert_eq!(s1.fetched, 0, "no photos on a playback miss");
        assert!(s1.transient >= 1, "playback miss is transient, got {s1:?}");
        assert!(
            load_photo_blobs(&db2, datalib_etl::pin::Reads::Own)
                .await?
                .is_empty(),
            "transient miss records nothing"
        );

        // Retry with the real fixtures — now it succeeds.
        std::env::set_var(PLAYBACK_ENV, &playback);
        let s2 = ingest::photos::fetch_connection_photos(
            &db2,
            db2.cas().expect("the download handle has a CAS"),
            &Progress::noop(),
            50,
        )
        .await?;
        std::env::remove_var(PLAYBACK_ENV);
        assert!(
            s2.fetched >= 1,
            "transient miss retried and fetched, got {s2:?}"
        );
        assert!(
            !load_photo_blobs(&db2, datalib_etl::pin::Reads::Own)
                .await?
                .is_empty(),
            "photo recorded after retry"
        );

        // ── give-up after N consecutive failures ───────────────────
        // Fresh store, empty playback (every fetch transient), limit 1:
        // it should stop after the very first failure rather than walk
        // all connections.
        let raw3 = tmp.path().join("raw3");
        fs::create_dir_all(&raw3)?;
        let db3 = RawDb::open(&db_path_for(&raw3)).await?;
        ingest::fetch(FetchOptions {
            db: db3.clone(),
            input_path: export.clone(),
            fetch_photos: false,
            photo_max_consecutive_failures: 50,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await?;
        // Commit, the way the processor does in production: render reads
        // committed state only.
        datalib_etl::doltlite_raw::commit_run(db3.pool(), "test: linkedin fetch").await?;
        std::env::set_var(PLAYBACK_ENV, &empty_pb);
        let g = ingest::photos::fetch_connection_photos(
            &db3,
            db3.cas().expect("the download handle has a CAS"),
            &Progress::noop(),
            1, // give up after a single consecutive failure
        )
        .await?;
        std::env::remove_var(PLAYBACK_ENV);
        assert!(g.gave_up, "should give up at the limit, got {g:?}");
        assert_eq!(g.attempted, 1, "stopped after the first failure, got {g:?}");

        Ok::<_, anyhow::Error>(())
    })?;

    Ok(())
}
