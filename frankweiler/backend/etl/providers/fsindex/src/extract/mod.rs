//! `fsindex` extract entry point.
//!
//! Orchestration: open the DB, optionally checkout a branch, optionally
//! reset, load prior `file_stats` for the fast-rescan compare, walk
//! the tree, optionally stamp directories, then bulk-write. The
//! framework's commit-lifecycle rule (see
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! §"Commit lifecycle") puts `dolt_commit` on the orchestrator, not
//! here — `fetch` returns and the caller decides whether to commit.
//!
//! See [`EXTRACT.md`](../../EXTRACT.md) for the design.

pub mod db;
pub mod hash;
pub mod options;
pub mod schema_raw;
pub mod stamp;
pub mod walker;

use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::{info, warn};

use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;

pub use db::RawDb;

use options::{EffectiveOptions, FsindexYaml, Identity, OptionsCascade};
use schema_raw::{FileKind, ScanMetaRow, StampKind};

/// Per-source options for one `fetch` run. Mirrors the shape of
/// every other provider's `FetchOptions`.
pub struct FetchOptions {
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub source_name: String,
    pub root: PathBuf,
    pub target_doltlite_branch: Option<String>,
    pub no_stamp: bool,
    pub progress: Progress,
    pub control: ExtractControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub entries_scanned: usize,
    pub entries_rehashed: usize,
    pub entries_reused: usize,
    pub stamped_directories: usize,
    pub errors: usize,
}

/// Run one extract pass against `opts.root`.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&opts.db_path).await?,
    };
    if let Some(branch) = opts.target_doltlite_branch.as_deref() {
        db.checkout_branch(branch).await?;
    }
    let _ = opts.control.refetch_blobs;

    opts.progress
        .set_message(&format!("indexing {}", opts.root.display()));

    // Truncate-and-rebuild. We always wipe the data tables before
    // a fresh walk so deleted files fall out naturally — no separate
    // reconciliation pass to maintain. Dolt's prolly-tree alignment
    // means re-inserting identical rows is a no-op at the storage
    // layer, so the diff between commits stays exactly "what
    // changed semantically." The rescan cache survives by living
    // in-memory: we load the prior `file_stats` + `files.blake3`
    // BEFORE truncating, so the Unison-style (mtime,size,inode)
    // → reuse-cached-hash fast path still works.
    //
    // `reset_and_redownload` here means "ignore the cache too"
    // — force a full rehash. Useful for verifying nothing has
    // silently drifted between scans.
    let (prev_stats, prev_file_blake3s) = if opts.control.reset_and_redownload {
        (Default::default(), Default::default())
    } else {
        let s = db.load_prev_stats().await?;
        let b = db.load_prev_file_blake3s().await?;
        (s, b)
    };
    db.reset().await?;

    let default_stamp_kind = if cfg!(unix) {
        StampKind::Inode
    } else {
        StampKind::NoStamp
    };
    let walker = walker::Walker::new(
        &opts.root,
        &prev_stats,
        &prev_file_blake3s,
        default_stamp_kind,
    );
    let (mut scan_results, walker_errors, walker_summary) = walker.collect()?;

    // ── Stamping pass ────────────────────────────────────────────────
    // Runs AFTER walking but is OK ordering-wise because `.fsindex.yaml`
    // is excluded from every directory's tree-hash (see
    // schema_raw §"Directory tree-hash canonicalization"). The stamp
    // writes a UUID inside that excluded file, so no ancestor's
    // blake3 is invalidated. This load-bearing ordering is what
    // avoids a rehash storm on first-stamp.
    let mut stamped = 0_usize;
    if !opts.no_stamp {
        stamped = stamp_directories(&opts.root, &mut scan_results).await?;
        if stamped > 0 {
            warn!(
                event = "fsindex_stamping_active",
                stamped = stamped,
                message =
                    "stamping is on — set `--no-stamp` or remove `stamp_me_with_uuid: true` to disable",
            );
        }
    }

    // ── scan_meta ────────────────────────────────────────────────────
    let root_cascade = build_root_cascade(&opts.root);
    let effective = root_cascade.effective();
    let options_fp = options::options_fingerprint(&effective);

    let os = std::env::consts::OS.to_string();
    // FIXME(case_sensitive-heuristic): assume case-insensitive on
    // macOS default volumes, case-sensitive elsewhere. Real detection
    // would `statvfs` or probe `pathconf(_PC_CASE_SENSITIVE)`.
    let case_sensitive = !matches!(os.as_str(), "macos");
    // FIXME(inode_stable-heuristic): assumed true for now; real
    // detection would probe filesystem type per-mount.
    let inode_stable = true;
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let scan_meta = ScanMetaRow {
        id: opts.source_name.clone(),
        abs_path: opts.root.to_string_lossy().into_owned(),
        os,
        case_sensitive,
        inode_stable,
        options_fingerprint: options_fp,
        last_scan_at: now.clone(),
        scanner_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let entries_scanned = scan_results.len();
    let files: Vec<_> = scan_results.iter().map(|r| r.file_row.clone()).collect();
    let stats: Vec<_> = scan_results.iter().map(|r| r.stat_row.clone()).collect();
    db.bulk_write_scan(&files, &stats, &scan_meta, &now).await?;

    // Walker errors → durable bookkeeping trail on `files`.
    for err in &walker_errors {
        db.record_error("files", &err.id, &err.message)
            .await
            .with_context(|| format!("record walker error for {}", err.id))?;
    }

    Ok(FetchSummary {
        entries_scanned,
        entries_rehashed: walker_summary.rehashed,
        entries_reused: walker_summary.reused,
        stamped_directories: stamped,
        errors: walker_errors.len(),
    })
}

async fn stamp_directories(
    root: &std::path::Path,
    scan_results: &mut [walker::ScanResult],
) -> Result<usize> {
    let mut count = 0_usize;
    for sr in scan_results.iter_mut() {
        if !matches!(sr.file_row.kind, FileKind::Dir) {
            continue;
        }
        let dir = if sr.file_row.id.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&sr.file_row.id)
        };
        let cascade = build_dir_cascade(root, &dir);
        let eff = cascade.effective();
        if !eff.stamp_me_with_uuid {
            continue;
        }
        let mut yaml = options::load_at(&dir)?.unwrap_or_default();
        if let Some(id) = &yaml.identity {
            sr.file_row.identity_uuid = Some(id.uuid.clone());
            continue;
        }
        let uuid = new_uuid();
        let stamped_at = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let originally_at = dir
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        yaml.identity = Some(Identity {
            uuid: uuid.clone(),
            stamped_at,
            stamper_version: 1,
            originally_at,
        });
        options::write_breadcrumb(&dir, &yaml)
            .with_context(|| format!("write breadcrumb {}", dir.display()))?;
        sr.file_row.identity_uuid = Some(uuid.clone());
        info!(
            event = "fsindex_stamped",
            path = %dir.display(),
            uuid = %uuid,
        );
        count += 1;
    }
    Ok(count)
}

/// UUIDv7: time-ordered, so breadcrumb UUIDs sort chronologically
/// by stamp time. Doesn't change the not-a-PK / `cp -r`-produces-
/// duplicates story; just a small ergonomic win when humans read
/// a sorted list of identities.
fn new_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn build_root_cascade(root: &std::path::Path) -> OptionsCascade {
    let mut c = OptionsCascade::new();
    if let Ok(Some(y)) = options::load_at(root) {
        c.push(root.to_path_buf(), y);
    }
    c
}

fn build_dir_cascade(root: &std::path::Path, dir: &std::path::Path) -> OptionsCascade {
    let mut c = OptionsCascade::new();
    let mut cur = root.to_path_buf();
    if let Ok(Some(y)) = options::load_at(&cur) {
        c.push(cur.clone(), y);
    }
    if let Ok(rel) = dir.strip_prefix(root) {
        for part in rel.iter() {
            cur.push(part);
            if let Ok(Some(y)) = options::load_at(&cur) {
                c.push(cur.clone(), y);
            }
        }
    }
    c
}

// Keep types referenced even if not used at the module surface.
const _: Option<EffectiveOptions> = None;
const _: Option<FsindexYaml> = None;
