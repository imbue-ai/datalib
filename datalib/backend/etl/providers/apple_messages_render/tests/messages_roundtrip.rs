//! The fixture `chat.db`, mirrored the way the ingest step mirrors it and
//! rendered the way the render step renders it. What is Messages-shaped
//! and so covered nowhere else: bodies read out of `attributedBody`,
//! tapbacks folded onto their messages and unfolded by a removal, the
//! join tables keyed by the config's override, and `skip_churn` making
//! an untouched database no commit at all.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::periodize::Period;
use datalib_etl::progress::Progress;
use datalib_etl_apple_messages::processor::mirror_options;
use datalib_etl_apple_messages_config::AppleMessagesConfig;
use datalib_etl_apple_messages_render::render::{chat_uuid, render, RenderOutcome};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_sqlite_mirror::mirror;
use datalib_source_common::LocalPath;

const RIKER: &str = "iMessage;-;+14155550142";
const BRIDGE: &str = "iMessage;+;chat240603120915";

struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    raw: PathBuf,
    out: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("chat.db");
        std::fs::copy(fixture_db(), &db).expect("stage chat.db");
        // A Bazel runfile is read-only and `fs::copy` keeps the mode;
        // the tests play Messages writing to the database.
        let mut perms = std::fs::metadata(&db).expect("stat").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&db, perms).expect("chmod");
        let raw = dir.path().join("ingest");
        std::fs::create_dir_all(&raw).expect("mkdir ingest");
        Self {
            out: dir.path().join("render_markdown"),
            _dir: dir,
            db,
            raw,
        }
    }

    async fn ingest(&self) -> Result<Option<String>> {
        let config = AppleMessagesConfig {
            database: Some(LocalPath {
                path: self.db.clone(),
            }),
            ..Default::default()
        };
        let pool = mirror::open_mirror(&dr::db_path_for(&self.raw)).await?;
        let stats = mirror::run(&pool, &mirror_options(&config)?, &Progress::noop()).await?;
        let commit = dr::commit_run(&pool, &stats.summary()).await?;
        pool.close().await;
        Ok(commit)
    }

    async fn edit(&self, stmts: &[&str]) -> Result<()> {
        let pool = mirror::open_sqlite(&self.db, false).await?;
        for s in stmts {
            // Test: literal edits written by the test itself.
            sqlx::query(sqlx::AssertSqlSafe(*s)).execute(&pool).await?;
        }
        pool.close().await;
        Ok(())
    }

    /// One render pass: the documents it emitted, the chats it named
    /// with nothing (gone), and the commit it consumed.
    async fn render(
        &self,
        cursor: Option<&str>,
    ) -> (Vec<RenderedMarkdown>, Vec<String>, RenderOutcome) {
        let (raw, out, cursor) = (
            self.raw.clone(),
            self.out.clone(),
            cursor.map(str::to_string),
        );
        tokio::task::spawn_blocking(move || {
            let stale = HashSet::new();
            let range = RawRange {
                cursor: cursor.as_deref(),
                pin: None,
                stale: Some(&stale),
            };
            let mut docs = Vec::new();
            let mut on_doc = |md: RenderedMarkdown| {
                docs.push(md);
                Ok(())
            };
            let outcome = render(
                &raw,
                &out,
                "messages",
                Period::Month,
                &Progress::noop(),
                range,
                &mut on_doc,
            )
            .expect("render");
            let rendered: HashSet<&str> = outcome
                .buckets
                .iter()
                .filter(|b| !b.inputs.is_empty())
                .map(|b| b.key.as_str())
                .collect();
            let gone: Vec<String> = outcome
                .buckets
                .iter()
                .filter(|b| b.inputs.is_empty() && !rendered.contains(b.key.as_str()))
                .map(|b| b.key.clone())
                .collect();
            (docs, gone, outcome)
        })
        .await
        .expect("render joined")
    }
}

fn fixture_db() -> PathBuf {
    let p = PathBuf::from(
        std::env::var("APPLE_MESSAGES_TNG_DB").expect("APPLE_MESSAGES_TNG_DB must be set"),
    );
    assert!(p.exists(), "fixture missing at {}", p.display());
    p
}

/// Every page rendered for one chat, concatenated.
fn pages(docs: &[RenderedMarkdown], chat_guid: &str) -> String {
    let uuid = chat_uuid("messages", chat_guid);
    let mut out = String::new();
    for doc in docs
        .iter()
        .filter(|d| d.bucket_key.as_deref() == Some(&uuid))
    {
        out.push_str(&std::fs::read_to_string(&doc.md_path).expect("read rendered page"));
    }
    assert!(!out.is_empty(), "no document for {chat_guid}");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bodies_tapbacks_and_attachments_render() -> Result<()> {
    let fx = Fixture::new();
    let first = fx.ingest().await?.expect("first ingest commits");

    let (docs, gone, outcome) = fx.render(None).await;
    assert!(gone.is_empty());
    assert_eq!(outcome.new_head.as_deref(), Some(first.as_str()));
    // Riker's chat spans two months; the bridge crew's is one.
    assert_eq!(
        docs.len(),
        3,
        "{:?}",
        docs.iter().map(|d| &d.md_path).collect::<Vec<_>>()
    );

    let riker = pages(&docs, RIKER);
    for body in [
        "Captain, the away team is ready.", // from attributedBody
        "Make it so.",                      // from the legacy `text` column
        "IMG_1701.jpeg",                    // the attachment, by name
        "❤️",                               // Picard's tapback on it
    ] {
        assert!(
            riker.contains(body),
            "Riker's page lacks {body:?}:\n{riker}"
        );
    }
    let bridge = pages(&docs, BRIDGE);
    assert!(bridge.contains("Named the group “Bridge crew”"), "{bridge}");
    assert!(
        !bridge.contains("👍"),
        "a removed tapback must not render:\n{bridge}"
    );

    let items: Vec<_> = docs
        .iter()
        .flat_map(|d| d.rows.iter())
        .filter(|r| r.kind == "Messages Message")
        .collect();
    assert_eq!(items.len(), 7, "seven messages, no tapback rows among them");
    let picture = items
        .iter()
        .find(|r| r.upstream_id.as_deref() == Some("A1B2C3D4-0003-4000-8000-000000000003"))
        .expect("the attachment message");
    assert!(
        !picture.text.contains('\u{fffc}'),
        "U+FFFC is not text: {:?}",
        picture.text
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_run_renders_only_what_moved() -> Result<()> {
    let fx = Fixture::new();
    let first = fx.ingest().await?.expect("first ingest commits");
    let (docs, _, _) = fx.render(None).await;
    assert_eq!(docs.len(), 3);

    // Nothing happened, and the daemons' bookkeeping does not count.
    fx.edit(&[
        "UPDATE kvtable SET value = X'02' WHERE key = 'chatVersion'",
        "UPDATE message SET index_state = 1",
    ])
    .await?;
    assert_eq!(fx.ingest().await?, None, "churn alone must not commit");
    let (docs, gone, _) = fx.render(Some(&first)).await;
    assert!(docs.is_empty() && gone.is_empty(), "{docs:?} {gone:?}");

    // Data answers his own question: one chat moves, one month of it.
    fx.edit(&[
        "INSERT INTO message (ROWID, guid, text, handle_id, date, is_from_me) \
         VALUES (11, 'A1B2C3D4-0011-4000-8000-000000000011', 'Recalibrating now.', 2, \
                 797075760000000000, 0)",
        "INSERT INTO chat_message_join (chat_id, message_id, message_date) \
         VALUES (2, 11, 797075760000000000)",
    ])
    .await?;
    let second = fx.ingest().await?.expect("a new message commits");
    let (docs, gone, outcome) = fx.render(Some(&first)).await;
    assert_eq!(outcome.new_head.as_deref(), Some(second.as_str()));
    assert!(gone.is_empty());
    assert_eq!(
        docs.len(),
        1,
        "{:?}",
        docs.iter().map(|d| &d.md_path).collect::<Vec<_>>()
    );
    assert_eq!(
        docs[0].bucket_key.as_deref(),
        Some(chat_uuid("messages", BRIDGE).as_str())
    );
    assert!(pages(&docs, BRIDGE).contains("Recalibrating now."));
    Ok(())
}
