//! End-to-end test for the Beeper provider against the
//! ST:TNG-themed SQL fixture.
//!
//! Materializes a Beeper Texts-shaped data directory from the
//! `fixtures/beeper_tng/` SQL files into a tempdir, runs
//! `extract::fetch` + `translate::render_all` against it, and
//! asserts the on-disk doltlite output + rendered markdown.
//!
//! Requires the system `sqlite3` CLI (macOS ships with it; on
//! Linux distros it's in the `sqlite3` package). Override via
//! `BEEPER_SQLITE3=/path/to/sqlite3` if needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_beeper::extract::{self, FetchOptions, FetchSummary};
use frankweiler_etl_beeper::translate::{self, Period};

/// Path to the fixture directory on disk. Bazel stages the fixture
/// at a runfiles-relative path and exposes it via
/// `BEEPER_FIXTURE_DIR`; cargo runs out of `CARGO_MANIFEST_DIR`.
fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("BEEPER_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/beeper_tng")
}

/// Build a Beeper Texts-shaped data dir at `target` from the
/// checked-in SQL + media. Mirrors `build_fixture.sh` so the test
/// stays self-contained.
fn materialize_fixture(target: &Path) -> Result<()> {
    let fixtures = fixture_dir();
    std::fs::create_dir_all(target.join("local-signal"))?;
    std::fs::create_dir_all(target.join("media/local.beeper.com"))?;
    std::fs::create_dir_all(target.join("media/localhostlocal-signal"))?;

    let sqlite3 = std::env::var("BEEPER_SQLITE3").unwrap_or_else(|_| "sqlite3".to_string());

    for (sql, db) in [
        (fixtures.join("index_db.sql"), target.join("index.db")),
        (
            fixtures.join("local_signal_megabridge.sql"),
            target.join("local-signal/megabridge.db"),
        ),
    ] {
        if db.exists() {
            std::fs::remove_file(&db).ok();
        }
        let sql_bytes = std::fs::read(&sql).with_context(|| format!("read {}", sql.display()))?;
        let mut cmd = Command::new(&sqlite3);
        cmd.arg(&db).stdin(std::process::Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("spawn {sqlite3}"))?;
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("stdin piped")
                .write_all(&sql_bytes)
                .context("write SQL to sqlite3 stdin")?;
        }
        let status = child.wait().context("wait sqlite3")?;
        anyhow::ensure!(status.success(), "sqlite3 failed loading {}", sql.display());
    }

    std::fs::copy(
        fixtures.join("media/local.beeper.com/TNGRPT01"),
        target.join("media/local.beeper.com/TNGRPT01"),
    )?;
    std::fs::copy(
        fixtures.join("media/localhostlocal-signal/TNGART01"),
        target.join("media/localhostlocal-signal/TNGART01"),
    )?;
    Ok(())
}

/// Invoke `extract::fetch` for the test's chosen sources. Tests
/// pick whichever wrapper matches their runtime context.
async fn run_extract(
    db_path: PathBuf,
    beeper_data_dir: PathBuf,
    sources: Vec<&str>,
) -> Result<FetchSummary> {
    extract::fetch(FetchOptions {
        db_path,
        sources: sources.into_iter().map(String::from).collect(),
        beeper_data_dir: Some(beeper_data_dir),
        media: true,
        progress: Progress::noop(),
    })
    .await
}

/// Sync wrapper: spin up a private runtime. Use from `#[test]`
/// (no outer runtime). Panics if invoked from inside one.
fn run_extract_sync(
    db_path: PathBuf,
    beeper_data_dir: PathBuf,
    sources: Vec<&str>,
) -> Result<FetchSummary> {
    let rt = tokio::runtime::Runtime::new().context("create test rt")?;
    rt.block_on(run_extract(db_path, beeper_data_dir, sources))
}

#[test]
fn tng_fixture_extract_summary() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let beeper_dir = tmp.path().join("BeeperTexts");
    materialize_fixture(&beeper_dir)?;
    let out_db = tmp.path().join("out.doltlite_db");

    let summary = run_extract_sync(out_db.clone(), beeper_dir, vec!["signal", "googlechat"])?;

    // 3 rooms (data, crusher, riker). $space is filtered.
    assert_eq!(summary.rooms, 3, "rooms");
    // 4 distinct users (picard self, data, crusher, riker).
    assert_eq!(summary.users, 4, "users");
    // 9 messages + 2 reactions + 2 HIDDEN + 1 file = 14.
    assert_eq!(summary.events, 14, "events");
    // 2 blobs (PNG + PDF). Both fetched successfully.
    assert_eq!(summary.blobs, 2, "blobs");
    assert_eq!(summary.blob_errors, 0, "blob_errors");
    // Megabridge enrichment paired 7 Signal messages + 2 reactions.
    assert_eq!(summary.events_enriched, 9, "events_enriched");
    // The deliberately-orphaned megabridge row.
    assert_eq!(summary.events_orphaned, 1, "events_orphaned");
    Ok(())
}

#[test]
fn tng_fixture_extract_landed_data() -> Result<()> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::Row;
    use std::str::FromStr;

    let tmp = tempfile::tempdir()?;
    let beeper_dir = tmp.path().join("BeeperTexts");
    materialize_fixture(&beeper_dir)?;
    let out_db = tmp.path().join("out.doltlite_db");
    run_extract_sync(out_db.clone(), beeper_dir, vec!["signal", "googlechat"])?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", out_db.display()))?
            .read_only(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;

        // The Signal rooms have their native conversation UUIDs
        // captured from `thread.extra.bridge.channel.id`.
        let row = sqlx::query(
            "SELECT external_room_id, external_workspace_id FROM rooms
             WHERE network = 'signal' AND native_room_id LIKE '%tng-data%'",
        )
        .fetch_one(&pool)
        .await?;
        let ext_room: String = row.try_get("external_room_id")?;
        let ext_ws: String = row.try_get("external_workspace_id")?;
        assert_eq!(ext_room, "tng-data-conv-uuid-0001");
        assert_eq!(ext_ws, "tng-picard-account-uuid");

        // Megabridge enriched Signal events with bridge-native ids.
        let row = sqlx::query(
            "SELECT external_event_id FROM events
             WHERE source = 'beeper_index'
               AND network = 'signal'
               AND native_event_id = '$tng-data-001:ba_TNG.local-signal.localhost'",
        )
        .fetch_one(&pool)
        .await?;
        let ext: String = row.try_get("external_event_id")?;
        assert_eq!(ext, "tng-picard-account-uuid|1710493200000");

        // GoogleChat events do NOT get external_event_id (no
        // megabridge for cloud bridges).
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events
             WHERE network = 'googlechat' AND external_event_id IS NOT NULL",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(count, 0, "googlechat should have no external_event_id");

        // The reaction event got the composite external id
        // (<target>#<emoji>) populated by megabridge enrichment.
        let row = sqlx::query(
            "SELECT external_event_id FROM events
             WHERE native_event_id = '$tng-data-react-001:ba_TNG.local-signal.localhost'",
        )
        .fetch_one(&pool)
        .await?;
        let ext: String = row.try_get("external_event_id")?;
        assert!(
            ext.contains("tng-picard-account-uuid|1710493800000") && ext.contains("❤️"),
            "reaction external_event_id should embed target id + emoji, got {ext:?}"
        );

        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

// translate::parse uses `block_in_place` to bridge into sqlx,
// which requires a multi-thread runtime context. Plain `#[test]`
// doesn't provide one; use `#[tokio::test(flavor = "multi_thread")]`
// for any test that calls into translate.
#[tokio::test(flavor = "multi_thread")]
async fn tng_fixture_translate_per_month_with_cross_month_reaction() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let beeper_dir = tmp.path().join("BeeperTexts");
    materialize_fixture(&beeper_dir)?;
    let out_db = tmp.path().join("out.doltlite_db");
    run_extract(out_db.clone(), beeper_dir, vec!["signal", "googlechat"]).await?;

    let parsed = translate::parse::parse(&out_db, Period::Month)?;

    // 4 docs total: GC-Riker/2024-03, Signal-Data/2024-03,
    // Signal-Data/2024-04, Signal-Crusher/2024-04.
    assert_eq!(parsed.docs.len(), 4, "doc bucket count");

    // The Signal-Data 2024-03 bucket has BOTH the ❤️ (Mar) AND
    // the 🤔 (Apr) reactions attached, because both target the
    // image that lives in March. That's the cross-month reaction
    // attachment the renderer was designed for.
    let data_march = parsed
        .docs
        .iter()
        .find(|d| {
            d.period_key == "2024-03"
                && parsed
                    .rooms
                    .get(&d.room_uuid)
                    .map(|r| r.network == "signal")
                    .unwrap_or(false)
        })
        .expect("missing Signal-Data March bucket");
    let image_native = "$tng-data-003:ba_TNG.local-signal.localhost";
    let rs = data_march
        .reactions_by_target
        .get(image_native)
        .expect("expected reactions on the March image");
    assert_eq!(rs.len(), 2, "expected ❤️ + 🤔 attached to March image");
    let emojis: std::collections::HashSet<&str> = rs
        .iter()
        .filter_map(|r| r.reaction_emoji.as_deref())
        .collect();
    assert!(emojis.contains("❤️"));
    assert!(emojis.contains("🤔"));

    // The Signal-Data April bucket has NO reactions attached to
    // any of its own messages (the only reactions in the data
    // target the March image).
    let data_april = parsed
        .docs
        .iter()
        .find(|d| {
            d.period_key == "2024-04"
                && parsed
                    .rooms
                    .get(&d.room_uuid)
                    .map(|r| r.is_dm)
                    .unwrap_or(false)
                && d.messages
                    .iter()
                    .any(|m| m.native_event_id.contains("tng-data"))
        })
        .expect("missing Signal-Data April bucket");
    assert!(
        data_april.reactions_by_target.is_empty(),
        "Apr bucket should have NO co-located reactions"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn tng_fixture_render_to_markdown_files() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let beeper_dir = tmp.path().join("BeeperTexts");
    materialize_fixture(&beeper_dir)?;
    let out_db = tmp.path().join("out.doltlite_db");
    run_extract(out_db.clone(), beeper_dir, vec!["signal", "googlechat"]).await?;

    let parsed = translate::parse::parse(&out_db, Period::Month)?;
    let rendered_root = tmp.path().join("rendered");
    let mut rendered: Vec<RenderedMarkdown> = Vec::new();
    let mut on_doc = |d: RenderedMarkdown| -> anyhow::Result<()> {
        rendered.push(d);
        Ok(())
    };
    let summary = translate::render::render_all(
        &parsed,
        &rendered_root,
        "tng",
        &Progress::noop(),
        &HashMap::new(),
        &mut on_doc,
        &out_db,
    )?;
    assert_eq!(summary.docs_total, 4);
    assert_eq!(summary.docs_rendered, 4);
    assert_eq!(summary.docs_skipped, 0);
    assert_eq!(summary.blobs_materialized, 2);
    assert_eq!(rendered.len(), 4);

    // March markdown contains BOTH reactions inline under the
    // image. April markdown contains neither.
    let march = std::fs::read_to_string(
        rendered
            .iter()
            .find(|d| d.md_path.to_string_lossy().contains("/signal/"))
            .and_then(|d| {
                if d.md_path.to_string_lossy().ends_with("2024-03.md") {
                    Some(d.md_path.clone())
                } else {
                    None
                }
            })
            .expect("signal 2024-03 doc"),
    )?;
    assert!(
        march.contains("❤️ Mr. Data"),
        "march md should show ❤️ Mr. Data reaction"
    );
    assert!(
        march.contains("🤔 Mr. Data"),
        "march md should show cross-month 🤔 Mr. Data reaction"
    );
    assert!(
        march.contains("![iconian-schematic.png]"),
        "march md should inline the image attachment"
    );

    // HIDDEN events become one-liners in their period file.
    let mar_gc = rendered
        .iter()
        .find(|d| d.md_path.to_string_lossy().contains("/googlechat/"))
        .map(|d| d.md_path.clone())
        .expect("googlechat doc");
    let gc = std::fs::read_to_string(&mar_gc)?;
    assert!(
        gc.contains("hidden: m.room.create") || gc.contains("hidden: m.bridge"),
        "googlechat md should surface HIDDEN events as one-liners"
    );

    // Frontmatter carries external IDs.
    assert!(march.contains("external_room_id: tng-data-conv-uuid-0001"));
    assert!(march.contains("external_workspace_id: tng-picard-account-uuid"));

    // Blob file actually got materialized into the page dir.
    let blob_dir = rendered
        .iter()
        .find(|d| {
            d.md_path.to_string_lossy().ends_with("signal/")
                || d.md_path.to_string_lossy().contains("/signal/")
        })
        .map(|d| d.md_path.parent().unwrap().join("blobs"))
        .expect("signal blob dir");
    let entries: Vec<_> = std::fs::read_dir(&blob_dir)
        .with_context(|| format!("read_dir {}", blob_dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries
            .iter()
            .any(|e| e.file_name().to_string_lossy().ends_with(".png")),
        "expected a .png blob in {}",
        blob_dir.display()
    );

    Ok(())
}
