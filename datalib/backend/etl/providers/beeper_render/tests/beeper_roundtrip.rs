//! The TNG fixture mirrored the way the ingest step mirrors it and
//! rendered the way the render step renders it: a document per room and
//! month, a reaction filed under its target's month, attachments read
//! back out of the blob store, the network filter, the bridges' own
//! message ids kept, and a cache read again committing only what changed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems;
use datalib_etl::stop::StopFlag;
use datalib_etl_beeper::ingest::{self, RawDb};
use datalib_etl_beeper_render::render::parse::{parse, ParsedBeeper};
use datalib_etl_beeper_render::render::{render_all, Period, RenderSummary};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_sqlite_mirror::mirror;

const DATA_IMAGE: &str = "$tng-data-003:ba_TNG.local-signal.localhost";
const SCHEMATIC: &str = "media/localhostlocal-signal/TNGART01";

struct Fixture {
    _dir: tempfile::TempDir,
    beeper: PathBuf,
    raw: PathBuf,
    out: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let beeper = dir.path().join("BeeperTexts");
        let staged = PathBuf::from(std::env::var("BEEPER_TNG_INDEX_DB").expect("set by BUILD"));
        copy_tree(staged.parent().expect("index.db's directory"), &beeper);
        let raw = dir.path().join("ingest");
        std::fs::create_dir_all(&raw).expect("mkdir ingest");
        Self {
            out: dir.path().join("render_markdown"),
            _dir: dir,
            beeper,
            raw,
        }
    }

    /// One download and its seal: the commit, `None` when nothing changed.
    async fn ingest(&self) -> Result<Option<String>> {
        let db = RawDb::open(&dr::db_path_for(&self.raw)).await?;
        let store = &db;
        let fetched = run_problems::collecting(db.pool(), &StopFlag::new(), |found| async move {
            ingest::fetch(&self.beeper, store, true, &Progress::noop(), &found).await
        })
        .await;
        let commit = match fetched {
            Ok(_) => dr::commit_run(db.pool(), "test: beeper download").await,
            Err(e) => Err(e),
        };
        db.close().await;
        commit
    }

    fn parse(&self, networks: Option<&[String]>) -> Result<ParsedBeeper> {
        parse(&self.raw, "tng", Period::Month, networks, RawRange::cold())
    }

    fn render(&self) -> Result<(RenderSummary, Vec<RenderedMarkdown>)> {
        let parsed = self.parse(None)?;
        let mut docs = Vec::new();
        let summary = render_all(
            &parsed,
            &self.out,
            "tng",
            &Progress::noop(),
            &mut |d| {
                docs.push(d);
                Ok(())
            },
            &dr::db_path_for(&self.raw),
            1,
        )?;
        Ok((summary, docs))
    }

    async fn problems(&self) -> Result<Vec<(String, String)>> {
        let db = RawDb::open(&dr::db_path_for(&self.raw)).await?;
        let rows = sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await;
        db.close().await;
        Ok(rows?)
    }

    async fn edit_index(&self, stmts: &[&str]) -> Result<()> {
        let pool = mirror::open_sqlite(&self.beeper.join("index.db"), false).await?;
        for s in stmts {
            // Test: literal edits written by the test itself.
            sqlx::query(sqlx::AssertSqlSafe(*s)).execute(&pool).await?;
        }
        pool.close().await;
        Ok(())
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let dest = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy fixture file");
            // Runfiles are read-only, and the copy keeps their mode.
            let mut perms = std::fs::metadata(&dest).expect("stat").permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(&dest, perms).expect("chmod");
        }
    }
}

fn md_of(docs: &[RenderedMarkdown], path_has: &str, ends: &str) -> String {
    let doc = docs
        .iter()
        .find(|d| {
            let p = d.md_path.to_string_lossy();
            p.contains(path_has) && p.ends_with(ends)
        })
        .unwrap_or_else(|| panic!("no document under {path_has} ending {ends}"));
    std::fs::read_to_string(&doc.md_path).expect("read rendered markdown")
}

#[tokio::test(flavor = "multi_thread")]
async fn each_room_renders_a_document_per_month() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;

    let parsed = fx.parse(None)?;
    // Data, Crusher, the bridge crew, Riker, #bridge; the `$space` thread
    // is no room.
    assert_eq!(parsed.rooms.len(), 5);
    // Riker/2024-03, #bridge/2024-03, Data/2024-03, Data/2024-04,
    // Crusher/2024-04, bridge crew/2364-04.
    assert_eq!(parsed.docs.len(), 6);
    let data = |month: &str| {
        parsed
            .docs
            .iter()
            .find(|d| d.period_key == month && d.room_uuid.contains("tng-data"))
            .unwrap_or_else(|| panic!("no Data document for {month}"))
    };
    // The 🤔 landed in April on the March image, and files with it.
    let reactions = &data("2024-03").reactions_by_target[DATA_IMAGE];
    let emoji: Vec<_> = reactions
        .iter()
        .filter_map(|r| r.reaction_emoji.as_deref())
        .collect();
    assert_eq!(emoji, ["❤️", "🤔"]);
    assert!(data("2024-04").reactions_by_target.is_empty());

    let (summary, docs) = fx.render()?;
    assert_eq!(summary.docs_rendered, 6);
    assert_eq!(summary.blobs_materialized, 2);
    let slack = md_of(&docs, "/slack/", "2024-03.md");
    assert!(
        slack.contains("href=\"https://slack.com/app_redirect?channel=C0BRIDGE&amp;team=T1701D\""),
        "{slack}"
    );
    let march = md_of(&docs, "/signal/", "2024-03.md");
    assert!(march.contains("❤️ [Mr. Data](tel:+15551234001"), "{march}");
    assert!(march.contains("🤔 [Mr. Data](tel:+15551234001"), "{march}");
    assert!(march.contains("![iconian-schematic.png]"), "{march}");
    assert!(
        march.contains("external_id: \"!tng-data:ba_TNG.local-signal.localhost\""),
        "{march}"
    );
    assert!(
        march.contains("project: \"tng-picard-account-uuid\""),
        "{march}"
    );
    let riker = md_of(&docs, "/googlechat/", ".md");
    assert!(
        riker.contains("hidden: m.room.create") || riker.contains("hidden: m.bridge"),
        "{riker}"
    );
    Ok(())
}

/// The download mirrors every network; render shows the ones named.
#[tokio::test(flavor = "multi_thread")]
async fn render_shows_only_the_networks_it_names() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    let parsed = fx.parse(Some(&["signal".to_string()]))?;
    assert_eq!(parsed.rooms.len(), 3);
    assert!(parsed.rooms.values().all(|r| r.network == "signal"));
    assert_eq!(parsed.docs.len(), 4);
    Ok(())
}

/// A bridge's own id for a message is what finds it again in a phone's
/// backup of the same chat, so every bridge's `message` rows are kept.
#[tokio::test(flavor = "multi_thread")]
async fn the_bridges_message_ids_are_kept() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    let db = RawDb::open(&dr::db_path_for(&fx.raw)).await?;
    let ids: Vec<(String, String)> =
        sqlx::query_as("SELECT mxid, id FROM message WHERE bridge_id = 'local-signal'")
            .fetch_all(db.pool())
            .await?;
    db.close().await;
    assert_eq!(ids.len(), 11);
    assert!(ids.contains(&(
        "$tng-data-001:ba_TNG.local-signal.localhost".to_string(),
        "tng-picard-account-uuid|1710493200000".to_string()
    )));
    Ok(())
}

/// Every run reads the whole cache again, so reading an unchanged one
/// must leave the store as it was.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_cache_read_again_commits_nothing() -> Result<()> {
    let fx = Fixture::new();
    assert!(fx.ingest().await?.is_some());
    assert_eq!(fx.ingest().await?, None);
    Ok(())
}

/// The desktop app evicts from its cache; what it let go stays here.
#[tokio::test(flavor = "multi_thread")]
async fn what_the_app_evicts_is_kept() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    std::fs::remove_file(fx.beeper.join(SCHEMATIC))?;
    fx.edit_index(&["DELETE FROM mx_room_messages WHERE eventID LIKE '$tng-bev-%'"])
        .await?;
    assert_eq!(fx.ingest().await?, None, "an eviction changes nothing");

    let (summary, _) = fx.render()?;
    assert_eq!(summary.docs_rendered, 6);
    assert_eq!(summary.blobs_materialized, 2);
    Ok(())
}

/// The app numbers its messages with a local `id`, and a re-index
/// numbers them again; a message is its Matrix event in its room.
#[tokio::test(flavor = "multi_thread")]
async fn a_reindexed_cache_commits_nothing() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    fx.edit_index(&["UPDATE mx_room_messages SET id = id + 1000"])
        .await?;
    assert_eq!(fx.ingest().await?, None);
    Ok(())
}

/// A file the app caches after a run is copied by the next one, and the
/// attachment that named it gets its bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_cached_later_is_copied_by_the_next_run() -> Result<()> {
    let fx = Fixture::new();
    let schematic = fx.beeper.join(SCHEMATIC);
    let kept = fx._dir.path().join("TNGART01");
    std::fs::rename(&schematic, &kept)?;
    fx.ingest().await?;
    assert_eq!(fx.render()?.0.blobs_materialized, 1);
    let march = md_of(&fx.render()?.1, "/signal/", "2024-03.md");
    assert!(march.contains("iconian-schematic.png"), "{march}");

    std::fs::rename(&kept, &schematic)?;
    assert!(fx.ingest().await?.is_some());
    assert_eq!(fx.render()?.0.blobs_materialized, 2);
    Ok(())
}

/// A bridge database that will not read keeps the ids an earlier run
/// mirrored, and says so until it reads again.
#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_database_that_will_not_read_is_a_phase_problem() -> Result<()> {
    let fx = Fixture::new();
    let bridge = fx.beeper.join("local-signal/megabridge.db");
    let good = std::fs::read(&bridge)?;
    std::fs::write(&bridge, b"not a database, Number One")?;
    fx.ingest().await?;
    assert_eq!(
        fx.problems().await?,
        [(
            "phase:megabridge local-signal".to_string(),
            "error".to_string()
        )]
    );

    std::fs::write(&bridge, good)?;
    fx.ingest().await?;
    assert!(fx.problems().await?.is_empty());
    Ok(())
}

/// An attachment with no id, or one whose id names no file this build
/// knows where to find, is a problem on its message's document.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_render_cannot_place_is_a_problem() -> Result<()> {
    let fx = Fixture::new();
    fx.edit_index(&[
        "UPDATE mx_room_messages SET message = json_set(message, '$.attachments', \
         json('[{\"fileName\": \"orders.txt\"}]')) \
         WHERE eventID = '$tng-data-001:ba_TNG.local-signal.localhost'",
        "UPDATE mx_room_messages SET message = json_set(message, '$.attachments', \
         json('[{\"id\": \"https://example.com/warp.png\", \"fileName\": \"warp.png\"}]')) \
         WHERE eventID = '$tng-data-002:ba_TNG.local-signal.localhost'",
    ])
    .await?;
    fx.ingest().await?;
    let (_, docs) = fx.render()?;
    let mut found: Vec<(Option<String>, String)> = docs
        .iter()
        .flat_map(|d| &d.problems)
        .map(|p| (p.field.clone(), p.reason.as_str().to_string()))
        .collect();
    found.sort();
    assert_eq!(
        found,
        [
            (
                Some("attachments.id".to_string()),
                "no_identity".to_string()
            ),
            (
                Some("attachments.id".to_string()),
                "uncovered_type".to_string()
            ),
        ]
    );
    Ok(())
}

/// A person is written by a handle the rest of datalib shares: a Slack
/// user by their Slack id (the owner too, by the user the room was
/// bridged for), anyone else by a number or address Beeper lists.
#[tokio::test(flavor = "multi_thread")]
async fn people_are_written_by_their_handles() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    let (_, docs) = fx.render()?;
    let slack = md_of(&docs, "/slack/", "2024-03.md");
    assert!(
        slack.contains("slack://user?team=T1701D&amp;id=U0WORF")
            || slack.contains("slack://user?team=T1701D&id=U0WORF"),
        "{slack}"
    );
    assert!(slack.contains("id=U0PICARD"), "{slack}");
    let data = md_of(&docs, "/signal/", "2024-03.md");
    assert!(data.contains("(tel:+15551234001"), "{data}");
    let riker = md_of(&docs, "/googlechat/", ".md");
    assert!(riker.contains("(mailto:riker@enterprise.org"), "{riker}");
    Ok(())
}

/// Each person with a handle is a source contact, with their avatar
/// written beside the page where the app had cached it; a chat with no
/// name of its own is called by its other person.
#[tokio::test(flavor = "multi_thread")]
async fn people_are_contacts_with_their_avatars() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    let (_, docs) = fx.render()?;
    let contact = |doc_path: &str, key: &str| {
        docs.iter()
            .filter(|d| d.md_path.to_string_lossy().contains(doc_path))
            .flat_map(|d| d.contacts.iter().map(move |c| (d, c)))
            .find(|(_, c)| c.key == key)
            .unwrap_or_else(|| panic!("no contact {key} under {doc_path}"))
    };
    let (doc, worf) = contact("/slack/", "slack:T1701D/U0WORF");
    assert_eq!(worf.names, ["Worf"]);
    let url = worf.photo_url.as_deref().expect("Worf's avatar is served");
    let rel = url
        .strip_prefix(&format!(
            "/applet/unified_document_index/asset/{}/",
            doc.markdown_uuid
        ))
        .expect("served beside the page");
    assert!(doc.md_path.parent().unwrap().join(rel).is_file(), "{rel}");

    let (_, data) = contact("/signal/", "tel:+15551234001");
    assert!(
        data.photo_url.is_some(),
        "Data's on-device avatar is served"
    );
    let (_, riker) = contact("/googlechat/", "email:riker@enterprise.org");
    assert_eq!(riker.photo_url, None, "Riker has no avatar");

    let data_march = md_of(&docs, "/signal/", "2024-03.md");
    assert!(
        data_march.contains("Beeper:Signal · Mr. Data"),
        "{data_march}"
    );
    Ok(())
}
