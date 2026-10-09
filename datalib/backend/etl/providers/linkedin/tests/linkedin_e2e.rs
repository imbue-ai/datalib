//! End-to-end test for the LinkedIn export ingester.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_linkedin::ingest::photos::load_photo_blobs;
use datalib_etl_linkedin::ingest::{self, db_path_for, FetchOptions, RawDb};
use datalib_etl_linkedin_render::connections;
use datalib_etl_linkedin_render::posts;
use datalib_etl_linkedin_render::processor::Source;
use datalib_etl_linkedin_render::render;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;

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
    // blank, on a post that isn't in Shares. Before `created_at` learned to
    // be null it rendered as `1970-01-01 00:00:00 UTC` in the transcript
    // and sorted to the top of the grid as a genuine-looking 1970 row.
    // No checked-in fixture had an undated record, so nothing caught it.
    // The fourth row is how the real export quotes a comment: an embedded
    // quote is `\"`, not `""`, and the message spans lines. Read as
    // RFC-4180 it splits into fragment rows, one with message text in
    // `Date`.
    // The fifth is a second comment on the first row's post. A comment
    // has no id of its own in the export, and keying the table on `Link`
    // alone kept only the last comment per post.
    fs::write(
        root.join("Comments_17529409.csv"),
        "Date,Link,Message\n\
         2026-05-08 09:00:00,https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216,Replying to my own post thread.\n\
         2026-05-08 09:30:00,https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216,And a second reply on the same post.\n\
         2026-04-30 15:32:07,https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226567681,\"Great point, Jean-Luc!\"\n\
         ,https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226567999,The export left this comment's Date blank.\n\
         2026-06-10 18:53:49,https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226568123,\"Back when I led the \\\"tea, Earl Grey\\\" replicator team,\n\nit was hot.\"\n",
    )?;

    // The user's own posts. The first shares a URN with a comment above
    // (they merge into one thread); the second is a standalone post whose
    // commentary quotes the other way the export does it: doubled (`""`).
    fs::write(
        root.join("Shares_17529409.csv"),
        "Date,ShareLink,ShareCommentary,SharedUrl,MediaUrl,Visibility\n\
         2026-05-07 16:41:18,https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216,Excited to share our new treemap viz!,,,MEMBER_NETWORK\n\
         2026-04-01 12:00:00,https://www.linkedin.com/feed/update/urn%3Ali%3Ashare%3A7448081445065035776,\"Check out \"\"this\"\" article\",https://example.com/article,,PUBLIC\n",
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

async fn problems(db: &RawDb) -> Vec<(String, String)> {
    sqlx::query_as("SELECT scope_key, sample FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

async fn rows(db: &RawDb, table: &str) -> Vec<serde_json::Value> {
    db.load_payloads(table).await.unwrap_or_default()
}

const CONTACT_PHOTOS_DDL: &str = "CREATE TABLE IF NOT EXISTS contact_photos (id TEXT PRIMARY KEY, \
     owner_id TEXT NOT NULL, source_url TEXT NOT NULL, blake3 TEXT NULL)";

/// What the photo fetch earlier builds ran left for one connection: the
/// bytes in the CAS and an edge row naming them.
async fn seed_photo(db: &RawDb, owner: &str, bytes: &[u8]) -> Result<()> {
    let cas = db.cas().expect("the download handle has a CAS");
    let blake3 = cas.put(bytes, Some("image/png")).await?;
    sqlx::query(CONTACT_PHOTOS_DDL).execute(db.pool()).await?;
    let image = format!("{owner}/photo.png");
    sqlx::query("INSERT INTO contact_photos VALUES (?, ?, ?, ?)")
        .bind(format!("{owner}#{image}"))
        .bind(owner)
        .bind(&image)
        .bind(blake3)
        .execute(db.pool())
        .await?;
    Ok(())
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
        // the assertions both, because the file takes one writer at a
        // time.
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let summary = ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch")?;
        // Commit, the way the processor does in production: render reads
        // committed state only.
        datalib_etl::store_handle::RawStoreHandle::commit_all(&db, "test: linkedin fetch").await?;

        // 7 CSVs + 1 articles batch = 8 "files".
        assert_eq!(summary.files, 8, "files (7 csv + articles)");
        assert_eq!(summary.parse_errors, 0, "no parse errors");

        // Member-id suffix stripped: table is `comments`, not
        // `comments_17529409`.
        let comments = rows(&db, "comments").await;
        assert_eq!(
            comments.len(),
            5,
            "comments rows: one per record, no fragments, two on one post"
        );
        assert!(
            rows(&db, "comments_17529409").await.is_empty(),
            "no member-id-suffixed table"
        );
        // The `\"`-quoted, multi-line comment is one row, unescaped, and
        // every `Date` is a date or blank — never a fragment of a message.
        let quoted = comments
            .iter()
            .find(|c| c["Date"] == "2026-06-10 18:53:49")
            .expect("the backslash-quoted comment landed as one row");
        assert_eq!(
            quoted["Message"],
            "Back when I led the \"tea, Earl Grey\" replicator team,\n\nit was hot."
        );
        for c in &comments {
            let date = c["Date"].as_str().unwrap_or_default();
            assert!(
                date.is_empty() || date.starts_with("2026-"),
                "a comment's Date is a date, not message text: {c}"
            );
        }
        // Shares ingested under the suffix-stripped `shares` table, and
        // the doubled-quote dialect still unescapes.
        let shares = rows(&db, "shares").await;
        assert_eq!(shares.len(), 2, "shares rows");
        assert!(
            shares
                .iter()
                .any(|s| s["ShareCommentary"] == "Check out \"this\" article"),
            "doubled quotes unescape: {shares:?}"
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
        let account =
            datalib_etl_linkedin_render::account::load_account(&raw_dir, RawRange::cold())?;
        assert_eq!(
            account.label.as_deref(),
            Some("data@enterprise.starfleet.test")
        );
        let source = Source {
            raw_dir: &raw_dir,
            out_dir: &out_dir,
            name: "linkedin",
            account: account.label.as_deref(),
            account_inputs: &account.inputs,
            range: RawRange::cold(),
        };
        let mut docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                docs.push(d);
                Ok(())
            };
            render::render(&source, &Progress::noop(), &mut on_doc).context("render")?;
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
            posts::render_posts(&source, &Progress::noop(), &mut on_doc).context("render_posts")?;
        }
        // Two shares (two URNs) + a comment that merges into the first +
        // a comment on an external post + the undated comment + the
        // backslash-quoted comment = 5 threads.
        assert_eq!(post_docs.len(), 5, "five post threads");

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
        assert!(
            md_a.contains("And a second reply on the same post."),
            "both comments on one post survive ingest and land in its thread: {md_a}"
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
            thread_d.rows.iter().all(|r| r.created_at.is_none()),
            "a comment with a blank Date must leave created_at null on every row it \
             produces, never a fabricated epoch: {:?}",
            thread_d
                .rows
                .iter()
                .map(|r| (r.kind.clone(), r.created_at.clone()))
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

        // Thread E: the backslash-quoted comment renders with its quotes
        // and both lines, under its own URN — nothing minted from a
        // fragment.
        let quoted_urn =
            "https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226568123";
        let thread_e = post_docs
            .iter()
            .find(|d| {
                d.rows
                    .iter()
                    .any(|r| r.source_url.as_deref() == Some(quoted_urn))
            })
            .expect("backslash-quoted comment thread rendered");
        let md_e = fs::read_to_string(&thread_e.md_path)?;
        assert!(
            md_e.contains("&quot;tea, Earl Grey&quot;") || md_e.contains("\"tea, Earl Grey\""),
            "escaped quotes render as quotes: {md_e}"
        );
        assert!(
            md_e.contains("it was hot."),
            "text after the newline kept: {md_e}"
        );
        assert!(
            post_docs.iter().all(|d| d.rows.iter().all(|r| r
                .source_url
                .as_deref()
                .is_none_or(|u| u.starts_with("https://")))),
            "no thread minted from a message fragment"
        );

        // ── connections → contacts ───────────────────────────────
        let mut contact_docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                contact_docs.push(d);
                Ok(())
            };
            connections::render_connections(&source, &Progress::noop(), &mut on_doc)
                .context("render_connections")?;
        }
        assert_eq!(contact_docs.len(), 2, "two connection contacts");
        // Identity + grid row are keyed off the profile URL.
        let picard_uuid = datalib_etl_linkedin_render::ids::connection(
            "linkedin",
            "https://www.linkedin.com/in/jlp",
        )
        .uuid;
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
        assert!(
            row.preview.contains("Captain"),
            "field values in search text"
        );

        // ── a photo an earlier build fetched ──────────────────────
        // Nothing fetches a photo now (linkedin.com shows a profile only
        // to a signed-in visitor), but a store an earlier build filled
        // keeps its photos: a re-read of the export leaves them, and the
        // contact still embeds one.
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        seed_photo(
            &db,
            "https://www.linkedin.com/in/jlp",
            b"PNG bytes for Picard",
        )
        .await?;
        ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("re-read the export")?;
        datalib_etl::store_handle::RawStoreHandle::commit_all(&db, "test: linkedin fetch").await?;
        assert_eq!(problems(&db).await, [], "nothing was asked of linkedin.com");
        let blobs = load_photo_blobs(&db).await?;
        let photo = blobs
            .get("https://www.linkedin.com/in/jlp")
            .expect("Picard's photo kept");
        assert_eq!(photo.bytes, b"PNG bytes for Picard");
        assert_eq!(photo.content_type.as_deref(), Some("image/png"));

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
            connections::render_connections(&source2, &Progress::noop(), &mut on_doc)
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
        db.close().await;

        Ok::<_, anyhow::Error>(())
    })?;

    Ok(())
}

/// A CSV or an article that will not read is a `listing:` row naming the
/// file, the rows an earlier read stored stay, and the next read that
/// works clears the row.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_will_not_read_keeps_its_rows_and_is_a_problem_until_it_reads() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let export = tmp.path().join("export");
    fs::create_dir_all(&export)?;
    build_export(&export)?;
    let articles = export.join("Articles").join("Articles");
    fs::write(
        articles.join("away-team.html"),
        "<html><body><h1>Away team</h1></body></html>",
    )?;
    let raw_dir = tmp.path().join("raw");
    fs::create_dir_all(&raw_dir)?;
    let db = RawDb::open(&db_path_for(&raw_dir)).await?;
    let fetch = || {
        ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
    };
    fetch().await?;
    assert_eq!(rows(&db, "connections").await.len(), 2);
    assert_eq!(rows(&db, "articles").await.len(), 2);

    // Not UTF-8: neither file can be read as text.
    let good_connections = fs::read(export.join("Connections.csv"))?;
    fs::write(export.join("Connections.csv"), b"First Name\n\xff\xfe\n")?;
    fs::write(articles.join("away-team.html"), b"<h1>\xff</h1>")?;
    let s = fetch().await?;
    assert_eq!(s.parse_errors, 2, "{s:?}");
    assert_eq!(
        rows(&db, "connections").await.len(),
        2,
        "the last read's rows stay"
    );
    assert_eq!(
        rows(&db, "articles").await.len(),
        2,
        "the article that would not read keeps its row, the other is still there"
    );
    let keys: Vec<String> = problems(&db).await.into_iter().map(|r| r.0).collect();
    assert_eq!(
        keys,
        [
            "listing:articles Articles/Articles/away-team.html",
            "listing:csv Connections.csv",
        ]
    );

    fs::write(export.join("Connections.csv"), good_connections)?;
    fs::write(articles.join("away-team.html"), "<h1>Away team, again</h1>")?;
    fetch().await?;
    assert_eq!(
        problems(&db).await,
        [],
        "both read, so neither is a problem"
    );
    db.close().await;
    Ok(())
}

/// An export path with nothing at it is not an empty export: the run
/// fails rather than reporting success with nothing read.
#[tokio::test(flavor = "multi_thread")]
async fn an_export_path_that_is_not_there_fails_the_run() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let db = RawDb::open(&db_path_for(tmp.path())).await?;
    let got = ingest::fetch(FetchOptions {
        db: db.clone(),
        input_path: tmp.path().join("not-unpacked-yet"),
        progress: Progress::noop(),
        control: Default::default(),
    })
    .await;
    assert!(got.is_err(), "{got:?}");
    db.close().await;
    Ok(())
}

/// An article directory the walk could not read deletes no article.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_error_deletes_no_article() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let export = tmp.path().join("export");
    fs::create_dir_all(&export)?;
    build_export(&export)?;
    let db = RawDb::open(&db_path_for(tmp.path())).await?;
    let fetch = || {
        ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
    };
    fetch().await?;
    assert_eq!(rows(&db, "articles").await.len(), 1);

    fs::create_dir_all(export.join("Articles/Drafts"))?;
    fs::write(
        export.join("Articles/Drafts/log.html"),
        "<h1>Captain's log</h1>",
    )?;
    fs::remove_file(export.join("Articles/Articles/my-post.html"))?;
    std::os::unix::fs::symlink(
        export.join("nowhere"),
        export.join("Articles/Articles/lost"),
    )?;
    fetch().await?;
    assert_eq!(
        rows(&db, "articles").await.len(),
        2,
        "the new one lands and the one the walk did not see stays"
    );
    let keys: Vec<String> = problems(&db).await.into_iter().map(|r| r.0).collect();
    assert_eq!(keys, ["listing:files"]);
    db.close().await;
    Ok(())
}

/// A CSV that is nothing (0 bytes, or the Notes preamble and no header)
/// read as one listing no rows and emptied its table. A CSV the export
/// left out keeps its table: the export form offers a subset. Only a
/// well-formed CSV with a header and no rows empties it.
#[tokio::test(flavor = "multi_thread")]
async fn only_a_well_formed_csv_empties_its_table() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let export = tmp.path().join("export");
    fs::create_dir_all(&export)?;
    build_export(&export)?;
    let db = RawDb::open(&db_path_for(tmp.path())).await?;
    let fetch = || {
        ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
    };
    fetch().await?;
    assert_eq!(rows(&db, "connections").await.len(), 2);
    assert_eq!(rows(&db, "messages").await.len(), 3);
    assert_eq!(rows(&db, "email_addresses").await.len(), 2);

    fs::write(export.join("Connections.csv"), b"")?;
    fs::write(
        export.join("Email Addresses.csv"),
        "Notes:\n\"Some preamble text about email visibility.\"\n",
    )?;
    fs::remove_file(export.join("messages.csv"))?;
    let s = fetch().await?;
    assert_eq!(
        rows(&db, "connections").await.len(),
        2,
        "a 0-byte Connections.csv is not an empty network"
    );
    assert_eq!(rows(&db, "email_addresses").await.len(), 2);
    assert_eq!(
        rows(&db, "messages").await.len(),
        3,
        "left out, not emptied"
    );
    assert_eq!(s.parse_errors, 2, "{s:?}");
    let keys: Vec<String> = problems(&db).await.into_iter().map(|r| r.0).collect();
    assert_eq!(
        keys,
        [
            "listing:csv Connections.csv",
            "listing:csv Email Addresses.csv"
        ]
    );

    fs::write(
        export.join("Connections.csv"),
        "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n",
    )?;
    fetch().await?;
    assert!(rows(&db, "connections").await.is_empty());
    db.close().await;
    Ok(())
}

/// A connection a newer Connections.csv no longer lists kept its photo
/// edge, so the photo outlived the contact. An export without the CSV, or
/// with one that will not read, says nothing about who is connected, and
/// keeps every edge.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_dropped_from_the_export_loses_its_photo_edge() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let export = tmp.path().join("export");
    fs::create_dir_all(&export)?;
    build_export(&export)?;
    let db = RawDb::open(&db_path_for(tmp.path())).await?;
    let fetch = || {
        ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: export.clone(),
            progress: Progress::noop(),
            control: Default::default(),
        })
    };
    let owners = || async {
        sqlx::query_scalar::<_, String>("SELECT owner_id FROM contact_photos ORDER BY owner_id")
            .fetch_all(db.pool())
            .await
            .unwrap()
    };
    fetch().await?;
    // What an earlier build's photo fetch recorded for two connections
    // that had no photo.
    sqlx::query(CONTACT_PHOTOS_DDL).execute(db.pool()).await?;
    for owner in [
        "https://www.linkedin.com/in/bev",
        "https://www.linkedin.com/in/jlp",
    ] {
        sqlx::query("INSERT INTO contact_photos VALUES (?, ?, ?, NULL)")
            .bind(format!("{owner}#{owner}"))
            .bind(owner)
            .bind(owner)
            .execute(db.pool())
            .await?;
    }

    let both = fs::read(export.join("Connections.csv"))?;
    fs::write(export.join("Connections.csv"), b"")?;
    fetch().await?;
    assert_eq!(
        owners().await.len(),
        2,
        "a CSV that will not read keeps both"
    );

    fs::remove_file(export.join("Connections.csv"))?;
    fetch().await?;
    assert_eq!(owners().await.len(), 2, "a CSV left out keeps both");

    fs::write(export.join("Connections.csv"), both)?;
    assert_eq!(fetch().await?.photos_removed, 0);
    fs::write(
        export.join("Connections.csv"),
        "First Name,Last Name,URL,Email Address,Company,Position,Connected On\n\
         Jean-Luc,Picard,https://www.linkedin.com/in/jlp,,Starfleet,Captain,16 Jun 2026\n",
    )?;
    let s = fetch().await?;
    assert_eq!(
        owners().await,
        ["https://www.linkedin.com/in/jlp"],
        "Crusher is no longer a connection"
    );
    assert_eq!(s.photos_removed, 1);
    db.close().await;
    Ok(())
}
