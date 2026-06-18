//! End-to-end test for the LinkedIn export ingester.
//!
//! Builds a small synthetic export in a tempdir that exercises every
//! interesting path — a `Notes:`-preamble file, a member-id-suffixed
//! filename, an `Articles/` HTML file, two message-shaped feeds, and a
//! CSV that isn't in the manifest — then runs `extract::fetch` and
//! `render::render` against it and asserts the landed raw tables and
//! rendered chats.
//!
//! Self-contained: the fixture is written by the test, so there are no
//! checked-in fixture files and nothing to stage via Bazel `data`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_linkedin::connections;
use frankweiler_etl_linkedin::extract::photos::load_photo_blobs;
use frankweiler_etl_linkedin::extract::schema_raw::connection_uuid;
use frankweiler_etl_linkedin::extract::{self, db_path_for, FetchOptions, RawDb};
use frankweiler_etl_linkedin::render;
use frankweiler_etl_linkedin::synthesize::LinkedinSynth;

/// Write the synthetic export tree under `root`.
fn build_export(root: &Path) -> Result<()> {
    // Connections.csv with the Notes: preamble we strip, and the real
    // column shape (URL is the natural key → uuid identity).
    fs::write(
        root.join("Connections.csv"),
        "Notes:\n\"Some preamble text about email visibility.\"\n\n\
         First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
         Jean-Luc,Picard,https://www.linkedin.com/in/jlp,,Starfleet,Captain,16 Jun 2026\n\
         Beverly,Crusher,https://www.linkedin.com/in/bev,,Starfleet,CMO,17 Jun 2026\n",
    )?;

    // Member-id-suffixed filename → canonical table `comments`.
    fs::write(
        root.join("Comments_99887766.csv"),
        "Date,Link,Message\n\
         2026-01-02 03:04:05,https://example.com/p/1,Make it so.\n",
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
    db.load_payloads(table).await.unwrap_or_default()
}

#[test]
fn ingests_complete_export_and_renders_all_message_feeds() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let export = tmp.path().join("export");
    fs::create_dir_all(&export)?;
    build_export(&export)?;

    // The raw store lives alongside, mirroring `<data_root>/raw/<name>`.
    let raw_dir = tmp.path().join("raw");
    fs::create_dir_all(&raw_dir)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async {
        // ── extract ──────────────────────────────────────────────
        let summary = extract::fetch(FetchOptions {
            db_path: raw_dir.clone(),
            db: None,
            input_path: export.clone(),
            fetch_photos: false,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch")?;

        // 5 CSVs + 1 articles batch = 6 "files".
        assert_eq!(summary.files, 6, "files (5 csv + articles)");
        assert_eq!(summary.parse_errors, 0, "no parse errors");

        let db = RawDb::open(&db_path_for(&raw_dir)).await?;

        // Member-id suffix stripped: table is `comments`, not
        // `comments_99887766`.
        assert_eq!(rows(&db, "comments").await.len(), 1, "comments rows");
        assert!(
            rows(&db, "comments_99887766").await.is_empty(),
            "no member-id-suffixed table"
        );

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
        // render() uses block_in_place internally, so it must run on a
        // multi-threaded runtime worker (this `block_on`), not a
        // spawn_blocking thread.
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&out_dir)?;
        let mut docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                docs.push(d);
                Ok(())
            };
            render::render(
                &raw_dir,
                &out_dir,
                "linkedin",
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
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

        // ── connections → contacts ───────────────────────────────
        let mut contact_docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                contact_docs.push(d);
                Ok(())
            };
            connections::render_connections(
                &raw_dir,
                &out_dir,
                "linkedin",
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
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
        // chokepoint at them, and re-extract with fetch_photos on. This
        // is the only test in this binary, so mutating the playback env
        // var here is race-free.
        let playback = tmp.path().join("playback");
        fs::create_dir_all(&playback)?;
        Synthesizer::synthesize(&LinkedinSynth::new(export.clone()), &playback)?;
        std::env::set_var(PLAYBACK_ENV, &playback);
        extract::fetch(FetchOptions {
            db_path: raw_dir.clone(),
            db: None,
            input_path: export.clone(),
            fetch_photos: true,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch with photos")?;
        std::env::remove_var(PLAYBACK_ENV);

        // The photo landed in CAS, keyed by the connection's uuid.
        let blobs = load_photo_blobs(&db, &db_path_for(&raw_dir)).await?;
        let (bytes, content_type) = blobs
            .get(&picard_uuid)
            .expect("Picard's photo fetched into CAS");
        assert!(!bytes.is_empty(), "photo bytes stored");
        assert_eq!(content_type.as_deref(), Some("image/png"));

        // Re-render: the contact markdown now embeds the photo blob.
        let out2 = tmp.path().join("out2");
        fs::create_dir_all(&out2)?;
        let mut with_photo: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                with_photo.push(d);
                Ok(())
            };
            connections::render_connections(
                &raw_dir,
                &out2,
                "linkedin",
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
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

        Ok::<_, anyhow::Error>(())
    })?;

    Ok(())
}
