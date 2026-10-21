//! Google Drive → doltlite, for the account's Docs, Sheets and Slides.
//!
//! One listing names every file at its Drive `version`. A file is owed
//! when the store does not hold it at that version (`owed::owed` over
//! `files`), and capturing it fetches, in one go: its revision list, its
//! comments, its native JSON, the images that JSON points at, and the
//! configured exports. A capture that fails anywhere is owed again next
//! run, whole. Nothing is pruned: a file that leaves the listing (trashed,
//! unshared, deleted) keeps every row and capture it had, since this is a
//! backup.
//!
//! After the captures, every stored revision of a listed Doc or Sheet is
//! owed its export in each `revision_exports` format, fetched once
//! through the revision's export link (`revision_exports`).
//!
//! Each kind drains on its own, because each kind's native API is signed
//! by its own latchkey service: a missing `google-sheets` sign-in ends
//! the Sheets and leaves the Docs and Slides to finish.

pub mod api;
pub mod db;
pub mod schema_raw;
pub mod shape;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl::download_problems::DownloadProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_google_drive_config::{ExportFormat, FileKind, GoogleDriveApi};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, BatchError, Fetcher, Listed, Loop, Outcome};

pub use db::{db_path_for, RawDb};
use db::{Capture, Part, RevisionExport, Split};
use schema_raw::*;

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    pub latchkey: LatchkeySettings,
    pub api: GoogleDriveApi,
    pub progress: Progress,
    pub control: DownloadControl,
    pub sealer: Option<Sealer>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct FetchSummary {
    pub listed: usize,
    pub owed: usize,
    pub captured: usize,
    pub failed: usize,
    /// Owed files a stop or a give-up left for the next run.
    pub left: usize,
    pub exports: usize,
    pub exports_declined: usize,
    pub images: usize,
    /// Captured files whose two revision listings named different
    /// revisions.
    pub revision_listings_disagreed: usize,
    /// Old revisions' exports fetched this run.
    pub revision_exports: usize,
    /// Revisions Drive no longer has the content of.
    pub revision_exports_gone: usize,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "listed={} owed={} captured={} failed={} left={} exports={} \
             exports_declined={} images={} revision_listings_disagreed={} \
             revision_exports={} revision_exports_gone={}",
            self.listed,
            self.owed,
            self.captured,
            self.failed,
            self.left,
            self.exports,
            self.exports_declined,
            self.images,
            self.revision_listings_disagreed,
            self.revision_exports,
            self.revision_exports_gone,
        )
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| walk(opts, found)).await
}

/// The run's own record in `sync_runs`: its config, its summary, and what
/// it changed in each table, so a run that fetched nothing says so.
async fn walk(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let run = DownloadRun::start(opts.db.pool(), &serde_json::to_value(&opts.api)?).await?;
    let mut summary = FetchSummary::default();
    let result = walk_files(&opts, &found, &mut summary).await;
    run.finish(&result, &summary).await;
    result.map(|()| summary)
}

async fn walk_files(
    opts: &FetchOptions,
    found: &RunProblems,
    summary: &mut FetchSummary,
) -> Result<()> {
    opts.progress.set_message("google_drive: listing files");
    let entries = list(opts, found).await?;
    summary.listed = entries.len();

    let mut by_kind: HashMap<FileKind, Vec<Value>> = HashMap::new();
    for entry in entries {
        let kind = entry
            .get("mimeType")
            .and_then(Value::as_str)
            .and_then(api::kind_of_mime);
        match kind {
            Some(kind) if opts.api.kinds.contains(&kind) => {
                by_kind.entry(kind).or_default().push(entry)
            }
            _ => {}
        }
    }

    let listed_ids: HashMap<FileKind, HashSet<String>> = by_kind
        .iter()
        .map(|(kind, entries)| {
            let ids = entries.iter().filter_map(|e| str_of(e, "id")).collect();
            (*kind, ids)
        })
        .collect();

    for kind in opts.api.kinds.iter().copied() {
        if opts.control.stop.requested() {
            break;
        }
        let entries = by_kind.remove(&kind).unwrap_or_default();
        let listed: Vec<Listed> = entries.iter().filter_map(listed_of).collect();
        let owed = owed::owed(opts.db.pool(), FILES, listed).await?;
        summary.owed += owed.len();
        if owed.is_empty() {
            continue;
        }
        let info = api::kind_info(kind);
        opts.progress.set_length(Some(owed.len() as u64));
        let captures = Captures {
            kind,
            exports: opts.api.exports.for_kind(kind),
            latchkey: &opts.latchkey,
            db: &opts.db,
            progress: &opts.progress,
            stop: &opts.control.stop,
            entries: entries
                .into_iter()
                .filter_map(|e| Some((str_of(&e, "id")?, e)))
                .collect(),
            exports_done: AtomicUsize::new(0),
            exports_declined: AtomicUsize::new(0),
            images: AtomicUsize::new(0),
            revision_listings_disagreed: AtomicUsize::new(0),
        };
        let drained = owed::drain(
            &Loop {
                pool: opts.db.pool(),
                table: FILES,
                phase: info.latchkey_service,
                stop: &opts.control.stop,
                found,
                sealer: opts.sealer.as_ref(),
                batch: 1,
                concurrency: 4,
                flush: 8,
                flush_bytes: 64 << 20,
                failures_in_a_row: 10,
            },
            owed,
            &captures,
        )
        .await?;
        summary.captured += drained.got;
        summary.failed += drained.failed;
        summary.left += drained.left;
        summary.exports += captures.exports_done.load(Ordering::Relaxed);
        summary.exports_declined += captures.exports_declined.load(Ordering::Relaxed);
        summary.images += captures.images.load(Ordering::Relaxed);
        summary.revision_listings_disagreed +=
            captures.revision_listings_disagreed.load(Ordering::Relaxed);
    }

    for kind in opts.api.kinds.iter().copied() {
        if opts.control.stop.requested() {
            break;
        }
        let empty = HashSet::new();
        let files = listed_ids.get(&kind).unwrap_or(&empty);
        export_revisions(opts, found, summary, kind, files).await?;
    }
    Ok(())
}

/// Fetch the configured exports of every stored revision of `kind`'s
/// listed files that the store does not hold yet. A file no longer
/// listed is left alone: its links would only answer that it is gone.
async fn export_revisions(
    opts: &FetchOptions,
    found: &RunProblems,
    summary: &mut FetchSummary,
    kind: FileKind,
    files: &HashSet<String>,
) -> Result<()> {
    // A deck's export link ignores `revision=` and always exports the
    // deck as it is now (measured in every format), so a deck's old
    // revisions have no copy to fetch.
    if kind == FileKind::Presentation {
        return Ok(());
    }
    let formats = opts.api.revision_exports.for_kind(kind);
    if formats.is_empty() || files.is_empty() {
        return Ok(());
    }
    let info = api::kind_info(kind);
    let mut wanted: HashMap<String, Wanted> = HashMap::new();
    let mut listed = Vec::new();
    for revision in opts.db.revisions_of_kind(info.mime_type).await? {
        if !files.contains(&revision.file_id) {
            continue;
        }
        for format in formats {
            let Some(url) = api::export_link(kind, *format, &revision.export_links) else {
                continue;
            };
            let (mime, ext) = api::export_type(kind, *format);
            let part = export_part(ext);
            let id = format!("{}#{part}", revision.id);
            listed.push(Listed::new(id.clone(), None::<String>));
            wanted.insert(
                id,
                Wanted {
                    revision_id: revision.id.clone(),
                    part,
                    url,
                    content_type: mime.to_string(),
                },
            );
        }
    }
    let owed = owed::owed(opts.db.pool(), REVISION_EXPORTS, listed).await?;
    if owed.is_empty() {
        return Ok(());
    }
    opts.progress
        .set_message(&format!("google_drive: {} old revisions", owed.len()));
    opts.progress.set_length(Some(owed.len() as u64));
    let fetcher = RevisionExports {
        latchkey: &opts.latchkey,
        db: &opts.db,
        progress: &opts.progress,
        stop: &opts.control.stop,
        wanted,
        gone: AtomicUsize::new(0),
    };
    let drained = owed::drain(
        &Loop {
            pool: opts.db.pool(),
            table: REVISION_EXPORTS,
            phase: "revision exports",
            stop: &opts.control.stop,
            found,
            sealer: opts.sealer.as_ref(),
            batch: 1,
            concurrency: 4,
            flush: 8,
            flush_bytes: 64 << 20,
            failures_in_a_row: 10,
        },
        owed,
        &fetcher,
    )
    .await?;
    let gone = fetcher.gone.load(Ordering::Relaxed);
    summary.revision_exports += drained.got - gone;
    summary.revision_exports_gone += gone;
    summary.failed += drained.failed;
    summary.left += drained.left;
    Ok(())
}

/// What an owed revision export is fetched from and stored as.
struct Wanted {
    revision_id: String,
    part: String,
    url: String,
    content_type: String,
}

struct RevisionExports<'a> {
    latchkey: &'a LatchkeySettings,
    db: &'a RawDb,
    progress: &'a Progress,
    stop: &'a datalib_etl::stop::StopFlag,
    wanted: HashMap<String, Wanted>,
    gone: AtomicUsize,
}

#[async_trait]
impl Fetcher<RevisionExport> for RevisionExports<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<owed::Fetched<RevisionExport>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let Some(w) = self.wanted.get(&listed.key) else {
                out.push(owed::Fetched {
                    listed,
                    outcome: Outcome::Failed("owed but not wanted".to_string()),
                });
                continue;
            };
            let export = |bytes| RevisionExport {
                id: listed.key.clone(),
                revision_id: w.revision_id.clone(),
                part: w.part.clone(),
                content_type: w.content_type.clone(),
                bytes,
            };
            let outcome = match api::get_revision_export(&w.url, self.latchkey).await {
                Ok(bytes) => Outcome::Got(export(Some(bytes))),
                Err(e) if api::is_not_found(&e) => {
                    self.gone.fetch_add(1, Ordering::Relaxed);
                    Outcome::Unusable(
                        export(None),
                        datalib_problems::Reason::NotFound,
                        "Drive no longer has this revision's content".to_string(),
                    )
                }
                Err(e) if api::is_service(&e) => return Err(BatchError::Terminal(e)),
                Err(e) if self.stop.requested() => return Err(BatchError::Batch(e)),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            self.progress.inc(1);
            out.push(owed::Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[owed::Fetched<RevisionExport>],
    ) -> Result<()> {
        let exports: Vec<&RevisionExport> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(e) | Outcome::Unusable(e, ..) => Some(e),
                _ => None,
            })
            .collect();
        db::store_revision_exports(tx, self.db.cas(), &exports).await
    }

    fn weight(&self, content: &RevisionExport) -> usize {
        content.bytes.as_ref().map_or(0, Vec::len)
    }
}

/// Every file the config asks for, as Drive's listing entry. A listing
/// cut short is a problem, not a failure: nothing is pruned to it, so a
/// partial listing only captures less this run.
async fn list(opts: &FetchOptions, found: &RunProblems) -> Result<Vec<Value>> {
    let lk = &opts.latchkey;
    let named = opts.api.wanted_file_ids();
    if !named.is_empty() {
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for id in named {
            match api::get_json(&api::file_url(&id), lk).await {
                Ok(entry) => out.push(entry),
                Err(e) if api::is_service(&e) => return Err(e),
                Err(e) if api::is_not_found(&e) => missing.push(DownloadProblem::not_found(
                    "file_ids",
                    &id,
                    format!("{e:#}"),
                )),
                Err(e) => missing.push(DownloadProblem::forbidden(
                    "file_ids",
                    &id,
                    format!("{e:#}"),
                )),
            }
        }
        found.config(missing);
        return Ok(out);
    }

    let mut out = Vec::new();
    let mut page: Option<String> = None;
    loop {
        if opts.control.stop.requested() {
            break;
        }
        let url = api::list_url(&opts.api.kinds, opts.api.owned_only, page.as_deref());
        let v = match api::get_json(&url, lk).await {
            Ok(v) => v,
            Err(e) if api::is_service(&e) => return Err(e),
            Err(e) => {
                found.listing("files", format!("{e:#}"));
                break;
            }
        };
        if v.get("incompleteSearch").and_then(Value::as_bool) == Some(true) {
            found.listing(
                "files",
                "Drive said this listing is incomplete; files it left out are not captured",
            );
        }
        let (files, next) = api::page_items(&v, "files").context("list files")?;
        out.extend(files);
        match next {
            Some(token) => page = Some(token),
            None => break,
        }
    }
    Ok(out)
}

fn listed_of(entry: &Value) -> Option<Listed> {
    Some(Listed::new(
        str_of(entry, "id")?,
        Some(str_of(entry, "version")?),
    ))
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The fetcher for one kind's owed files.
struct Captures<'a> {
    kind: FileKind,
    exports: &'a [ExportFormat],
    latchkey: &'a LatchkeySettings,
    db: &'a RawDb,
    progress: &'a Progress,
    stop: &'a datalib_etl::stop::StopFlag,
    entries: HashMap<String, Value>,
    exports_done: AtomicUsize,
    exports_declined: AtomicUsize,
    images: AtomicUsize,
    revision_listings_disagreed: AtomicUsize,
}

#[async_trait]
impl Fetcher<Capture> for Captures<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<owed::Fetched<Capture>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let outcome = match self.capture(&listed).await {
                Ok(outcome) => outcome,
                Err(e) if api::is_service(&e) => return Err(BatchError::Terminal(e)),
                Err(e) if self.stop.requested() => return Err(BatchError::Batch(e)),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            self.progress.inc(1);
            out.push(owed::Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[owed::Fetched<Capture>],
    ) -> Result<()> {
        let captures: Vec<&Capture> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(c) | Outcome::Unusable(c, ..) => Some(c),
                _ => None,
            })
            .collect();
        db::store_captures(tx, self.db.cas(), &captures).await
    }

    fn weight(&self, content: &Capture) -> usize {
        content.bytes()
    }
}

impl Captures<'_> {
    async fn capture(&self, listed: &Listed) -> Result<Outcome<Capture>> {
        let id = listed.key.as_str();
        let version = listed.version.as_deref().unwrap_or_default();
        let entry = self
            .entries
            .get(id)
            .with_context(|| format!("file {id} was owed but not listed"))?;
        self.progress
            .set_message(&format!("google_drive: file {id}"));
        let lk = self.latchkey;

        let can = |capability: &str| {
            entry
                .pointer(&format!("/capabilities/{capability}"))
                .and_then(Value::as_bool)
                != Some(false)
        };
        let can_read_revisions = can("canReadRevisions");
        // Drive's first listing after a quiet spell can leave out a
        // file's oldest revision, and one made right after has it
        // (docs/dev/plans/google_drive.md), so a capture lists twice and
        // keeps both.
        let revisions = if can_read_revisions {
            let first = paged(|token| api::revisions_url(id, token), "revisions", lk).await?;
            let second = paged(|token| api::revisions_url(id, token), "revisions", lk).await?;
            let (revisions, disagreed) = shape::union_of_listings(&first, &second);
            if !disagreed.is_empty() {
                self.revision_listings_disagreed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    file = id,
                    revisions = %disagreed.join(","),
                    "two listings of this file's revisions disagreed; keeping every revision either named"
                );
            }
            revisions
        } else {
            Vec::new()
        };
        let comments = paged(|token| api::comments_url(id, token), "comments", lk).await?;

        let native_bytes =
            api::get_bytes(&api::native_url(self.kind, id, can("canComment")), lk).await?;
        let mut native: Value = serde_json::from_slice(&native_bytes)
            .with_context(|| format!("parse the native JSON of {id}"))?;
        let links = shape::take_image_links(&mut native);
        let mut parts = vec![Part {
            name: PART_NATIVE.to_string(),
            bytes: Some(shape::gzip(&serde_json::to_vec(&native)?)?),
            content_type: Some(NATIVE_CONTENT_TYPE.to_string()),
        }];

        let mut by_url: HashMap<&str, Vec<u8>> = HashMap::new();
        for (pointer, url) in &links {
            if !by_url.contains_key(url.as_str()) {
                let bytes = api::get_image(url)
                    .await
                    .with_context(|| format!("image at {pointer}"))?;
                self.images.fetch_add(1, Ordering::Relaxed);
                by_url.insert(url, bytes);
            }
            parts.push(Part {
                name: image_part(pointer),
                bytes: by_url.get(url.as_str()).cloned(),
                content_type: None,
            });
        }

        let mut declined = Vec::new();
        for format in self.exports {
            let (mime, ext) = api::export_type(self.kind, *format);
            let bytes = match api::get_bytes(&api::export_url(id, mime), lk).await {
                Ok(bytes) => {
                    self.exports_done.fetch_add(1, Ordering::Relaxed);
                    Some(bytes)
                }
                Err(e) if api::is_export_too_large(&e) => {
                    self.exports_declined.fetch_add(1, Ordering::Relaxed);
                    declined.push(ext);
                    None
                }
                Err(e) => return Err(e),
            };
            parts.push(Part {
                name: export_part(ext),
                bytes,
                content_type: Some(mime.to_string()),
            });
        }

        let capture = Capture {
            file: split(
                id.to_string(),
                &shape::order_file_entry(entry),
                FILE_VOLATILE_PATHS,
                |p| FileRow { id_and_payload: p },
            )?,
            row: Split {
                row: CaptureRow {
                    id: CaptureRow::id_for(id, version),
                    file_id: id.to_string(),
                    version: version.to_string(),
                    modified_time: str_of(entry, "modifiedTime"),
                    native_revision_id: shape::native_revision_id(&native),
                    head_revision_id: revisions.last().and_then(|r| str_of(r, "id")),
                },
                volatile: shape::image_links_volatile(&links),
            },
            revisions: children(id, &revisions, REVISION_VOLATILE_PATHS, |p| RevisionRow {
                id_and_payload: p,
                file_id: id.to_string(),
            })?,
            comments: children(id, &comments, COMMENT_VOLATILE_PATHS, |p| CommentRow {
                id_and_payload: p,
                file_id: id.to_string(),
            })?,
            parts,
        };
        Ok(if declined.is_empty() {
            Outcome::Got(capture)
        } else {
            Outcome::Unusable(
                capture,
                datalib_problems::Reason::FetchFailed,
                format!(
                    "Google declined to export this version as {} (over its 10 MB export \
                     limit); the native JSON and the other parts are kept",
                    declined.join(", ")
                ),
            )
        })
    }
}

/// Every item of a paged Drive list.
async fn paged(
    url: impl Fn(Option<&str>) -> String,
    key: &str,
    lk: &LatchkeySettings,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let v = api::get_json(&url(page.as_deref()), lk).await?;
        let (items, next) = api::page_items(&v, key).with_context(|| format!("list {key}"))?;
        out.extend(items);
        match next {
            Some(token) => page = Some(token),
            None => return Ok(out),
        }
    }
}

fn split<R>(
    id: String,
    payload: &Value,
    volatile: &[dr::VolatilePath],
    row: impl Fn(WirePayload) -> R,
) -> Result<Split<R>> {
    let (base, volatile) = dr::split_volatile(payload, volatile);
    Ok(Split {
        row: row(WirePayload {
            id,
            payload: serde_json::to_string(&base)?,
        }),
        volatile,
    })
}

/// A file's revisions or comments as rows keyed `{file}#{their id}`.
/// An item without an id cannot be keyed, and is left out.
fn children<R>(
    file_id: &str,
    items: &[Value],
    volatile: &[dr::VolatilePath],
    row: impl Fn(WirePayload) -> R,
) -> Result<Vec<Split<R>>> {
    items
        .iter()
        .filter_map(|item| Some((str_of(item, "id")?, item)))
        .map(|(child, item)| split(format!("{file_id}#{child}"), item, volatile, &row))
        .collect()
}
