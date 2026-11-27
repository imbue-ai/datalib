//! The TNG fixture mirrored the way the ingest step mirrors it and
//! rendered the way the render step renders it: a document per room, a
//! reply an edge to the message it answers, attachments read
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
use datalib_etl_beeper_render::render::{ids, render_all, RenderSummary};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_render::section::join;
use datalib_etl_sqlite_mirror::mirror;
use datalib_schema::edges::EdgeKind;
use datalib_sql::{test_sql, test_sql_id, Sql};

const DATA_IMAGE: &str = "$tng-data-003:ba_TNG.local-signal.localhost";
const SCHEMATIC: &str = "media/localhostlocal-signal/TNGART01";

struct Fixture {
    _dir: tempfile::TempDir,
    beeper: PathBuf,
    raw: PathBuf,
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
        parse(&self.raw, "tng", networks, RawRange::cold())
    }

    fn render(&self) -> Result<(RenderSummary, Vec<RenderedMarkdown>)> {
        let parsed = self.parse(None)?;
        let mut docs = Vec::new();
        let summary = render_all(
            &parsed,
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
        let rows = test_sql!("SELECT scope_key, severity FROM problems ORDER BY scope_key")
            .query_as()
            .fetch_all(db.pool())
            .await;
        db.close().await;
        Ok(rows?)
    }

    async fn edit_index(&self, stmts: &[&str]) -> Result<()> {
        let pool = mirror::open_sqlite(&self.beeper.join("index.db"), false).await?;
        for s in stmts {
            // Test: literal edits written by the test itself.
            Sql::dynamic(test_sql_id!(), *s)
                .query()
                .execute(&pool)
                .await?;
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

/// Whether a document came through the bridge to `network` (its
/// `source_label`, after `Beeper:`).
fn bridged(doc: &RenderedMarkdown, network: &str) -> bool {
    let label = format!("Beeper:{network}");
    doc.rows.iter().any(|r| r.source_label == label)
}

/// The markdown of the document bridged to `network` that contains
/// `needle`; any of its documents for `""`.
fn md_of(docs: &[RenderedMarkdown], network: &str, needle: &str) -> String {
    docs.iter()
        .filter(|d| bridged(d, network))
        .map(|d| join(&d.sections))
        .find(|md| md.contains(needle))
        .unwrap_or_else(|| panic!("no {network} document holding {needle:?}"))
}

const DATA_ROOM: &str = "!tng-data:ba_TNG.local-signal.localhost";

#[tokio::test(flavor = "multi_thread")]
async fn each_room_renders_one_document() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;

    let parsed = fx.parse(None)?;
    // Data, Crusher, the bridge crew, Riker, #bridge; the `$space` thread
    // is no room.
    assert_eq!(parsed.rooms.len(), 5);
    assert_eq!(parsed.docs.len(), 5);
    let data = parsed
        .docs
        .iter()
        .find(|d| d.room_uuid == DATA_ROOM)
        .expect("Data's room");
    let reactions = &data.reactions_by_target[DATA_IMAGE];
    let emoji: Vec<_> = reactions
        .iter()
        .filter_map(|r| r.reaction_emoji.as_deref())
        .collect();
    assert_eq!(emoji, ["❤️", "🤔"]);

    let (summary, docs) = fx.render()?;
    assert_eq!(summary.docs_rendered, 5);
    // Data's room runs from March into April, and is one document under
    // the room's own uuid.
    let room_uuid = ids::room("tng", DATA_ROOM).uuid;
    let data_docs: Vec<_> = docs
        .iter()
        .filter(|d| d.rows.iter().any(|r| r.uuid == room_uuid))
        .collect();
    assert_eq!(data_docs.len(), 1);
    assert_eq!(data_docs[0].markdown_uuid, room_uuid);
    let months: std::collections::BTreeSet<_> = data_docs[0]
        .rows
        .iter()
        .filter_map(|r| r.created_at.as_deref())
        .map(|t| &t[..7])
        .collect();
    assert_eq!(
        months.into_iter().collect::<Vec<_>>(),
        ["2024-03", "2024-04"]
    );
    assert_eq!(summary.blobs_materialized, 2);
    let slack = md_of(&docs, "Slack", "");
    assert!(
        slack.contains("href=\"https://slack.com/app_redirect?channel=C0BRIDGE&amp;team=T1701D\""),
        "{slack}"
    );
    let march = md_of(&docs, "Signal", DATA_ROOM);
    for emoji in ["❤️", "🤔"] {
        let pill = format!("<span class=\"msg-reaction\">{emoji} 1<span class=\"msg-reactors\"> ");
        let reactors = march
            .split_once(&pill)
            .and_then(|(_, rest)| rest.split_once("</span></span>"))
            .map(|(reactors, _)| reactors);
        assert!(
            reactors.is_some_and(|r| r.contains("\">[Mr. Data](tel:+15551234001")),
            "{emoji}: {march}"
        );
    }
    assert!(march.contains("![iconian-schematic.png]"), "{march}");
    assert!(
        march.contains("external_id: \"!tng-data:ba_TNG.local-signal.localhost\""),
        "{march}"
    );
    assert!(
        march.contains("project: \"tng-picard-account-uuid\""),
        "{march}"
    );
    let riker = md_of(&docs, "Google Chat", "");
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
    assert_eq!(parsed.docs.len(), 3);
    Ok(())
}

/// A chat on an account no bridge here reads was left out with nothing to say
/// so. Beeper's own `hungryserv` is on record at `info`; a bridge this render
/// does not know is a warning. A login in the account id goes no further.
#[tokio::test(flavor = "multi_thread")]
async fn chats_on_an_unlisted_account_are_on_record() -> Result<()> {
    use datalib_etl_beeper_render::render::parse::unlisted_chat_rows;
    let fx = Fixture::new();
    fx.ingest().await?;
    assert!(fx.parse(None)?.unlisted_chats.is_empty());

    fx.edit_index(&[
        "UPDATE threads SET accountID = 'hungryserv' \
         WHERE threadID = '!tng-data:ba_TNG.local-signal.localhost'",
        "UPDATE threads SET accountID = 'matrix_ba_picard' \
         WHERE threadID NOT LIKE '%local-signal%'",
    ])
    .await?;
    fx.ingest().await?;
    let parsed = fx.parse(None)?;
    assert_eq!(parsed.unlisted_chats.get("hungryserv"), Some(&1));
    assert!(parsed.unlisted_chats.get("matrix").is_some_and(|n| *n > 0));
    assert!(!parsed.unlisted_chats.contains_key("matrix_ba_picard"));

    let rows = unlisted_chat_rows(&parsed.unlisted_chats, "tng", 1);
    let got: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.scope_key.as_str(), r.severity.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("unlisted_bridges:hungryserv", "info"),
            ("unlisted_bridges:matrix", "warning"),
        ]
    );
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
        test_sql!("SELECT mxid, id FROM message WHERE bridge_id = 'local-signal'")
            .query_as()
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
    assert_eq!(summary.docs_rendered, 5);
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
    let march = md_of(&fx.render()?.1, "Signal", DATA_ROOM);
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
    let slack = md_of(&docs, "Slack", "");
    assert!(
        slack.contains("slack://user?team=T1701D&amp;id=U0WORF")
            || slack.contains("slack://user?team=T1701D&id=U0WORF"),
        "{slack}"
    );
    assert!(slack.contains("id=U0PICARD"), "{slack}");
    let data = md_of(&docs, "Signal", DATA_ROOM);
    assert!(data.contains("(tel:+15551234001"), "{data}");
    let riker = md_of(&docs, "Google Chat", "");
    assert!(riker.contains("(mailto:riker@enterprise.org"), "{riker}");
    Ok(())
}

/// Each person with a handle is a source contact, with their avatar
/// linked where the blob store holds it when the app had cached it; a chat with no name of its own is
/// called by its other person.
#[tokio::test(flavor = "multi_thread")]
async fn people_are_contacts_with_their_avatars() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    let (_, docs) = fx.render()?;
    let contact = |network: &str, key: &str| {
        docs.iter()
            .filter(|d| bridged(d, network))
            .flat_map(|d| d.contacts.iter())
            .find(|c| c.key == key)
            .unwrap_or_else(|| panic!("no contact {key} bridged to {network}"))
    };
    let worf = contact("Slack", "slack:T1701D/U0WORF");
    assert_eq!(worf.names, ["Worf"]);
    let url = worf.photo_url.as_deref().expect("Worf's avatar is served");
    let hash = url
        .strip_prefix("/data/")
        .and_then(|rest| rest.rsplit_once("/blob/"))
        .map(|(_, hash)| hash)
        .expect("linked at its blob-store address");
    assert_eq!(hash.len(), 64, "{url}");

    let data = contact("Signal", "tel:+15551234001");
    assert!(
        data.photo_url.is_some(),
        "Data's on-device avatar is served"
    );
    let riker = contact("Google Chat", "email:riker@enterprise.org");
    assert_eq!(riker.photo_url, None, "Riker has no avatar");

    let data_md = md_of(&docs, "Signal", DATA_ROOM);
    assert!(data_md.contains(">Mr. Data</"), "{data_md}");
    Ok(())
}

/// A message that answers another is a reply edge from it to the one it
/// answers; before, only its "in reply to" line said so.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_is_an_edge_to_the_message_it_answers() -> Result<()> {
    let fx = Fixture::new();
    fx.ingest().await?;
    let (_, docs) = fx.render()?;
    let slack = docs.iter().find(|d| bridged(d, "Slack")).expect("#bridge");
    let uuid_of = |event: &str| {
        slack
            .rows
            .iter()
            .find(|r| r.upstream_id.as_deref() == Some(event))
            .map(|r| r.uuid.clone())
            .unwrap_or_else(|| panic!("no row for {event}"))
    };
    let replies: Vec<_> = slack
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::ReplyTo.as_i64())
        .map(|e| (e.src_anchor_uuid.clone(), e.dst_anchor_uuid.clone()))
        .collect();
    assert_eq!(
        replies,
        [(
            Some(uuid_of("$tng-slack-002:beeper.com")),
            Some(uuid_of("$tng-slack-001:beeper.com"))
        )]
    );
    assert!(md_of(&docs, "Slack", "").contains("↪ in reply to"));
    Ok(())
}
