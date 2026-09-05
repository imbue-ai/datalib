//! The `media` download side: walk a tree, work out what each audio,
//! image, video and playlist file is, and record it.
//!
//! There is no render side — see the crate docs. This is the whole
//! provider.
//!
//! The shape follows `pdf`: load the rescan cache, truncate the
//! path-keyed tables so deletions fall out, walk, and do per-item work
//! only for content we have not seen. What differs is what "per-item
//! work" means. `pdf` classifies; here it is a container sniff, a
//! payload-hash plan, and a metadata read — all of which are keyed on
//! content, so N copies of one song are parsed once.

pub mod db;
pub mod kind;
pub mod meta;
pub mod payload;
pub mod playlist;
pub mod schema_raw;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan;
use datalib_etl::fswalk;
use datalib_etl::progress::Progress;

pub use db::{db_path_for, RawDb, WriteBatch};
use kind::{Container, MediaClass};
use schema_raw::{
    MediaAudioRow, MediaFileRow, MediaItemRow, MediaPlaylistEntryRow, MediaPlaylistRow,
    MediaScanMetaRow, MediaVisualRow,
};

/// Rows flushed to the store at a time. Bounded so memory stays flat on
/// a library of any size; the batch is a transaction, not a commit.
const BATCH_SIZE: usize = 2_000;

/// Refuse to parse a playlist bigger than this. A real one is
/// kilobytes; a multi-megabyte `.m3u8` is a generated stream manifest
/// or a corrupt file.
const MAX_PLAYLIST_BYTES: u64 = 16 * 1024 * 1024;

pub struct FetchOptions {
    pub db: RawDb,
    /// Source name from config, used as the `media_scan_meta` key.
    pub source_name: String,
    /// Tree to scan.
    pub root: PathBuf,
    pub ignore: Vec<String>,
    /// This host's shared fingerprint cache. Host state, so it lives
    /// outside the scan store — see [`datalib_etl::fingerprint_cache`].
    pub cache: FingerprintCache,
    pub max_bytes: Option<u64>,
    pub payload_max_bytes: Option<u64>,
    pub playlists: bool,
    pub skip_dataless: bool,
    /// Ignore the rescan cache and re-read every file. Wired to the
    /// framework's `--reset-and-redownload`.
    pub force_rehash: bool,
    /// Run-pinned "now", per AGENTS.md — steps prefer `DATALIB_DAG_NOW`
    /// over sampling their own clock so one run's outputs agree.
    pub now: String,
    pub progress: Progress,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    /// Everything the walk covered, playlists included. `hashed +
    /// reused` accounts for exactly this — which `files_seen` does not,
    /// since it excludes playlists.
    pub entries_scanned: usize,
    /// Media files visited (playlists excluded).
    pub files_seen: usize,
    /// Files whose bytes were actually read and hashed — playlists
    /// among them. They are hashed once, by the scan, rather than again
    /// when they are parsed.
    pub hashed: usize,
    /// Files whose digest came from this host's fingerprint cache, so
    /// no bytes were read.
    pub reused: usize,
    /// Distinct items (by content) behind those paths.
    pub items: usize,
    pub audio: usize,
    pub images: usize,
    pub videos: usize,
    /// Items that got a metadata-excluding payload hash.
    pub payload_hashed: usize,
    /// Items whose container has no payload recipe, or which were past
    /// `payload_max_bytes`.
    pub payload_skipped: usize,
    pub playlists: usize,
    pub playlist_entries: usize,
    /// Entries naming a path inside the scanned tree. Says nothing
    /// about whether a file is at that path — that is a join, not a
    /// scan-time fact.
    pub playlist_entries_in_tree: usize,
    /// `.m3u8` files that turned out to be HLS stream manifests.
    pub hls_skipped: usize,
    /// Files skipped for being cloud placeholders with no local data.
    pub dataless_skipped: usize,
    /// Paths skipped for exceeding `max_bytes`.
    pub too_large: usize,
    /// Path rows dropped because this scan did not see them — files and
    /// playlists that are gone from the tree.
    pub removed: usize,
    pub errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let mut summary = FetchSummary::default();

    // The rescan cache, read once so the walk never touches the
    // database. Nothing is deleted here: stale rows are swept at the
    // *end* of the scan, which is what lets an interrupted run leave
    // usable cursors behind (see `RawDb::sweep_unseen`).
    let mut prev = opts.db.load_prev().await.context("load rescan cache")?;

    // Written before the walk, so an interrupted scan still leaves a
    // record of which tree it was reading.
    opts.db
        .write_scan_meta(&MediaScanMetaRow {
            id: opts.source_name.clone(),
            abs_root: opts.root.to_string_lossy().to_string(),
            scanned_at: opts.now.clone(),
        })
        .await
        .context("record scan root")?;

    // One walk, hashing only what this host's shared cache cannot
    // vouch for. `scan_with` rather than `scan` because a dataless file
    // — evicted to iCloud — must not be read at all: it has a size and
    // an mtime, and touching a byte silently pulls the whole thing back
    // over the network.
    let dataless_skipped = std::sync::atomic::AtomicUsize::new(0);
    let scan = fsscan::scan_with(
        &opts.cache,
        &opts.root,
        &fsscan::ScanOptions {
            ignore: opts.ignore.clone(),
            max_bytes: opts.max_bytes,
            force_rehash: opts.force_rehash,
        },
        kind::accept,
        |path, meta| {
            if opts.skip_dataless && is_dataless(meta) {
                dataless_skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(path = %path.display(), "media_skipped_dataless");
                return false;
            }
            true
        },
    )
    .await?;
    summary.errors += scan.errors.len();
    for e in &scan.errors {
        tracing::warn!(path = %e.path.display(), error = %e.error, "media_walk_error");
    }
    summary.dataless_skipped = dataless_skipped.into_inner();
    summary.entries_scanned = scan.files.len();
    summary.too_large = scan.stats.too_large;
    summary.hashed = scan.stats.hashed;
    summary.reused = scan.stats.reused;
    opts.progress.set_length(Some(scan.files.len() as u64));

    let mut playlist_files = Vec::new();
    let mut batch = WriteBatch::default();
    // Items identified during *this* scan, so N copies of one file are
    // parsed once rather than N times.
    let mut seen_items: std::collections::HashSet<String> = std::collections::HashSet::new();

    for f in &scan.files {
        opts.progress.inc(1);

        if kind::is_playlist_extension(&f.path) {
            playlist_files.push(f.clone());
            continue;
        }
        summary.files_seen += 1;
        let hash_hex = fswalk::to_hex(&f.blake3);
        // Taking the entry out is how this path is marked as still
        // indexed; what remains in `prev.paths` at the end is the set
        // of rows to delete.
        prev.paths.remove(&f.rel);

        // ── Identify, once per distinct content ──────────────────────
        if !prev.known_items.contains(&hash_hex) && !seen_items.contains(&hash_hex) {
            match identify(&f.path, f.size, &hash_hex, &opts) {
                Ok(id) => {
                    seen_items.insert(hash_hex.clone());
                    summary.items += 1;
                    match id.item.media_class {
                        MediaClass::Audio => summary.audio += 1,
                        MediaClass::Image => summary.images += 1,
                        MediaClass::Video => summary.videos += 1,
                    }
                    if id.item.payload_blake3.is_some() {
                        summary.payload_hashed += 1;
                    } else {
                        summary.payload_skipped += 1;
                    }
                    batch.items.push(id.item);
                    if let Some(a) = id.audio {
                        batch.audio.push(a);
                    }
                    if let Some(v) = id.visual {
                        batch.visual.push(v);
                    }
                }
                Err(e) => {
                    summary.errors += 1;
                    tracing::warn!(path = %f.rel, error = %e, "media_identify_failed");
                    continue;
                }
            }
        }

        batch.files.push(MediaFileRow {
            id: f.rel.clone(),
            blake3: hash_hex,
            last_seen_at: opts.now.clone(),
        });

        if batch.len() >= BATCH_SIZE {
            opts.db.write_batch(&batch).await?;
            batch.clear();
        }
    }
    opts.db.write_batch(&batch).await?;

    if opts.playlists {
        scan_playlists(&opts, &playlist_files, &mut prev, &mut summary).await?;
    }

    // Reconcile last. Whatever is still in the cache was never visited,
    // so it is a path that is gone.
    //
    // Doing it here rather than up front is what makes an interrupted
    // scan cheap: a run killed before this point leaves every row it
    // had, and a stale `(mtime, size, inode, dev)` is exactly as good a
    // cursor as a fresh one. The cost is the opposite window — rows for
    // deleted files linger until a scan runs to completion — which is
    // the safe direction of the two.
    let gone_files: Vec<String> = prev.paths.into_keys().collect();
    let gone_playlists: Vec<String> = prev.playlists.into_iter().collect();
    summary.removed = (opts.db.delete_files(&gone_files).await?
        + opts
            .db
            .delete_playlists(&gone_playlists)
            .await
            .context("delete vanished playlists")?) as usize;
    Ok(summary)
}

/// A cloud placeholder: the file has a size but no allocated blocks, so
/// its bytes are not here.
///
/// Reading one is not a cheap mistake — it asks Dropbox or iCloud to
/// materialize the file, so a first scan of an evicted library would
/// try to pull the whole thing down. Skipping is the safe default and
/// every skip is counted into `dataless_skipped=`.
///
/// It is a heuristic: a filesystem that reports no block counts at all
/// looks entirely evicted, which is why `skip_dataless` can be turned
/// off. iCloud's `.icloud` eviction markers need no handling here —
/// they are named `.track.mp3.icloud`, so `kind::accept` never visits
/// them in the first place.
///
/// Consulted through [`fsscan::scan_with`]'s admit hook, which is the
/// only place that sees a stat *before* anything reads the file.
#[cfg(unix)]
fn is_dataless(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    md.blocks() == 0 && md.size() > 0
}

#[cfg(not(unix))]
fn is_dataless(_md: &std::fs::Metadata) -> bool {
    false
}

/// What one distinct item's worth of bytes turned out to be.
struct Identified {
    item: MediaItemRow,
    audio: Option<MediaAudioRow>,
    visual: Option<MediaVisualRow>,
}

fn identify(path: &Path, size: i64, blake3: &str, opts: &FetchOptions) -> Result<Identified> {
    let head = read_head(path)?;
    let container = Container::sniff(&head);
    let class = kind::resolve_class(container, path);

    // The payload hash is a second pass over the file, so it is the one
    // thing gated on size. NULL above the ceiling, and the summary says
    // how often that happened.
    let payload = match opts.payload_max_bytes {
        Some(max) if size as u64 > max => {
            tracing::debug!(path = %path.display(), size, "media_payload_skipped_too_large");
            None
        }
        _ => payload::compute(path, container)
            .with_context(|| format!("payload hash {}", path.display()))?,
    };

    let m = meta::extract(path, class, container);
    let item = MediaItemRow {
        blake3: blake3.to_string(),
        size,
        media_class: class,
        container,
        codec: m.codec.clone(),
        duration_ms: m.duration_ms,
        payload_blake3: payload.as_ref().map(|p| p.blake3.clone()),
        payload_scheme: payload.as_ref().map(|p| p.scheme.to_string()),
        first_seen_at: opts.now.clone(),
    };

    let audio = m.audio.map(|a| MediaAudioRow {
        blake3: blake3.to_string(),
        title: a.title,
        artist: a.artist,
        album: a.album,
        album_artist: a.album_artist,
        composer: a.composer,
        genre: a.genre,
        date: a.date,
        track_no: a.track_no,
        track_total: a.track_total,
        disc_no: a.disc_no,
        disc_total: a.disc_total,
        bitrate_kbps: a.bitrate_kbps,
        sample_rate_hz: a.sample_rate_hz,
        channels: a.channels,
        bit_depth: a.bit_depth,
    });
    let visual = m.visual.map(|v| MediaVisualRow {
        blake3: blake3.to_string(),
        width: v.width,
        height: v.height,
        orientation: v.orientation,
        captured_at: v.captured_at,
        camera_make: v.camera_make,
        camera_model: v.camera_model,
        lens_model: v.lens_model,
        iso: v.iso,
        exposure_time: v.exposure_time,
        f_number: v.f_number,
        focal_length_mm: v.focal_length_mm,
        gps_lat: v.gps_lat,
        gps_lon: v.gps_lon,
        gps_altitude_m: v.gps_altitude_m,
        title: v.title,
        caption: v.caption,
        frame_rate: v.frame_rate,
        video_codec: v.video_codec,
        audio_codec: v.audio_codec,
    });

    Ok(Identified {
        item,
        audio,
        visual,
    })
}

fn read_head(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = vec![0u8; kind::SNIFF_LEN];
    let n = f
        .read(&mut buf)
        .with_context(|| format!("read head of {}", path.display()))?;
    buf.truncate(n);
    Ok(buf)
}

async fn scan_playlists(
    opts: &FetchOptions,
    files: &[fsscan::ScannedFile],
    prev: &mut db::PrevCache,
    summary: &mut FetchSummary,
) -> Result<()> {
    let mut rows: Vec<MediaPlaylistRow> = Vec::new();
    let mut entries: Vec<MediaPlaylistEntryRow> = Vec::new();

    for f in files {
        let size = f.size as u64;
        if size == 0 || size > MAX_PLAYLIST_BYTES {
            continue;
        }
        // No dataless check here: the scan's admit hook already refused
        // to read those, so they never reached this list.
        let bytes = match std::fs::read(&f.path) {
            Ok(b) => b,
            Err(e) => {
                summary.errors += 1;
                tracing::warn!(path = %f.rel, error = %e, "media_playlist_read_failed");
                continue;
            }
        };
        // Seen, whatever we decide about it below — an HLS manifest we
        // skip is still not a playlist that vanished.
        prev.playlists.remove(&f.rel);

        let parsed = playlist::parse(&bytes);
        if parsed.is_hls {
            // A stream manifest, not something a person made. Counted
            // so a corpus full of them is visible rather than puzzling.
            summary.hls_skipped += 1;
            tracing::debug!(path = %f.rel, "media_playlist_is_hls");
            // A file that *became* an HLS manifest since the last scan
            // must lose its old playlist row, which the cache-consuming
            // pass above would otherwise have spared.
            opts.db
                .delete_playlists(std::slice::from_ref(&f.rel))
                .await?;
            continue;
        }

        // A playlist is rewritten whole. Clearing first is what makes a
        // *shortened* playlist lose its trailing entries, which an
        // upsert keyed on `<path>#<position>` would leave behind.
        opts.db.clear_playlist_entries(&f.rel).await?;

        // Already hashed by the scan; re-reading would be a second
        // pass over bytes we hold the digest for.
        let hash = fswalk::to_hex(&f.blake3);
        for e in &parsed.entries {
            // `resolve` is pure string work against the playlist's own
            // path — no I/O, no database. Whether a file is actually
            // there is a join at read time, not a column: see
            // `MediaPlaylistEntryRow::resolved_path`.
            let resolved_path = playlist::resolve(&e.target_raw, e.target_kind, &f.rel);
            if resolved_path.is_some() {
                summary.playlist_entries_in_tree += 1;
            }
            entries.push(MediaPlaylistEntryRow {
                id: format!("{}#{}", f.rel, e.position),
                playlist_id: f.rel.clone(),
                position: e.position,
                target_raw: e.target_raw.clone(),
                target_kind: e.target_kind.as_str().to_string(),
                resolved_path,
                ext_title: e.ext_title.clone(),
                ext_duration_s: e.ext_duration_s,
            });
        }

        summary.playlists += 1;
        summary.playlist_entries += parsed.entries.len();
        rows.push(MediaPlaylistRow {
            id: f.rel.clone(),
            blake3: hash,
            format: playlist::format_of(&f.path).to_string(),
            title: parsed.title.clone(),
            entry_count: parsed.entries.len() as i64,
            last_seen_at: opts.now.clone(),
        });

        if entries.len() >= BATCH_SIZE {
            opts.db.write_playlists(&rows, &entries).await?;
            rows.clear();
            entries.clear();
        }
    }
    opts.db.write_playlists(&rows, &entries).await?;
    Ok(())
}
