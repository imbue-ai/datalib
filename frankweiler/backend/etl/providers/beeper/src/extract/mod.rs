//! Beeper extract entry point.
//!
//! Reads Beeper Texts' on-disk SQLite stores under
//! `~/Library/Application Support/BeeperTexts/` and re-shapes them
//! into our `rooms`/`users`/`events`/`blobs` doltlite tables. No
//! network, no auth — everything's local data the desktop app
//! already syncs and decrypts for us.
//!
//! The user picks which chat networks to ingest via `FetchOptions.sources`
//! (e.g. `["signal", "googlechat"]`). Each canonical network is
//! mapped to the right `accountID` patterns by [`index_db`]; rows
//! that don't match are skipped at the source.
//!
//! See `EXTRACT.md` for the on-disk layout and the rationale for
//! reading from index.db rather than the network.

pub mod db;
pub mod index_db;
pub mod megabridge;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{info, instrument};

pub use db::{db_path_for, RawDb};

/// Default location of Beeper Texts' data directory on macOS. The
/// reader walks `<beeper_data_dir>/index.db` and
/// `<beeper_data_dir>/media/`.
pub fn default_beeper_data_dir() -> PathBuf {
    // ~/Library/Application Support/BeeperTexts is the macOS path.
    // On other platforms there's no Beeper Texts install at all, so
    // we'd surface a clear error at open time rather than guessing.
    if let Some(home) = dirs_home() {
        home.join("Library/Application Support/BeeperTexts")
    } else {
        PathBuf::from("BeeperTexts")
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Path to the doltlite database we write into. Legacy directory
    /// paths get rewritten to `<dir>.doltlite_db` by [`db_path_for`].
    pub db_path: PathBuf,
    /// Canonical network names to ingest. Empty = none (refuse;
    /// caller probably forgot to configure). Order doesn't matter.
    pub sources: Vec<String>,
    /// Override for the Beeper Texts data directory. Defaults to
    /// [`default_beeper_data_dir`].
    pub beeper_data_dir: Option<PathBuf>,
    /// Download cached media bytes into the `blobs` table. When
    /// false, blob rows are pre-seeded with metadata + source URL
    /// only.
    pub media: bool,
    pub progress: frankweiler_etl::progress::Progress,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            sources: Vec::new(),
            beeper_data_dir: None,
            media: true,
            progress: frankweiler_etl::progress::Progress::noop(),
        }
    }
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub rooms: usize,
    pub users: usize,
    pub events: usize,
    pub blobs: usize,
    pub blob_errors: usize,
    /// Rows backfilled with `external_event_id` from a
    /// `local-*/megabridge.db` after the index.db pass.
    pub events_enriched: usize,
    /// Megabridge rows whose `mxid` didn't match anything we'd
    /// already ingested. Either index.db hadn't cached them yet, or
    /// the bridge retains messages the desktop app evicted.
    pub events_orphaned: usize,
}

#[instrument(skip_all, fields(db = %opts.db_path.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    if opts.sources.is_empty() {
        anyhow::bail!(
            "no sources configured; set e.g. `sources: [\"signal\", \"googlechat\"]`"
        );
    }
    let db_path = db_path_for(&opts.db_path);
    let dst = RawDb::open(&db_path)
        .await
        .with_context(|| format!("open dest doltlite {}", db_path.display()))?;

    let beeper_dir = opts
        .beeper_data_dir
        .clone()
        .unwrap_or_else(default_beeper_data_dir);
    let index_db_path = beeper_dir.join("index.db");
    let media_root = beeper_dir.join("media");

    if !index_db_path.is_file() {
        anyhow::bail!(
            "index.db not found at {}. Is Beeper Texts installed and has it run at \
             least once? (Pass --beeper-data-dir to override.)",
            index_db_path.display()
        );
    }
    // We don't open index.db via sqlx — see index_db.rs for the
    // reason. The path is handed straight to the reader, which
    // shells out to the system `sqlite3` CLI.

    let run_config = json!({
        "sources": opts.sources,
        "beeper_data_dir": beeper_dir.display().to_string(),
        "media": opts.media,
    });
    let run_id = dst.start_run(&run_config).await?;

    let mut summary = FetchSummary::default();
    let result = (async {
        index_db::ingest(
            &index_db_path,
            &dst,
            &media_root,
            &opts.sources,
            opts.media,
            &mut summary,
            &opts.progress,
        )
        .await?;
        // After the index.db spine is in place, walk every
        // local-*/megabridge.db that matches a requested network and
        // backfill external_event_id by joining on mxid. Cloud
        // bridges (slack/googlechat/…) have no local megabridge file
        // and are silently skipped.
        megabridge::enrich(&beeper_dir, &dst, &opts.sources, &mut summary).await?;
        Ok::<(), anyhow::Error>(())
    })
    .await;

    // The accumulators tracked operations performed (upserts,
    // attempts). Distinct row counts come from the DB itself
    // after the run — that's the ground truth a downstream
    // consumer cares about. We still keep `events_enriched` /
    // `events_orphaned` as accumulators because they describe
    // megabridge-pass operations, not row counts.
    let counts = match dst.row_counts().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                event = "beeper_row_counts_failed",
                error = %format!("{e:#}")
            );
            db::RowCounts::default()
        }
    };
    summary.rooms = counts.rooms;
    summary.users = counts.users;
    summary.events = counts.events;
    summary.blobs = counts.blobs;
    summary.blob_errors = counts.blob_errors;

    let summary_json = json!({
        "rooms": summary.rooms,
        "users": summary.users,
        "events": summary.events,
        "blobs": summary.blobs,
        "blob_errors": summary.blob_errors,
        "events_enriched": summary.events_enriched,
        "events_orphaned": summary.events_orphaned,
        "error": result.as_ref().err().map(|e| e.to_string()),
    });
    let status = if result.is_ok() { "ok" } else { "error" };
    let _ = dst.finish_run(run_id, status, &summary_json).await;
    result?;

    info!(
        event = "beeper_fetch_complete",
        rooms = summary.rooms,
        users = summary.users,
        events = summary.events,
        blobs = summary.blobs,
        blob_errors = summary.blob_errors,
        events_enriched = summary.events_enriched,
        events_orphaned = summary.events_orphaned,
    );
    Ok(summary)
}

// Re-export for the [`sync`] orchestrator to thread through.
pub fn resolve_beeper_data_dir(p: Option<&Path>) -> PathBuf {
    p.map(Path::to_path_buf)
        .unwrap_or_else(default_beeper_data_dir)
}
