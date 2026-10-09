//! Google Takeout extractor entry point.

pub mod attachment_path;
pub mod db;
pub mod gemini_apps;
pub mod google_chat;
pub mod google_voice;
pub mod maps_photos;
pub mod maps_reviews;
pub mod maps_saved_places;
pub mod mdl_html;
pub mod schema_raw;
pub mod time;
pub mod unpack;
pub mod youtube_subscriptions;
pub mod youtube_watch_history;

pub use db::{db_path_for, RawDb};

use datalib_etl::download_problems::RunProblemKind;
use datalib_etl_files::fingerprint_cache::{abs_key, canonical_root, FingerprintCache};
use datalib_etl_files::{file_checkpoint, fsscan};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// One switch per Takeout feed. Defaults are all `false` — a fresh
/// user has to enable each feed consciously; INGEST.md says why
/// that matters and what each flag writes.
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
    /// Google Voice (`Voice/` subtree): texts, voicemails, calls, bills.
    pub google_voice: bool,
    /// When `google_voice` is on, also process the `Voice/Spam/` folder
    /// (download + render). Off by default — spam is bulky and only
    /// useful for parser hardening / practice corpora.
    pub google_voice_include_spam: bool,
}

impl SyncFlags {
    pub fn all() -> Self {
        Self {
            maps_reviews: true,
            maps_saved_places: true,
            maps_photos: true,
            youtube_watch_history: true,
            youtube_subscriptions: true,
            google_chat: true,
            gemini_apps: true,
            google_voice: true,
            google_voice_include_spam: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Root of the user's Takeout export: the unpacked `Takeout/` folder
    /// (the one holding `Maps (your places)/`, `Google Chat/` and the
    /// rest), or a folder holding the `.zip` or `.tgz` parts of one export.
    pub input_path: PathBuf,
    /// Host-wide fingerprint cache: the shared answer to "did this
    /// file change?". Every feed's resume cursor is a content hash
    /// read through it, so an unchanged export costs a `stat` per
    /// file and no re-reads.
    pub cache: FingerprintCache,
    /// Per-feed opt-in switches.
    pub sync: SyncFlags,
    pub progress: Progress,
    pub control: DownloadControl,
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
    pub voice_messages: usize,
    pub voice_bills: usize,
    pub voice_greetings: usize,
    pub voice_attachments: usize,
    pub blobs_stored: usize,
    /// Feeds that failed as a whole, each a `phase:<feed>` problem row.
    pub feeds_failed: usize,
    /// Records deleted because the export no longer holds them.
    pub removed: usize,
    /// Export files that are gone since the last run, in the feeds read.
    pub files_removed: usize,
    /// The archive parts the export came as; 0 for an unpacked one.
    pub archives: usize,
    /// Files unpacked from them. 0 with parts means nothing in them
    /// changed since a run last finished reading them, so none was read.
    pub unpacked: usize,
}

/// Whether the export holds `product_dir` at all. Only a product that is
/// here says what was deleted from it: one missing entirely was left out of
/// the Takeout request, so its records stay and its cursor is kept.
pub(crate) fn product_exported(scan: &fsscan::Scan, product_dir: &str) -> bool {
    scan.files
        .iter()
        .any(|f| fsscan::is_under(&f.rel, product_dir))
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_export(opts, found)).await
}

async fn read_export(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let parts = unpack::parts_in(&opts.input_path)?;
    if parts.is_empty() {
        let (summary, _) = read_tree(&opts, &opts.input_path, &found).await?;
        return Ok(summary);
    }
    read_archives(&opts, &parts, &found).await
}

/// Hash the parts (a `stat` each once the cache has them), and unless the
/// feeds that are on already finished reading these very parts, unpack what
/// they read into a temporary directory and read that.
async fn read_archives(
    opts: &FetchOptions,
    parts: &[String],
    found: &RunProblems,
) -> Result<FetchSummary> {
    let names: BTreeSet<&str> = parts.iter().map(String::as_str).collect();
    let scan_opts = fsscan::ScanOptions {
        max_depth: Some(1),
        progress: opts.progress.clone(),
        ..Default::default()
    };
    let hashed = fsscan::scan(&opts.cache, &opts.input_path, &scan_opts, |p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| names.contains(n))
    })
    .await?;
    if hashed.files.len() != parts.len() {
        let read: BTreeSet<&str> = hashed.files.iter().map(|f| f.rel.as_str()).collect();
        let missing: Vec<&str> = names.difference(&read).copied().collect();
        bail!(
            "{} of the export's {} parts could not be read: {}",
            missing.len(),
            parts.len(),
            missing.join(", ")
        );
    }

    let pool = opts.db.pool();
    let stamps = unpack::stamps(&opts.sync, &hashed.files);
    let prev = file_checkpoint::load_cursor(pool, unpack::SCOPE).await?;
    if unpack::unchanged(&opts.sync, &prev, &stamps) {
        info!(
            event = "takeout_archives_unchanged",
            parts = parts.len(),
            "the export's parts are the ones last read; nothing to read"
        );
        return Ok(FetchSummary {
            archives: parts.len(),
            ..FetchSummary::default()
        });
    }

    let tmp = tempfile::tempdir().context("create a directory to unpack the export into")?;
    let root = tmp.path().join("Takeout");
    std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    opts.progress
        .set_message(&format!("unpacking {} parts of the export", parts.len()));
    let unpacked = {
        let paths: Vec<PathBuf> = hashed.files.iter().map(|f| f.path.clone()).collect();
        let (sync, dest, stop) = (opts.sync.clone(), root.clone(), opts.control.stop.clone());
        tokio::task::spawn_blocking(move || unpack::unpack(&paths, &sync, &dest, &stop))
            .await
            .context("unpack task panicked")??
    };
    let read = read_tree(opts, &root, found).await;
    forget_fingerprints_under(&opts.cache, &root).await?;
    let (mut summary, scan) = read?;

    let finished =
        !opts.control.stop.requested() && summary.feeds_failed == 0 && scan.errors.is_empty();
    if finished {
        file_checkpoint::clear_scope(pool, unpack::SCOPE).await?;
        for stamp in &stamps {
            file_checkpoint::record_file_pool(pool, unpack::SCOPE, stamp).await?;
        }
    }
    summary.archives = parts.len();
    summary.unpacked = unpacked;
    Ok(summary)
}

/// The host-wide cache keeps a fingerprint per path, and a temporary
/// directory's paths are never seen again.
async fn forget_fingerprints_under(cache: &FingerprintCache, root: &Path) -> Result<()> {
    let root = canonical_root(root);
    let cached = cache.load_under(&root).await?;
    let keys: Vec<String> = cached.paths().map(|rel| abs_key(&root, rel)).collect();
    cache.forget(&keys).await?;
    Ok(())
}

/// Every feed that is on, over the unpacked export at `root`.
async fn read_tree(
    opts: &FetchOptions,
    root: &Path,
    found: &RunProblems,
) -> Result<(FetchSummary, fsscan::Scan)> {
    let db = opts.db.clone();

    let mut summary = FetchSummary::default();
    let progress = &opts.progress;
    // One scan of the export, up front, so the walk and the hashing happen
    // once rather than nine times in nine slightly different shapes. The first
    // run hashes everything; later runs are `stat`-only, and a feed enabled
    // later costs nothing extra because its files are already in the cache.
    let walked = fsscan::scan(&opts.cache, root, &fsscan::ScanOptions::default(), |_| true).await?;
    for e in &walked.errors {
        warn!(event = "takeout_walk_error", path = %e.path.display(), error = %e.error, "an entry of the export could not be walked");
    }
    let scan = &walked;
    scan.report_problems(found, "files");

    if opts.sync.maps_reviews {
        if let Some(n) = found
            .run_phase(
                "maps_reviews",
                maps_reviews::ingest(&db, scan, progress, found),
            )
            .await
        {
            summary.maps_reviews = n.written;
            summary.removed += n.removed;
        }
    }
    if opts.sync.maps_saved_places {
        if let Some(n) = found
            .run_phase(
                "maps_saved_places",
                maps_saved_places::ingest(&db, scan, progress, found),
            )
            .await
        {
            summary.maps_saved_places = n.written;
            summary.removed += n.removed;
        }
    }
    if opts.sync.maps_photos {
        if let Some(s) = found
            .run_phase(
                "maps_photos",
                maps_photos::ingest(&db, scan, progress, found),
            )
            .await
        {
            summary.maps_photos = s.rows;
            summary.blobs_stored += s.blobs;
            summary.removed += s.removed;
            summary.files_removed += s.files_removed;
        }
    }
    if opts.sync.youtube_watch_history {
        if let Some(n) = found
            .run_phase(
                "youtube_watch_history",
                youtube_watch_history::ingest(&db, scan, progress, found),
            )
            .await
        {
            summary.youtube_watch_history = n.written;
            summary.removed += n.removed;
        }
    }
    if opts.sync.youtube_subscriptions {
        if let Some(n) = found
            .run_phase(
                "youtube_subscriptions",
                youtube_subscriptions::ingest(&db, scan, progress, found),
            )
            .await
        {
            summary.youtube_subscriptions = n.written;
            summary.removed += n.removed;
        }
    }
    if opts.sync.google_chat {
        if let Some(s) = found
            .run_phase(
                "google_chat",
                google_chat::ingest(&db, scan, progress, found),
            )
            .await
        {
            summary.chat_groups += s.groups;
            summary.chat_users += s.users;
            summary.chat_messages += s.messages;
            summary.chat_attachments += s.attachments;
            summary.blobs_stored += s.blobs_stored;
            summary.removed += s.removed;
            summary.files_removed += s.files_removed;
        }
    }
    if opts.sync.gemini_apps {
        if let Some(s) = found
            .run_phase(
                "gemini_apps",
                gemini_apps::ingest(&db, scan, progress, found),
            )
            .await
        {
            summary.gemini_activity += s.activity;
            summary.gemini_attachments += s.attachments;
            summary.blobs_stored += s.blobs_stored;
            summary.removed += s.removed;
        }
    }
    if opts.sync.google_voice {
        let voice = google_voice::ingest(
            &db,
            scan,
            opts.sync.google_voice_include_spam,
            progress,
            found,
        );
        if let Some(s) = found.run_phase("google_voice", voice).await {
            summary.voice_messages += s.messages;
            summary.voice_bills += s.bills;
            summary.voice_greetings += s.greetings;
            summary.voice_attachments += s.attachments;
            summary.blobs_stored += s.blobs_stored;
            summary.removed += s.removed;
            summary.files_removed += s.files_removed;
            found.extend(s.held_back);
        }
    }
    summary.feeds_failed = found.count(RunProblemKind::Phase);

    Ok((summary, walked))
}

/// A file that is not in the layout its reader knows: `what` says how.
/// Nothing is stored or deleted on its word, and the feed fails where a
/// person sees it, so a newer export Google reshaped cannot empty a table.
pub(crate) fn unknown_layout(file: &str, what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{file} {what}, so it is not in a layout this reader knows; nothing was stored or deleted"
    )
}

/// A file that lists entries none of which could be read is a layout
/// that moved, not a product emptied upstream.
pub(crate) fn require_some_read(file: &str, listed: usize, read: usize) -> Result<()> {
    if listed > 0 && read == 0 {
        return Err(unknown_layout(
            file,
            &format!("lists {listed} entries and none could be read"),
        ));
    }
    Ok(())
}
