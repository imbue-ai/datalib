//! Google Takeout extractor entry point.
//!
//! Walks the on-disk Takeout tree under [`FetchOptions::input_path`]
//! and dispatches each enabled sub-feed walker. Sub-feeds are opted
//! in individually via [`SyncFlags`] so a fresh user has to enable
//! each one consciously.

pub mod db;
pub mod gemini_apps;
pub mod google_chat;
pub mod maps_photos;
pub mod maps_reviews;
pub mod maps_saved_places;
pub mod mdl_html;
pub mod schema_raw;
pub mod time;
pub mod youtube_subscriptions;
pub mod youtube_watch_history;

pub use db::{db_path_for, RawDb};

use std::path::PathBuf;

use anyhow::Result;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Opt-in switches matching the YAML `sync:` block in
/// `docs/google_takeout_ingestion.md`. Defaults are all `false` —
/// a fresh user has to enable each feed consciously.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncFlags {
    pub maps_reviews: bool,
    pub maps_saved_places: bool,
    pub maps_photos: bool,
    pub youtube_watch_history: bool,
    pub youtube_subscriptions: bool,
    pub google_chat: bool,
    pub gemini_apps: bool,
}

impl SyncFlags {
    /// Convenience: every feed enabled. Tests use this.
    pub fn all() -> Self {
        Self {
            maps_reviews: true,
            maps_saved_places: true,
            maps_photos: true,
            youtube_watch_history: true,
            youtube_subscriptions: true,
            google_chat: true,
            gemini_apps: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Doltlite database path. Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. The sync orchestrator populates this so the
    /// post-extract commit hits the same pool.
    pub db: Option<RawDb>,
    /// Root of the user's Takeout export (the directory that contains
    /// `Maps (your places)/`, `YouTube and YouTube Music/`,
    /// `Google Chat/`, etc.). May or may not be the literal
    /// `Takeout/` subdirectory of a Takeout zip.
    pub input_path: PathBuf,
    /// Per-feed opt-in switches.
    pub sync: SyncFlags,
    pub progress: Progress,
    pub control: ExtractControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            input_path: PathBuf::new(),
            sync: SyncFlags::default(),
            progress: Progress::noop(),
            control: ExtractControl::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    pub maps_reviews: usize,
    pub maps_saved_places: usize,
    pub maps_photos: usize,
    pub youtube_watch_history: usize,
    pub youtube_subscriptions: usize,
    pub chat_groups: usize,
    pub chat_users: usize,
    pub chat_messages: usize,
    pub chat_attachments: usize,
    pub gemini_activity: usize,
    pub gemini_attachments: usize,
    pub blobs_stored: usize,
    pub parse_errors: usize,
}

/// Run one extract pass.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path_for(&opts.db_path)).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    // No upstream blob fetches — `--refetch-blobs` is a no-op for
    // this provider. Files on disk are the source of truth.
    let _ = opts.control.refetch_blobs;

    let mut summary = FetchSummary::default();
    let root = &opts.input_path;
    let progress = &opts.progress;

    if opts.sync.maps_reviews {
        match maps_reviews::ingest(&db, root, progress).await {
            Ok(n) => summary.maps_reviews = n,
            Err(e) => {
                warn!(event = "google_takeout_feed_failed", feed = "maps_reviews", error = %e);
                summary.parse_errors += 1;
            }
        }
    }
    if opts.sync.maps_saved_places {
        match maps_saved_places::ingest(&db, root, progress).await {
            Ok(n) => summary.maps_saved_places = n,
            Err(e) => {
                warn!(event = "google_takeout_feed_failed", feed = "maps_saved_places", error = %e);
                summary.parse_errors += 1;
            }
        }
    }
    if opts.sync.maps_photos {
        match maps_photos::ingest(&db, root, progress).await {
            Ok((rows, blobs)) => {
                summary.maps_photos = rows;
                summary.blobs_stored += blobs;
            }
            Err(e) => {
                warn!(event = "google_takeout_feed_failed", feed = "maps_photos", error = %e);
                summary.parse_errors += 1;
            }
        }
    }
    if opts.sync.youtube_watch_history {
        match youtube_watch_history::ingest(&db, root, progress).await {
            Ok(n) => summary.youtube_watch_history = n,
            Err(e) => {
                warn!(event = "google_takeout_feed_failed", feed = "youtube_watch_history", error = %e);
                summary.parse_errors += 1;
            }
        }
    }
    if opts.sync.youtube_subscriptions {
        match youtube_subscriptions::ingest(&db, root, progress).await {
            Ok(n) => summary.youtube_subscriptions = n,
            Err(e) => {
                warn!(event = "google_takeout_feed_failed", feed = "youtube_subscriptions", error = %e);
                summary.parse_errors += 1;
            }
        }
    }
    if opts.sync.google_chat {
        match google_chat::ingest(&db, root, progress).await {
            Ok(s) => {
                summary.chat_groups += s.groups;
                summary.chat_users += s.users;
                summary.chat_messages += s.messages;
                summary.chat_attachments += s.attachments;
                summary.blobs_stored += s.blobs_stored;
            }
            Err(e) => {
                warn!(event = "google_takeout_feed_failed", feed = "google_chat", error = %e);
                summary.parse_errors += 1;
            }
        }
    }
    if opts.sync.gemini_apps {
        match gemini_apps::ingest(&db, root, progress).await {
            Ok(s) => {
                summary.gemini_activity += s.activity;
                summary.gemini_attachments += s.attachments;
                summary.blobs_stored += s.blobs_stored;
            }
            Err(e) => {
                warn!(event = "google_takeout_feed_failed", feed = "gemini_apps", error = %e);
                summary.parse_errors += 1;
            }
        }
    }

    Ok(summary)
}
